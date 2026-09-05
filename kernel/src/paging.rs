//! Raw x86_64 page-table manipulation.
//!
//! This file exists to serve invariant I5: *page tables are a cache of `Maps`
//! edges*. Nothing here decides policy. It writes what it is told to write and,
//! critically, it can walk back what is actually there so the checker can diff
//! hardware against the graph.
//!
//! Physical memory is reached through the bootloader's higher-half direct map,
//! so no temporary mappings and no recursive page-table trick are needed.

use bramble_graph::edge::Prot;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::tlb;
use x86_64::registers::control::Cr3;
use x86_64::VirtAddr;

use crate::frames::{FrameAllocator, FRAME_SIZE};

pub const PAGE_SIZE: u64 = 4096;
const ENTRIES: usize = 512;

// Page-table entry bits.
pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_WRITABLE: u64 = 1 << 1;
pub const PTE_USER: u64 = 1 << 2;
pub const PTE_HUGE: u64 = 1 << 7;
pub const PTE_NO_EXEC: u64 = 1 << 63;
const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// The bootloader's direct map offset. Set once at boot, read everywhere.
static HHDM: AtomicU64 = AtomicU64::new(0);

/// The kernel's own page-table root: the address space to run in when the
/// current thread has none of its own.
///
/// Cached outside the graph so the context switch does not need a lookup. It is
/// set once at boot and never changes, which is invariant I5' restated.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

pub fn set_hhdm(offset: u64) {
    HHDM.store(offset, Ordering::Release);
}

pub fn set_kernel_pml4(phys: u64) {
    KERNEL_PML4.store(phys, Ordering::Release);
}

#[inline]
pub fn kernel_pml4() -> u64 {
    KERNEL_PML4.load(Ordering::Acquire)
}

#[inline]
pub fn hhdm() -> u64 {
    HHDM.load(Ordering::Acquire)
}

/// A physical address, viewed through the direct map.
///
/// # Safety
/// `phys` must be inside physical memory the bootloader direct-mapped.
#[inline]
unsafe fn phys_table(phys: u64) -> *mut u64 {
    (hhdm() + (phys & PTE_ADDR_MASK)) as *mut u64
}

#[inline]
fn index(vaddr: u64, level: u32) -> usize {
    ((vaddr >> (12 + 9 * level)) & 0x1FF) as usize
}

/// Translate `Prot` into hardware bits. `Prot` is what the graph records; these
/// bits are the cache of it.
pub fn prot_to_flags(prot: Prot) -> u64 {
    let mut f = PTE_PRESENT;
    if prot.contains(Prot::WRITE) {
        f |= PTE_WRITABLE;
    }
    if prot.contains(Prot::USER) {
        f |= PTE_USER;
    }
    if !prot.contains(Prot::EXEC) {
        f |= PTE_NO_EXEC;
    }
    f
}

/// True if the hardware entry grants everything `prot` asks for and nothing
/// it forbids. This is the comparison the I5 checker makes.
pub fn flags_match(entry: u64, prot: Prot) -> bool {
    if entry & PTE_PRESENT == 0 {
        return false;
    }
    let want_write = prot.contains(Prot::WRITE);
    let want_user = prot.contains(Prot::USER);
    let want_exec = prot.contains(Prot::EXEC);
    (entry & PTE_WRITABLE != 0) == want_write
        && (entry & PTE_USER != 0) == want_user
        && (entry & PTE_NO_EXEC == 0) == want_exec
}

/// The page-table root the cpu is currently using.
pub fn active_pml4() -> u64 {
    Cr3::read().0.start_address().as_u64()
}

/// Switch page tables.
///
/// # Safety
/// `pml4` must be a valid page-table root whose higher half maps the kernel,
/// or the next instruction fetch faults.
pub unsafe fn load_pml4(pml4: u64) {
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) pml4, options(nostack, preserves_flags));
    }
}

/// Run `f` with a different address space installed, then restore.
///
/// Safe only because every address space shares the kernel's higher half, so
/// the code, stack and interrupt tables stay mapped across the switch.
///
/// # Safety
/// `pml4` must be a page-table root whose higher half maps the kernel.
pub unsafe fn with_space<R>(pml4: u64, f: impl FnOnce() -> R) -> R {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let old = active_pml4();
        unsafe { load_pml4(pml4) };
        let r = f();
        unsafe { load_pml4(old) };
        r
    })
}

/// Create a fresh address space: an empty user half, sharing the kernel's
/// higher half so that a switch to it does not unmap the code doing the switch.
pub fn new_address_space(fa: &mut FrameAllocator) -> Option<u64> {
    let phys = fa.alloc()?;
    // SAFETY: a freshly allocated frame, direct-mapped, owned by this call.
    unsafe {
        let table = phys_table(phys);
        core::ptr::write_bytes(table as *mut u8, 0, FRAME_SIZE);
        // Entries 256..512 are the kernel half. Sharing them, rather than
        // copying the tables beneath, is what makes the kernel mapping one
        // fixed thing in every address space (invariant I5').
        let kernel = phys_table(active_pml4());
        for i in ENTRIES / 2..ENTRIES {
            table.add(i).write(kernel.add(i).read());
        }
    }
    Some(phys)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    OutOfFrames,
    /// Something is already mapped there. The caller broke invariant I7 before
    /// getting here, so this should be unreachable.
    AlreadyMapped(u64),
}

/// Walk to the leaf entry for `vaddr`, creating intermediate tables if asked.
///
/// # Safety
/// `pml4` must be a valid page-table root.
unsafe fn leaf_entry(
    pml4: u64,
    vaddr: u64,
    create: Option<&mut FrameAllocator>,
) -> Option<*mut u64> {
    unsafe {
        let mut table = phys_table(pml4);
        let mut fa = create;
        for level in (1..=3).rev() {
            let e = table.add(index(vaddr, level));
            let mut entry = e.read();
            if entry & PTE_PRESENT == 0 {
                let fa = fa.as_deref_mut()?;
                let frame = fa.alloc()?;
                core::ptr::write_bytes(phys_table(frame) as *mut u8, 0, FRAME_SIZE);
                // Intermediate entries are permissive; the leaf decides.
                entry = frame | PTE_PRESENT | PTE_WRITABLE | PTE_USER;
                e.write(entry);
            } else if entry & PTE_HUGE != 0 {
                // The kernel half uses large pages. Nothing maps 4 KiB pages
                // into a range already covered by one.
                return None;
            }
            table = phys_table(entry);
        }
        Some(table.add(index(vaddr, 0)))
    }
}

/// Map `pages` 4 KiB pages of `paddr` at `vaddr`.
///
/// This is called only by `vm::map`, which creates the `Maps` edge in the same
/// operation. Keeping the two together in one place is how I5 holds.
///
/// # Safety
/// `pml4` must be a valid page-table root not concurrently in use elsewhere.
pub unsafe fn map_pages(
    pml4: u64,
    vaddr: u64,
    paddr: u64,
    pages: u32,
    prot: Prot,
    fa: &mut FrameAllocator,
) -> Result<(), MapError> {
    let flags = prot_to_flags(prot);
    for i in 0..pages as u64 {
        let va = vaddr + i * PAGE_SIZE;
        // SAFETY: caller guarantees the root; `create` supplies fresh tables.
        let e = unsafe { leaf_entry(pml4, va, Some(fa)) }.ok_or(MapError::OutOfFrames)?;
        // SAFETY: `e` points into a page table reached through the direct map.
        let existing = unsafe { e.read() };
        if existing & PTE_PRESENT != 0 {
            return Err(MapError::AlreadyMapped(va));
        }
        unsafe { e.write((paddr + i * PAGE_SIZE) | flags) };
        tlb::flush(VirtAddr::new(va));
    }
    Ok(())
}

/// Remove `pages` pages starting at `vaddr` and flush them.
///
/// The flush is not optional and not deferred: a stale TLB entry is a mapping
/// the graph says does not exist, which is exactly the divergence I5 forbids.
///
/// # Safety
/// `pml4` must be a valid page-table root.
pub unsafe fn unmap_pages(pml4: u64, vaddr: u64, pages: u32) {
    for i in 0..pages as u64 {
        let va = vaddr + i * PAGE_SIZE;
        // SAFETY: caller guarantees the root; no tables are created.
        if let Some(e) = unsafe { leaf_entry(pml4, va, None) } {
            unsafe { e.write(0) };
        }
        tlb::flush(VirtAddr::new(va));
    }
}

/// What the hardware actually says about `vaddr`: its frame and its entry bits.
///
/// # Safety
/// `pml4` must be a valid page-table root.
pub unsafe fn translate(pml4: u64, vaddr: u64) -> Option<(u64, u64)> {
    // SAFETY: caller guarantees the root; no tables are created.
    let e = unsafe { leaf_entry(pml4, vaddr, None) }?;
    let entry = unsafe { e.read() };
    if entry & PTE_PRESENT == 0 {
        return None;
    }
    Some((entry & PTE_ADDR_MASK, entry))
}

/// Overwrite a leaf entry directly. The only caller is the phase 3 milestone,
/// which corrupts a mapping on purpose to prove the checker notices.
///
/// # Safety
/// `pml4` must be valid. This deliberately breaks invariant I5.
pub unsafe fn poke_entry(pml4: u64, vaddr: u64, value: u64) -> Option<u64> {
    // SAFETY: caller guarantees the root.
    let e = unsafe { leaf_entry(pml4, vaddr, None) }?;
    let old = unsafe { e.read() };
    unsafe { e.write(value) };
    tlb::flush(VirtAddr::new(vaddr));
    Some(old)
}

/// Visit every present 4 KiB page in the *user* half. Returning false stops the
/// walk. This is the half of the I5 check that looks for mappings the graph
/// does not know about.
///
/// # Safety
/// `pml4` must be a valid page-table root.
pub unsafe fn walk_user_pages(pml4: u64, f: &mut impl FnMut(u64, u64) -> bool) -> bool {
    unsafe {
        let l4 = phys_table(pml4);
        for i4 in 0..ENTRIES / 2 {
            let e4 = l4.add(i4).read();
            if e4 & PTE_PRESENT == 0 {
                continue;
            }
            let l3 = phys_table(e4);
            for i3 in 0..ENTRIES {
                let e3 = l3.add(i3).read();
                if e3 & PTE_PRESENT == 0 || e3 & PTE_HUGE != 0 {
                    continue;
                }
                let l2 = phys_table(e3);
                for i2 in 0..ENTRIES {
                    let e2 = l2.add(i2).read();
                    if e2 & PTE_PRESENT == 0 || e2 & PTE_HUGE != 0 {
                        continue;
                    }
                    let l1 = phys_table(e2);
                    for i1 in 0..ENTRIES {
                        let e1 = l1.add(i1).read();
                        if e1 & PTE_PRESENT == 0 {
                            continue;
                        }
                        let vaddr = ((i4 as u64) << 39)
                            | ((i3 as u64) << 30)
                            | ((i2 as u64) << 21)
                            | ((i1 as u64) << 12);
                        if !f(vaddr, e1) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }
}

/// Return every frame the user half of this address space uses for page tables,
/// then the root itself. Called when an `AddressSpace` node is reaped.
///
/// # Safety
/// `pml4` must be a valid page-table root that nothing is running on.
pub unsafe fn free_user_tables(pml4: u64, fa: &mut FrameAllocator) {
    unsafe {
        let l4 = phys_table(pml4);
        for i4 in 0..ENTRIES / 2 {
            let e4 = l4.add(i4).read();
            if e4 & PTE_PRESENT == 0 {
                continue;
            }
            let l3 = phys_table(e4);
            for i3 in 0..ENTRIES {
                let e3 = l3.add(i3).read();
                if e3 & PTE_PRESENT == 0 || e3 & PTE_HUGE != 0 {
                    continue;
                }
                let l2 = phys_table(e3);
                for i2 in 0..ENTRIES {
                    let e2 = l2.add(i2).read();
                    if e2 & PTE_PRESENT != 0 && e2 & PTE_HUGE == 0 {
                        fa.free_contiguous(e2 & PTE_ADDR_MASK, 1);
                    }
                }
                fa.free_contiguous(e3 & PTE_ADDR_MASK, 1);
            }
            fa.free_contiguous(e4 & PTE_ADDR_MASK, 1);
        }
        fa.free_contiguous(pml4, 1);
    }
}
