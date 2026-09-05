//! Turning an ELF image into a process.
//!
//! Every resource the program will use becomes a node owned, directly or
//! indirectly, by its `Process`. That is not tidiness: it is the entire
//! lifetime story. Killing the process detaches one `Owns` edge, and the
//! address space, its page tables, its memory and its threads all follow,
//! because ownership means "dies with" and nothing else has to remember.

use bramble_abi as abi;
use bramble_graph::body::*;
use bramble_graph::edge::{MapsAttr, Prot};
use bramble_graph::graph::Ref;

use crate::elf::{self, PF_R, PF_W, PF_X};
use crate::paging::{self, PAGE_SIZE};
use crate::sched;
use crate::state::GRAPH;
use crate::vm::{self, VmError};

/// Where a user stack lives. High in the user half, far from any program text.
pub const USER_STACK_BASE: u64 = 0x0000_7fff_0000_0000;
pub const USER_STACK_PAGES: u32 = 4;

#[derive(Clone, Copy, Debug)]
pub enum SpawnError {
    Elf(elf::ElfError),
    Vm(VmError),
    Graph(bramble_graph::graph::GraphError),
    ImageTooLarge,
    TooManyRuns,
    SegmentOutsideImage,
}

/// Why the graph refused. Worth naming: an arena filling up and an invariant
/// being violated are very different problems.
fn graph_reason(e: bramble_graph::graph::GraphError) -> &'static str {
    use bramble_graph::graph::GraphError as E;
    match e {
        E::ArenaFull(_) => "graph: a node arena is full",
        E::EdgeArenaFull => "graph: the edge arena is full",
        E::NoFreeSlot => "graph: the handle table is full",
        E::RangeOverlap => "graph: two mappings would cover the same page",
        E::Incompatible { .. } => "graph: that edge kind does not join those node kinds",
        E::Dying(_) => "graph: the owner is being destroyed",
        E::StaleNode(_) | E::StaleEdge(_) => "graph: stale reference",
        E::MissingRights { .. } => "graph: missing right",
        E::EmptySlot(_) => "graph: empty handle slot",
        E::NameTooLong => "graph: name does not fit inline",
        E::RootExists => "graph: a root already exists",
        E::BadState => "graph: illegal thread state transition",
    }
}

impl SpawnError {
    /// A short reason, so a failed spawn says what went wrong rather than
    /// printing a struct nobody reads.
    pub fn describe(&self) -> &'static str {
        match self {
            SpawnError::Elf(elf::ElfError::TooSmall) => "elf: file too small",
            SpawnError::Elf(elf::ElfError::NotAnElf) => "elf: not an elf",
            SpawnError::Elf(elf::ElfError::NotX86_64) => "elf: wrong architecture",
            SpawnError::Elf(elf::ElfError::NotExecutable) => "elf: not ET_EXEC",
            SpawnError::Elf(elf::ElfError::BadProgramHeaders) => "elf: bad program headers",
            SpawnError::Elf(elf::ElfError::NoLoadableSegments) => "elf: nothing to load",
            SpawnError::Vm(VmError::Graph(e)) => graph_reason(*e),
            SpawnError::Vm(VmError::Paging(_)) => "vm: out of frames",
            SpawnError::Vm(VmError::OutOfBounds) => "vm: range outside the object",
            SpawnError::Vm(VmError::NoAllocator) => "vm: no frame allocator",
            SpawnError::Vm(VmError::StaleSpace) => "vm: address space vanished",
            SpawnError::Graph(e) => graph_reason(*e),
            SpawnError::ImageTooLarge => "image is larger than the loader accepts",
            SpawnError::TooManyRuns => "image has too many separately-protected runs",
            SpawnError::SegmentOutsideImage => "a segment falls outside the mapped image",
        }
    }
}

impl From<VmError> for SpawnError {
    fn from(e: VmError) -> Self {
        SpawnError::Vm(e)
    }
}
impl From<bramble_graph::graph::GraphError> for SpawnError {
    fn from(e: bramble_graph::graph::GraphError) -> Self {
        SpawnError::Graph(e)
    }
}

fn prot_of(flags: u32) -> Prot {
    let mut p = Prot::USER;
    if flags & PF_R != 0 {
        p = p.union(Prot::READ);
    }
    if flags & PF_W != 0 {
        p = p.union(Prot::WRITE);
    }
    if flags & PF_X != 0 {
        p = p.union(Prot::EXEC);
    }
    p
}

/// The largest image the loader will take, in pages.
const MAX_IMAGE_PAGES: usize = 2048;
/// The most separately-protected runs one image may have.
const MAX_RUNS: usize = 16;

#[derive(Clone, Copy)]
struct Run {
    vaddr: u64,
    pages: u32,
    phys: u64,
}

/// Map an image's segments, then copy their contents in.
///
/// Segments are laid out per *page*, not per segment, because a linker will
/// happily put the end of one segment and the start of the next in the same
/// page: lld synthesises `.got` after any script has had its say, so no linker
/// script can prevent it. Two `Maps` edges covering one virtual page would
/// violate invariant I7, and rightly so, since the hardware has only one set of
/// permission bits per page.
///
/// So a page's protection is the union of every segment that touches it, and
/// consecutive pages sharing a protection become one mapping. That does mean a
/// page holding both read-only and writable data ends up writable; a real
/// dynamic loader makes exactly the same trade, for exactly this reason.
fn load_segments(
    proc: Ref<Process>,
    space: Ref<AddressSpace>,
    elf: &elf::Image<'_>,
) -> Result<(), SpawnError> {
    let (lo, hi) = elf.span().map_err(SpawnError::Elf)?;
    let total_pages = ((hi - lo) / PAGE_SIZE) as usize;
    if total_pages > MAX_IMAGE_PAGES {
        return Err(SpawnError::ImageTooLarge);
    }

    // One protection byte per page: the union of everything covering it.
    let mut prot = [0u8; MAX_IMAGE_PAGES];
    for seg in elf.segments() {
        let first = ((seg.vaddr & !(PAGE_SIZE - 1)) - lo) / PAGE_SIZE;
        let last = ((seg.vaddr + seg.mem_size).div_ceil(PAGE_SIZE) * PAGE_SIZE - lo) / PAGE_SIZE;
        for p in first..last {
            prot[p as usize] |= prot_of(seg.flags).0;
        }
    }

    // Consecutive pages with the same protection become one mapping.
    let mut runs = [Run { vaddr: 0, pages: 0, phys: 0 }; MAX_RUNS];
    let mut run_count = 0usize;
    let mut i = 0usize;
    while i < total_pages {
        if prot[i] == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < total_pages && prot[i] == prot[start] {
            i += 1;
        }
        if run_count == MAX_RUNS {
            return Err(SpawnError::TooManyRuns);
        }
        let pages = (i - start) as u32;
        let obj = vm::alloc_object_for(proc, pages)?;
        let phys = {
            let g = GRAPH.lock();
            g.body(obj).ok_or(VmError::StaleSpace)?.phys_base
        };
        // SAFETY: frames just allocated for this object, reachable only here.
        unsafe {
            core::ptr::write_bytes(
                (paging::hhdm() + phys) as *mut u8,
                0,
                pages as usize * PAGE_SIZE as usize,
            )
        };
        let vaddr = lo + start as u64 * PAGE_SIZE;
        vm::map(
            space,
            obj,
            MapsAttr { vaddr, len_pages: pages, off_pages: 0, prot: Prot(prot[start]) },
        )?;
        runs[run_count] = Run { vaddr, pages, phys };
        run_count += 1;
    }

    // Now the contents, placed by address rather than by segment order.
    for seg in elf.segments() {
        let data = elf.segment_data(&seg);
        let mut written = 0usize;
        while written < data.len() {
            let at = seg.vaddr + written as u64;
            let run = runs[..run_count]
                .iter()
                .find(|r| at >= r.vaddr && at < r.vaddr + r.pages as u64 * PAGE_SIZE)
                .ok_or(SpawnError::SegmentOutsideImage)?;
            let offset = at - run.vaddr;
            let room = run.pages as u64 * PAGE_SIZE - offset;
            let n = (data.len() - written).min(room as usize);
            // SAFETY: the destination is inside frames this image owns, at an
            // offset just bounds-checked against the run's length.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data.as_ptr().add(written),
                    (paging::hhdm() + run.phys + offset) as *mut u8,
                    n,
                );
            }
            written += n;
        }
    }
    Ok(())
}

/// Load an executable and make it runnable.
///
/// `grants` is the process's entire authority: the capabilities it starts with
/// and, since there is no other way to name anything, the only objects it can
/// ever reach.
pub fn spawn(
    root: Ref<Root>,
    image: &[u8],
    grants: &[(bramble_graph::id::NodeId, Rights)],
) -> Result<Ref<Process>, SpawnError> {
    let elf = elf::Image::parse(image).map_err(SpawnError::Elf)?;

    let proc = {
        let mut g = GRAPH.lock();
        g.create_under_root(root, Process::ZERO)?
    };
    let space = vm::create_space_for(proc)?;

    load_segments(proc, space, &elf)?;

    // The stack.
    let stack = vm::alloc_object_for(proc, USER_STACK_PAGES)?;
    vm::map(
        space,
        stack,
        MapsAttr {
            vaddr: USER_STACK_BASE,
            len_pages: USER_STACK_PAGES,
            off_pages: 0,
            prot: Prot::RWU,
        },
    )?;
    // Sixteen bytes of headroom, and the ABI's alignment at the entry point.
    let user_rsp = USER_STACK_BASE + USER_STACK_PAGES as u64 * PAGE_SIZE - 16;

    // The thread, and the kernel stack it will be entered on.
    let kstack_phys = {
        let mut fa = crate::state::FRAMES.lock();
        let fa = fa.as_mut().ok_or(VmError::NoAllocator)?;
        fa.alloc_contiguous(sched::KSTACK_PAGES as usize).ok_or(VmError::OutOfBounds)?
    };
    let kstack_top =
        paging::hhdm() + kstack_phys + sched::KSTACK_PAGES as u64 * PAGE_SIZE;
    // SAFETY: freshly allocated frames, direct-mapped, used by nothing else.
    let saved_rsp = unsafe { sched::init_stack(kstack_top, sched::enter_user) };
    // SAFETY: as above.
    unsafe { sched::plant_canary(kstack_top) };

    let mut g = GRAPH.lock();
    let pml4 = g.body(space).ok_or(VmError::StaleSpace)?.pml4_phys;
    let thread = g.create_under_process(
        proc,
        Thread {
            kstack_top,
            saved_rsp,
            user_entry: elf.entry,
            cr3: pml4,
            owner_proc: proc.id(),
            ..Thread::ZERO
        },
    )?;
    // The graph records the user stack pointer, so a snapshot shows where a
    // thread was without needing to read its registers.
    if let Some(b) = g.body_mut(thread) {
        b.msg.words[0] = user_rsp;
    }
    g.create_under_thread(
        thread,
        MemoryObject {
            phys_base: kstack_phys,
            pages: sched::KSTACK_PAGES,
            ..MemoryObject::ZERO
        },
    )?;
    g.link_in_space(thread, space)?;

    // Authority. Everything this program can ever do starts here.
    for (target, rights) in grants {
        g.grant_raw(proc, *target, *rights)?;
    }

    let cpu: Ref<Cpu> = g.typed(crate::state::cpu0()).ok_or(VmError::StaleSpace)?;
    g.make_ready(cpu, thread)?;
    let _ = abi::R_READ;
    Ok(proc)
}
