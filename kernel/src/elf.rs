//! A minimal static ELF64 loader.
//!
//! Enough to load a position-dependent, statically linked executable and no
//! more: no dynamic linking, no relocations, no interpreter. v1's programs are
//! boot modules built by the same repository, so anything else would be
//! machinery for a case that cannot arise.

pub const PT_LOAD: u32 = 1;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    TooSmall,
    NotAnElf,
    NotX86_64,
    NotExecutable,
    BadProgramHeaders,
    NoLoadableSegments,
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub vaddr: u64,
    pub offset: u64,
    pub file_size: u64,
    pub mem_size: u64,
    pub flags: u32,
}

pub struct Image<'a> {
    bytes: &'a [u8],
    pub entry: u64,
    ph_offset: u64,
    ph_entry_size: u16,
    ph_count: u16,
}

fn read_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn read_u64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

impl<'a> Image<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Image<'a>, ElfError> {
        if bytes.len() < 64 {
            return Err(ElfError::TooSmall);
        }
        if &bytes[0..4] != b"\x7fELF" {
            return Err(ElfError::NotAnElf);
        }
        if bytes[4] != 2 || bytes[5] != 1 {
            return Err(ElfError::NotX86_64); // 64-bit, little endian
        }
        if read_u16(bytes, 18) != 0x3E {
            return Err(ElfError::NotX86_64);
        }
        // ET_EXEC only. A position-independent ET_DYN would need relocations,
        // and v1 has nothing to relocate with.
        if read_u16(bytes, 16) != 2 {
            return Err(ElfError::NotExecutable);
        }
        let entry = read_u64(bytes, 24);
        let ph_offset = read_u64(bytes, 32);
        let ph_entry_size = read_u16(bytes, 54);
        let ph_count = read_u16(bytes, 56);
        if ph_entry_size < 56
            || ph_offset as usize + ph_count as usize * ph_entry_size as usize > bytes.len()
        {
            return Err(ElfError::BadProgramHeaders);
        }
        Ok(Image { bytes, entry, ph_offset, ph_entry_size, ph_count })
    }

    /// The loadable segments, in file order.
    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        (0..self.ph_count as usize).filter_map(move |i| {
            let at = self.ph_offset as usize + i * self.ph_entry_size as usize;
            let b = self.bytes;
            if read_u32(b, at) != PT_LOAD {
                return None;
            }
            let seg = Segment {
                flags: read_u32(b, at + 4),
                offset: read_u64(b, at + 8),
                vaddr: read_u64(b, at + 16),
                file_size: read_u64(b, at + 32),
                mem_size: read_u64(b, at + 40),
            };
            if seg.file_size > seg.mem_size {
                return None;
            }
            if seg.offset + seg.file_size > b.len() as u64 {
                return None;
            }
            Some(seg)
        })
    }

    /// The page-aligned span every loadable segment falls inside.
    pub fn span(&self) -> Result<(u64, u64), ElfError> {
        const PAGE: u64 = 4096;
        let mut lo = u64::MAX;
        let mut hi = 0u64;
        for s in self.segments() {
            lo = lo.min(s.vaddr & !(PAGE - 1));
            hi = hi.max((s.vaddr + s.mem_size).div_ceil(PAGE) * PAGE);
        }
        if lo == u64::MAX {
            return Err(ElfError::NoLoadableSegments);
        }
        Ok((lo, hi))
    }

    /// The bytes of one segment as they appear in the file.
    pub fn segment_data(&self, seg: &Segment) -> &'a [u8] {
        let start = seg.offset as usize;
        &self.bytes[start..start + seg.file_size as usize]
    }
}
