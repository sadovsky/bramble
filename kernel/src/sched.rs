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

use crate::state::{BOOT, FRAMES, GRAPH};

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
        let cpu: Ref<Cpu> = match g.typed(BOOT.lock().cpu0) {
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
        let new_rsp = match g.body(next) {
            Some(b) => b.saved_rsp,
            None => return,
        };
        Decision { save_slot, new_rsp, switching: true }
    };

    if decision.switching {
        // SAFETY: interrupts are off and this is the only core, so the arena
        // cannot move; both stack pointers were prepared by this module.
        unsafe { switch_stack(decision.save_slot, decision.new_rsp) };
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
    match g.typed::<Cpu>(BOOT.lock().cpu0) {
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
        if let Some(cpu) = g.typed::<Cpu>(BOOT.lock().cpu0) {
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
pub const KSTACK_PAGES: u32 = 4;

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
    let cpu: Ref<Cpu> = g.typed(BOOT.lock().cpu0).ok_or(vm::VmError::StaleSpace)?;
    g.make_ready(cpu, t)?;
    Ok(t)
}

/// Give the boot path a `Thread` node, so that the thing currently running is
/// in the graph like everything else and can be switched away from.
pub fn adopt_boot_thread(root: Ref<Root>) -> Result<Ref<Thread>, vm::VmError> {
    let mut g = GRAPH.lock();
    // The bootloader gave us this stack; we do not own it, so there is no
    // MemoryObject for it and `kstack_top` stays zero.
    let t = g.create_under_root(root, Thread { state: ThreadState::Running, ..Thread::ZERO })?;
    let cpu: Ref<Cpu> = g.typed(BOOT.lock().cpu0).ok_or(vm::VmError::StaleSpace)?;
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
