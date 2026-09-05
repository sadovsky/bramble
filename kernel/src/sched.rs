//! The scheduler: picking the next thread out of the graph, and switching to it.
//!
//! This is where the design's second claim gets tested. A run queue *is* a
//! graph: it is the `Ready` adjacency list of a `Cpu` node. What makes it fast
//! is not that it avoids the graph but that the operations are the head and
//! tail of one intrusive list, so "who runs next" is a single memory read and
//! "put this one at the back" is two splices. There is no traversal anywhere on
//! this path.

use core::sync::atomic::{AtomicBool, Ordering};

use bramble_graph::body::{Cpu, MemoryObject, NodeBody, Root, Thread, ThreadState};
use bramble_graph::graph::Ref;
use bramble_graph::id::NodeId;

use crate::state::{FRAMES, GRAPH};

static NEED_RESCHED: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn set_need_resched() {
    NEED_RESCHED.store(true, Ordering::Relaxed);
}

#[inline]
pub fn take_need_resched() -> bool {
    NEED_RESCHED.swap(false, Ordering::Relaxed)
}

/// Swap kernel stacks.
///
/// Saves the callee-saved registers and the flags on the outgoing stack,
/// records where they ended up, then adopts the incoming stack and unwinds it
/// the same way. Saving flags is what lets a thread that yielded with
/// interrupts enabled resume with them enabled, whoever switched to it.
///
/// # Safety
/// `save_rsp` must point at the outgoing thread's saved-stack slot, and
/// `new_rsp` must be a stack prepared by this function or by `init_stack`.
#[unsafe(naked)]
unsafe extern "C" fn switch_stack(save_rsp: *mut u64, new_rsp: u64) {
    core::arch::naked_asm!(
        "pushfq",
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "popfq",
        "ret",
    );
}

/// Mark the bottom of a kernel stack so an overflow can be detected.
///
/// # Safety
/// `stack_top` must be the top of `KSTACK_PAGES` frames of writable memory.
pub unsafe fn plant_canary(stack_top: u64) {
    let bottom = stack_top - KSTACK_PAGES as u64 * 4096;
    unsafe { (bottom as *mut u64).write(STACK_CANARY) };
}

/// Has any live thread's kernel stack been written past its bottom?
///
/// Kernel stacks live in the direct map, so there is no guard page to fault on;
/// without this an overflow silently corrupts unrelated physical memory. Four
/// pages was too few for the system-call path and the failure looked like a
/// hang with no output at all.
pub fn check_stack_canaries(g: &bramble_graph::graph::Graph) -> Result<(), NodeId> {
    for id in g.live_nodes() {
        if id.kind() != Some(bramble_graph::id::NodeKind::Thread) {
            continue;
        }
        let t: Ref<Thread> = match g.typed(id) {
            Some(t) => t,
            None => continue,
        };
        let top = match g.body(t) {
            Some(b) => b.kstack_top,
            None => continue,
        };
        if top == 0 {
            continue; // the boot context's stack is the bootloader's
        }
        let bottom = top - KSTACK_PAGES as u64 * 4096;
        // SAFETY: reading one word of a stack this kernel allocated.
        if unsafe { (bottom as *const u64).read() } != STACK_CANARY {
            return Err(id);
        }
    }
    Ok(())
}

/// Lay out a stack so that switching to it enters `entry` with interrupts on.
///
/// # Safety
/// `stack_top` must be the 16-byte-aligned top of at least 4 KiB of writable,
/// otherwise unused memory.
pub unsafe fn init_stack(stack_top: u64, entry: extern "C" fn() -> !) -> u64 {
    // Mirrors what `switch_stack` pops, in order, plus the address it returns
    // to. The extra slot at the top leaves the ABI's stack alignment correct at
    // the entry point.
    let saved_rsp = stack_top - 72;
    unsafe {
        let p = saved_rsp as *mut u64;
        for i in 0..6 {
            p.add(i).write(0); // r15, r14, r13, r12, rbx, rbp
        }
        p.add(6).write(0x202); // rflags: interrupts enabled, reserved bit set
        p.add(7).write(entry as usize as u64);
        p.add(8).write(0); // alignment filler
    }
    saved_rsp
}

/// What the graph says should happen next, decided under the lock.
struct Decision {
    save_slot: *mut u64,
    new_rsp: u64,
    /// Page-table root of the incoming thread, or zero for a kernel thread.
    cr3: u64,
    /// Kernel stack for entries from ring 3, or zero.
    kstack_top: u64,
    switching: bool,
}

/// Pick the next thread and switch to it.
///
/// Must be called with interrupts disabled. On one core that is what makes it
/// safe to let the raw pointer to the outgoing thread's saved stack outlive the
/// lock guard: nothing else can run, so nothing can move the arena underneath
/// it. When SMP arrives this becomes the lock hand-off described in DESIGN 3.8
/// rule 5, where the incoming thread releases the lock the outgoing one took.
pub fn schedule() {
    debug_assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "schedule() requires interrupts to be disabled"
    );

    let decision = {
        let mut g = GRAPH.lock();
        let cpu: Ref<Cpu> = match g.typed(crate::state::cpu0()) {
            Some(c) => c,
            None => return,
        };
        let current_id = match g.body(cpu) {
            Some(b) => b.current,
            None => return,
        };

        // The head of this cpu's Ready list, and the head advances. Both halves
        // of the decision in one lookup.
        let next = match g.pick_and_rotate(cpu) {
            Some(t) => t,
            None => return, // nothing else to run; carry on
        };
        if next.id() == current_id {
            return;
        }

        // The outgoing thread goes to the back of the queue if it is still
        // runnable. A thread that blocked or died has already left the list.
        let current: Option<Ref<Thread>> = g.typed(current_id);
        let save_slot = match current {
            Some(t) => {
                if g.body(t).map(|b| b.state) == Some(ThreadState::Running) {
                    let _ = g.make_ready(cpu, t);
                }
                match g.body_mut(t) {
                    Some(b) => &mut b.saved_rsp as *mut u64,
                    None => return,
                }
            }
            None => {
                // The outgoing context is gone; discard its stack pointer.
                static mut DISCARD: u64 = 0;
                &raw mut DISCARD
            }
        };

        let _ = g.make_running(cpu, next);
        let (new_rsp, cr3, kstack_top) = match g.body(next) {
            Some(b) => (b.saved_rsp, b.cr3, b.kstack_top),
            None => return,
        };
        Decision { save_slot, new_rsp, cr3, kstack_top, switching: true }
    };

    if !decision.switching {
        return;
    }

    // A thread that can reach ring 3 needs its own address space installed and
    // its kernel stack recorded, before it runs. Getting the second wrong means
    // an interrupt from user mode lands on someone else's stack.
    //
    // A thread with no address space of its own runs in the kernel's, and that
    // is not a detail. Leaving the previous process's tables loaded means the
    // kernel keeps running on an address space it is about to destroy: the
    // reaper frees those page-table frames, the allocator hands one straight
    // back for the next process's root, and zeroing it wipes the mappings out
    // from under the code doing the zeroing. The machine stops with no output.
    let target_cr3 = if decision.cr3 != 0 { decision.cr3 } else { paging::kernel_pml4() };
    if target_cr3 != 0 && target_cr3 != paging::active_pml4() {
        // SAFETY: the value came from an AddressSpace node this kernel built,
        // and every such space shares the kernel's higher half.
        unsafe { paging::load_pml4(target_cr3) };
    }
    if decision.kstack_top != 0 {
        crate::cpu::set_kernel_stack(decision.kstack_top);
    }

    // SAFETY: interrupts are off and this is the only core, so the arena
    // cannot move; both stack pointers were prepared by this module.
    unsafe { switch_stack(decision.save_slot, decision.new_rsp) };
}

/// End the calling process: it and everything it owns become unreachable, and
/// the cpu moves on. The reaper returns the storage later, from another thread,
/// which is why it is safe to do this while standing on a stack the process
/// owns.
/// How many processes have exited, so a watcher can tell without walking the
/// graph on every scheduling round.
pub static PROCESSES_EXITED: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

pub fn exit_current_process(code: i32) -> ! {
    x86_64::instructions::interrupts::disable();
    PROCESSES_EXITED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    {
        let mut g = GRAPH.lock();
        let me = current_locked(&g);
        let owner = g
            .typed::<Thread>(me)
            .and_then(|t| g.body(t))
            .map(|b| b.owner_proc)
            .unwrap_or(NodeId::NULL);
        if let Some(p) = g.typed::<bramble_graph::body::Process>(owner) {
            if let Some(b) = g.body_mut(p) {
                b.exit_code = code;
            }
            let _ = g.begin_delete(owner);
        }
        let _ = g.begin_delete(me);
        if let Some(cpu) = g.typed::<Cpu>(crate::state::cpu0()) {
            if let Some(b) = g.body_mut(cpu) {
                b.current = NodeId::NULL;
            }
        }
    }
    schedule();
    unreachable!("an exited process was scheduled again");
}

/// The running thread, read from a graph the caller already has locked.
/// The spin lock is not reentrant, so calling `current()` under it would hang.
pub fn current_locked(g: &bramble_graph::graph::Graph) -> NodeId {
    match g.typed::<Cpu>(crate::state::cpu0()) {
        Some(cpu) => g.body(cpu).map(|b| b.current).unwrap_or(NodeId::NULL),
        None => NodeId::NULL,
    }
}

/// Give up the rest of this thread's turn.
pub fn yield_now() {
    x86_64::instructions::interrupts::without_interrupts(schedule);
}

/// Called from the timer interrupt, after the end-of-interrupt is sent.
pub fn on_tick() {
    let mut g = GRAPH.lock();
    if let Some(root) = g.root() {
        if let Some(b) = g.body_mut(root) {
            b.ticks += 1;
        }
    }
    drop(g);
    set_need_resched();
}

/// The currently running thread, or null.
pub fn current() -> NodeId {
    let g = GRAPH.lock();
    match g.typed::<Cpu>(crate::state::cpu0()) {
        Some(cpu) => g.body(cpu).map(|b| b.current).unwrap_or(NodeId::NULL),
        None => NodeId::NULL,
    }
}

/// Stop running the calling thread for good and pick someone else.
pub fn exit_current() -> ! {
    x86_64::instructions::interrupts::disable();
    let me = current();
    {
        let mut g = GRAPH.lock();
        let _ = g.begin_delete(me);
        if let Some(cpu) = g.typed::<Cpu>(crate::state::cpu0()) {
            if let Some(b) = g.body_mut(cpu) {
                b.current = NodeId::NULL;
            }
        }
    }
    schedule();
    unreachable!("a deleted thread was scheduled again");
}

// ------------------------------------------------------------- creation ---

use crate::paging;
use crate::vm;

/// Kernel stack size per thread, in frames.
///
/// Four pages was not enough and failed silently. Kernel stacks sit inside the
/// direct map, so there is no unmapped guard page below them: an overflow walks
/// into whatever physical memory happens to be next and corrupts it. The
/// canary below turns that into a diagnosis instead of a mystery.
pub const KSTACK_PAGES: u32 = 8;

/// Written at the lowest word of every kernel stack and checked by the graph
/// consistency pass.
pub const STACK_CANARY: u64 = 0x00B2_AB1E_57AC_C0DE;

/// Create a kernel thread with its own stack and queue it to run.
///
/// The stack is a `MemoryObject` like any other memory: described by a node, so
/// the reaper knows to give the frames back when the thread dies.
pub fn spawn_kernel_thread(
    root: Ref<Root>,
    entry: extern "C" fn() -> !,
) -> Result<Ref<Thread>, vm::VmError> {
    let phys = {
        let mut fa = FRAMES.lock();
        let fa = fa.as_mut().ok_or(vm::VmError::NoAllocator)?;
        fa.alloc_contiguous(KSTACK_PAGES as usize).ok_or(vm::VmError::OutOfBounds)?
    };
    // Kernel stacks are reached through the direct map; they are never mapped
    // into a user address space.
    let stack_top = paging::hhdm() + phys + (KSTACK_PAGES as u64) * 4096;
    // SAFETY: freshly allocated frames, direct-mapped, used by nothing else.
    let saved_rsp = unsafe { init_stack(stack_top, entry) };
    // SAFETY: as above; the lowest word of a stack nothing has used yet.
    unsafe { plant_canary(stack_top) };

    let mut g = GRAPH.lock();
    let t = g.create_under_root(
        root,
        Thread { kstack_top: stack_top, saved_rsp, ..Thread::ZERO },
    )?;
    // The stack is owned by the thread, not by the root. Ownership is what
    // makes the reaper give the frames back when the thread dies; hanging the
    // stack off the root instead leaks it, which is what phase 4 first measured.
    g.create_under_thread(
        t,
        MemoryObject { phys_base: phys, pages: KSTACK_PAGES, ..MemoryObject::ZERO },
    )?;
    let cpu: Ref<Cpu> = g.typed(crate::state::cpu0()).ok_or(vm::VmError::StaleSpace)?;
    g.make_ready(cpu, t)?;
    Ok(t)
}

/// The first instruction a user thread's kernel stack returns into.
///
/// Everything up to here is ordinary kernel scheduling; this is the doorway.
/// The address space and the kernel stack were installed by `schedule` on the
/// way in, so all that is left is to hand the CPU a ring 3 frame and let it go.
pub extern "C" fn enter_user() -> ! {
    // `iretq` restores the user's flags with interrupts on. Until then they
    // stay off, so nothing can arrive while the ring 3 frame is half built.
    x86_64::instructions::interrupts::disable();

    let (entry, user_rsp) = {
        let g = GRAPH.lock();
        let t: Ref<Thread> = match g.typed(current_locked(&g)) {
            Some(t) => t,
            None => panic!("enter_user with no current thread"),
        };
        let b = g.body(t).expect("thread body");
        (b.user_entry, b.msg.words[0])
    };
    let sel = crate::cpu::selectors();
    // Requested privilege level 3 in both selectors: this is the transition.
    let cs = (sel.user_code.0 | 3) as u64;
    let ss = (sel.user_data.0 | 3) as u64;

    // SAFETY: the frame describes a valid ring 3 context in the address space
    // already installed for this thread, and interrupts are off across swapgs.
    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {rsp}",
            "push 0x202",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) ss,
            rsp = in(reg) user_rsp,
            cs = in(reg) cs,
            rip = in(reg) entry,
            options(noreturn),
        )
    }
}

/// Give the boot path a `Thread` node, so that the thing currently running is
/// in the graph like everything else and can be switched away from.
pub fn adopt_boot_thread(root: Ref<Root>) -> Result<Ref<Thread>, vm::VmError> {
    let mut g = GRAPH.lock();
    // The bootloader gave us this stack; we do not own it, so there is no
    // MemoryObject for it and `kstack_top` stays zero.
    let t = g.create_under_root(root, Thread { state: ThreadState::Running, ..Thread::ZERO })?;
    let cpu: Ref<Cpu> = g.typed(crate::state::cpu0()).ok_or(vm::VmError::StaleSpace)?;
    if let Some(b) = g.body_mut(cpu) {
        b.current = t.id();
    }
    Ok(t)
}

// ------------------------------------------------------------- the control ---

/// A plain intrusive run queue: the same job done the way a conventional kernel
/// does it, as the control for phase 4's go/no-go.
///
/// This is candidate B from DESIGN 3.3 applied to one relationship. It exists
/// only to be measured against, and it is what the fallback would look like if
/// the graph scheduler had proved too slow.
pub mod control {
    const SLOTS: usize = 64;

    #[derive(Clone, Copy)]
    struct Link {
        next: u16,
        prev: u16,
        live: bool,
    }

    pub struct Queue {
        links: [Link; SLOTS],
        head: u16,
        len: u16,
    }

    const EMPTY: u16 = u16::MAX;

    impl Queue {
        pub const fn new() -> Queue {
            Queue {
                links: [Link { next: EMPTY, prev: EMPTY, live: false }; SLOTS],
                head: EMPTY,
                len: 0,
            }
        }

        pub fn push(&mut self, id: u16) {
            let i = id as usize;
            if self.links[i].live {
                return;
            }
            self.links[i].live = true;
            if self.head == EMPTY {
                self.links[i].next = id;
                self.links[i].prev = id;
                self.head = id;
            } else {
                let head = self.head;
                let tail = self.links[head as usize].prev;
                self.links[i].next = head;
                self.links[i].prev = tail;
                self.links[tail as usize].next = id;
                self.links[head as usize].prev = id;
            }
            self.len += 1;
        }

        #[inline]
        pub fn peek(&self) -> Option<u16> {
            if self.head == EMPTY {
                None
            } else {
                Some(self.head)
            }
        }

        #[inline]
        pub fn rotate(&mut self) -> bool {
            if self.head == EMPTY {
                return false;
            }
            self.head = self.links[self.head as usize].next;
            true
        }
    }
}
