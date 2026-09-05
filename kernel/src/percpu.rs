//! The two words the system-call entry stub needs before it has a stack.
//!
//! When user code executes `syscall`, the CPU jumps to the kernel with the
//! *user's* stack pointer still in `rsp`. Before anything else can happen the
//! kernel has to find a stack of its own, without touching memory it does not
//! already have a pointer to.
//!
//! A real kernel does this with `swapgs`: the `gs` base holds a per-cpu block
//! that only the kernel can reach. v1 does not, and the reason is worth
//! recording. `swapgs` has to be paired *exactly*: every entry from ring 3 must
//! perform it and every exit must undo it, including interrupt entries. Rust's
//! `x86-interrupt` calling convention generates its own prologue, so a handler
//! written in it cannot swap first — and a timer arriving while ring 3 runs
//! would leave the two bases crossed, after which the next system call reads
//! its stack pointer from address zero. That is a genuinely nasty failure and
//! it is exactly what happened here.
//!
//! With one core there is no per-cpu anything, so the stub reads two absolute
//! addresses instead and the whole class of bug disappears. SMP brings it back:
//! at that point these become a `gs`-relative block, and every interrupt
//! handler that can arrive from ring 3 needs a hand-written stub that swaps
//! conditionally on the saved code segment. Recorded as debt in docs/PLAN.md.

/// Kernel stack top for the current thread. Read by the syscall stub.
#[unsafe(no_mangle)]
pub static mut BRAMBLE_KERNEL_RSP: u64 = 0;

/// One scratch word, live for the two instructions between entering the kernel
/// and having a stack to push onto. It must not hold anything for longer than
/// that: interrupts are masked across those two instructions, but a system call
/// that *blocks* runs other threads, and anything left in a global here would
/// be read back by whichever thread returns to ring 3 first.
#[unsafe(no_mangle)]
pub static mut BRAMBLE_USER_RSP: u64 = 0;

pub fn init() {
    // Nothing to install: the stub reaches these by address.
}

pub fn set_kernel_rsp(top: u64) {
    // SAFETY: single core; callers hold interrupts off.
    unsafe { BRAMBLE_KERNEL_RSP = top };
}


