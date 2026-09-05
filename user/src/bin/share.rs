//! Shared memory, mediated entirely by a capability.
//!
//! This program reserves a region of memory that does not exist yet, writes to
//! two of its pages, and hands a capability to it to a child. The child maps the
//! same object at a *different* virtual address and sees the same bytes.
//!
//! Nothing named the memory. There is no shared-memory key, no path in `/dev`,
//! no identifier the child could have guessed. The child can reach those pages
//! because an edge was created saying it may, and for no other reason.

#![no_std]
#![no_main]

use bramble_user::{
    abi, endpoint_create, entry, exit, grant, kill, lookup, map, mem_create, println, recv,
    set_console, spawn, start,
};

const CONSOLE: u32 = 1;
const ROOT: u32 = 2;

const PAGES: u64 = 64;
const BASE: u64 = 0x2000_0000;
const PAGE: u64 = 4096;

const MAGIC_FIRST: u64 = 0x5EED_0001;
const MAGIC_LAST: u64 = 0x5EED_0002;
const REPLY: u64 = 0x5EED_BEEF;

entry!(main);

fn poke(at: u64, v: u64) {
    // SAFETY: inside a region this process mapped for itself.
    unsafe { core::ptr::write_volatile(at as *mut u64, v) }
}

fn peek(at: u64) -> u64 {
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile(at as *const u64) }
}

extern "C" fn main() -> ! {
    set_console(CONSOLE);
    println!();
    println!("[share] up");

    let mem = mem_create(PAGES, true);
    if mem < 0 {
        println!("[share] mem_create failed: {}", abi::error_name(mem));
        exit(1);
    }
    if map(mem as u32, BASE, abi::P_READ | abi::P_WRITE, true) != 0 {
        println!("[share] map failed");
        exit(1);
    }
    // Two pages of sixty-four. Only these two ever become real.
    poke(BASE, MAGIC_FIRST);
    poke(BASE + (PAGES - 1) * PAGE, MAGIC_LAST);
    println!("[share] reserved {} pages, made 2 of them real, wrote to both", PAGES);

    let image = lookup("peer");
    let bootstrap = endpoint_create();
    if image < 0 || bootstrap < 0 {
        println!("[share] could not find the peer or make an endpoint");
        exit(1);
    }
    let child = spawn(image as u32);
    if child < 0 {
        println!("[share] spawn failed: {}", abi::error_name(child));
        exit(1);
    }
    let child = child as u32;
    grant(child, CONSOLE, abi::R_READ | abi::R_WRITE);
    grant(child, bootstrap as u32, abi::R_SEND | abi::R_GRANT);
    // The whole of the sharing: one edge, carrying the right to map.
    let peer_slot = grant(child, mem as u32, abi::R_READ | abi::R_WRITE | abi::R_MAP);
    if peer_slot < 0 {
        println!("[share] could not grant the memory: {}", abi::error_name(peer_slot));
        exit(1);
    }
    println!("[share] granted the peer read, write and map on that memory, as its slot {}", peer_slot);
    start(child);

    let mut msg = [0u64; abi::MSG_WORDS];
    if recv(bootstrap as u32, &mut msg) < 0 {
        println!("[share] the peer never reported back");
        exit(1);
    }
    if msg[0] != MAGIC_FIRST || msg[1] != MAGIC_LAST {
        println!("[share] the peer read {:#x} and {:#x}, wanted {:#x} and {:#x}",
                 msg[0], msg[1], MAGIC_FIRST, MAGIC_LAST);
        exit(1);
    }
    println!("[share] the peer read both values back from its own address {:#x}", msg[2]);

    // And the other direction: it wrote, and we can see it.
    let seen = peek(BASE + PAGE);
    if seen != REPLY {
        println!("[share] expected {:#x} written by the peer, found {:#x}", REPLY, seen);
        exit(1);
    }
    println!("[share] and what the peer wrote is visible here: {:#x}", seen);

    kill(child);
    let _ = ROOT;
    println!("[share] done");
    exit(0)
}
