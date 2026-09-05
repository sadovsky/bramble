//! The physical frame allocator: a bitmap, deliberately outside the graph.
//!
//! DESIGN 4.4 lists this in the non-graph register with the reason. A set of
//! free frames is not a relationship, and modelling one node per frame would
//! make the graph a million nodes that say nothing. Nothing has a relationship
//! with a free frame; the moment a frame is handed out and outlives the call,
//! a `MemoryObject` node describes it.

use limine::memory_map::{Entry, EntryType};

pub const FRAME_SIZE: usize = 4096;

pub struct FrameAllocator {
    /// One bit per frame; 1 means used. Lives in memory carved out of the
    /// first usable region big enough to hold it.
    bitmap: &'static mut [u64],
    total_frames: usize,
    free_frames: usize,
    /// Where to resume scanning, so allocation is not quadratic in practice.
    hint: usize,
    bitmap_phys: u64,
    bitmap_pages: usize,
}

/// Where the allocator put its own bookkeeping, so the caller can describe it
/// with a `MemoryObject` like every other piece of live memory.
#[derive(Clone, Copy, Debug)]
pub struct BitmapRegion {
    pub phys: u64,
    pub pages: u32,
}

impl FrameAllocator {
    /// Build the allocator from Limine's memory map.
    ///
    /// # Safety
    /// `hhdm` must be the bootloader's higher-half direct map offset, and the
    /// memory map must describe the machine this is running on.
    pub unsafe fn new(entries: &[&Entry], hhdm: u64) -> FrameAllocator {
        let mut highest = 0u64;
        for e in entries {
            if e.entry_type == EntryType::USABLE {
                highest = highest.max(e.base + e.length);
            }
        }
        let total_frames = (highest as usize).div_ceil(FRAME_SIZE);
        let bitmap_bytes = total_frames.div_ceil(8);
        let bitmap_pages = bitmap_bytes.div_ceil(FRAME_SIZE);
        let bitmap_len = bitmap_pages * FRAME_SIZE;

        // Carve the bitmap out of the first usable region large enough. It is
        // the one allocation that cannot come from the allocator itself.
        let mut bitmap_phys = 0u64;
        for e in entries {
            if e.entry_type == EntryType::USABLE && e.length as usize >= bitmap_len {
                bitmap_phys = e.base;
                break;
            }
        }
        assert!(bitmap_phys != 0, "no usable region large enough for the frame bitmap");

        let words = bitmap_len / 8;
        // SAFETY: the region is usable RAM per the memory map, mapped by the
        // HHDM, page aligned, and claimed here exclusively for the bitmap.
        let bitmap: &'static mut [u64] =
            unsafe { core::slice::from_raw_parts_mut((hhdm + bitmap_phys) as *mut u64, words) };

        // Start with everything used, then release what the map says is usable.
        bitmap.fill(u64::MAX);
        let mut alloc = FrameAllocator {
            bitmap,
            total_frames,
            free_frames: 0,
            hint: 1,
            bitmap_phys,
            bitmap_pages,
        };
        for e in entries {
            if e.entry_type != EntryType::USABLE {
                continue;
            }
            let first = e.base as usize / FRAME_SIZE;
            let count = e.length as usize / FRAME_SIZE;
            for f in first..first + count {
                if f < total_frames {
                    alloc.release(f);
                }
            }
        }
        // Frame zero stays used as a null guard, and the bitmap owns its own.
        alloc.claim(0);
        let first = bitmap_phys as usize / FRAME_SIZE;
        for f in first..first + bitmap_pages {
            alloc.claim(f);
        }
        alloc
    }

    #[inline]
    fn claim(&mut self, frame: usize) {
        let (w, b) = (frame / 64, frame % 64);
        if self.bitmap[w] & (1 << b) == 0 {
            self.bitmap[w] |= 1 << b;
            self.free_frames -= 1;
        }
    }

    #[inline]
    fn release(&mut self, frame: usize) {
        let (w, b) = (frame / 64, frame % 64);
        if self.bitmap[w] & (1 << b) != 0 {
            self.bitmap[w] &= !(1 << b);
            self.free_frames += 1;
        }
    }

    #[inline]
    fn is_free(&self, frame: usize) -> bool {
        self.bitmap[frame / 64] & (1 << (frame % 64)) == 0
    }

    pub fn bitmap_region(&self) -> BitmapRegion {
        BitmapRegion { phys: self.bitmap_phys, pages: self.bitmap_pages as u32 }
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames
    }
    pub fn free_frames(&self) -> usize {
        self.free_frames
    }
    pub fn used_frames(&self) -> usize {
        self.total_frames - self.free_frames
    }

    /// Allocate one frame. Returns its physical address.
    pub fn alloc(&mut self) -> Option<u64> {
        self.alloc_contiguous(1)
    }

    /// Allocate `count` physically contiguous frames, which is what a
    /// `MemoryObject` describes in v1.
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<u64> {
        if count == 0 {
            return None;
        }
        // Two sweeps: from the hint, then from the start, so a wrapped scan
        // still finds space without scanning twice in the common case.
        for start in [self.hint, 1] {
            let mut f = start;
            while f + count <= self.total_frames {
                // Skip whole words that are fully used.
                if count == 1 && f % 64 == 0 && self.bitmap[f / 64] == u64::MAX {
                    f += 64;
                    continue;
                }
                let mut ok = true;
                for k in 0..count {
                    if !self.is_free(f + k) {
                        f += k + 1;
                        ok = false;
                        break;
                    }
                }
                if ok {
                    for k in 0..count {
                        self.claim(f + k);
                    }
                    self.hint = f + count;
                    return Some((f * FRAME_SIZE) as u64);
                }
            }
        }
        None
    }

    /// Return frames to the pool. Called by the reaper when a `MemoryObject`
    /// that owns real RAM is reclaimed.
    pub fn free_contiguous(&mut self, phys: u64, count: usize) {
        let first = phys as usize / FRAME_SIZE;
        for f in first..first + count {
            if f < self.total_frames {
                self.release(f);
            }
        }
        self.hint = self.hint.min(first.max(1));
    }
}
