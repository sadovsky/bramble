//! The other side of the shared region.
//!
//! It maps the memory at an address of its own choosing. The object is the
//! shared thing; the address is not.

#![no_std]
#![no_main]

use bramble_user::{abi, entry, exit, map, println, send, set_console};

const CONSOLE: u32 = 1;
const TO_PARENT: u32 = 2;
const SHARED: u32 = 3;

/// Deliberately nothing like the address the parent chose.
const BASE: u64 = 0x6600_0000;
const PAGES: u64 = 64;
const PAGE: u64 = 4096;
const REPLY: u64 = 0x5EED_BEEF;

entry!(main);

extern "C" fn main() -> ! {
    set_console(CONSOLE);

    if map(SHARED, BASE, abi::P_READ | abi::P_WRITE, true) != 0 {
        println!("[peer] could not map the shared memory");
        exit(1);
    }
    println!("[peer] mapped the same memory at {:#x}, my own choice of address", BASE);

    // SAFETY: inside a region this process mapped for itself, from a capability
    // its parent granted.
    let first = unsafe { core::ptr::read_volatile(BASE as *const u64) };
    let last = unsafe { core::ptr::read_volatile((BASE + (PAGES - 1) * PAGE) as *const u64) };
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile((BASE + PAGE) as *mut u64, REPLY) };
    println!("[peer] read {:#x} and {:#x}, wrote {:#x} back", first, last, REPLY);

    let mut msg = [0u64; abi::MSG_WORDS];
    msg[0] = first;
    msg[1] = last;
    msg[2] = BASE;
    send(TO_PARENT, &msg, 0);

    // Wait to be killed: the parent is done with us once it has checked.
    loop {
        bramble_user::yield_now();
    }
}
