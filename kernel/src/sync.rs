//! The kernel's locking discipline.
//!
//! DESIGN 3.8: v1 has one global graph lock, and every critical section is
//! bounded. The lock disables interrupts while held, because the timer and the
//! serial receiver both mutate the graph from interrupt context and a handler
//! that spun on a lock the interrupted code already held would deadlock a
//! single core instantly.
//!
//! The rule that keeps SMP possible is not in this file but in how it is used:
//! no reference into the graph outlives its guard. Everything outside a
//! critical section holds a `NodeId` or `EdgeId` and revalidates.

use core::ops::{Deref, DerefMut};

use x86_64::instructions::interrupts;

pub struct IrqLock<T> {
    inner: spin::Mutex<T>,
}

pub struct IrqGuard<'a, T> {
    guard: Option<spin::MutexGuard<'a, T>>,
    /// Whether interrupts were enabled when we took the lock.
    restore: bool,
}

impl<T> IrqLock<T> {
    pub const fn new(value: T) -> IrqLock<T> {
        IrqLock { inner: spin::Mutex::new(value) }
    }

    pub fn lock(&self) -> IrqGuard<'_, T> {
        let restore = interrupts::are_enabled();
        interrupts::disable();
        IrqGuard { guard: Some(self.inner.lock()), restore }
    }

    /// Take the lock only if it is free. Used by the panic path, which must not
    /// deadlock on a lock the faulting code was already holding.
    pub fn try_lock(&self) -> Option<IrqGuard<'_, T>> {
        let restore = interrupts::are_enabled();
        interrupts::disable();
        match self.inner.try_lock() {
            Some(g) => Some(IrqGuard { guard: Some(g), restore }),
            None => {
                if restore {
                    interrupts::enable();
                }
                None
            }
        }
    }
}

unsafe impl<T: Send> Sync for IrqLock<T> {}
unsafe impl<T: Send> Send for IrqLock<T> {}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().expect("guard is live until drop")
    }
}

impl<T> DerefMut for IrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().expect("guard is live until drop")
    }
}

impl<T> Drop for IrqGuard<'_, T> {
    fn drop(&mut self) {
        // Release the lock before re-enabling interrupts, never the other way
        // round: an interrupt taken with the lock still held would deadlock.
        drop(self.guard.take());
        if self.restore {
            interrupts::enable();
        }
    }
}
