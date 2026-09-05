//! A program that misbehaves on purpose.
//!
//! It holds a console capability and uses it, then writes through a null
//! pointer. The kernel must destroy it and reclaim everything it owned, with
//! every invariant still holding afterwards. A hobby kernel that cannot survive
//! this cannot survive anything.

#![no_std]
#![no_main]

use bramble_user::{entry, println, set_console};

entry!(main);

extern "C" fn main() -> ! {
    set_console(1);
    println!("[faulter] alive, holding one console capability");
    println!("[faulter] about to write through a null pointer");

    // SAFETY: none whatsoever. That is the point.
    unsafe {
        core::ptr::write_volatile(0 as *mut u64, 0xdead);
    }

    println!("[faulter] FAULT: still running after touching address zero");
    bramble_user::exit(1)
}
