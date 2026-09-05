//! Memory management, from userspace.
//!
//! This program allocates a region far larger than it intends to use, maps it
//! *lazily*, and touches three pages of it. The mapping exists in the kernel's
//! graph the moment `map` returns; the page tables learn about it three pages
//! at a time, as the pages are touched.
//!
//! That distinction is the whole point. The graph says what memory this process
//! has. The page tables are a cache of that, and a cache is allowed to be cold.

#![no_std]
#![no_main]

use bramble_user::{abi, check, entry, exit, map, mem_create, println, set_console, unmap};

const CONSOLE: u32 = 1;

/// Far more than will be touched, to make the point.
const PAGES: u64 = 512;
const BASE: u64 = 0x5000_0000;
const PAGE: u64 = 4096;

entry!(main);

fn touch(vaddr: u64, value: u64) {
    // SAFETY: inside a region this process mapped for itself. The first access
    // to each page faults, and the kernel fills it in.
    unsafe { core::ptr::write_volatile(vaddr as *mut u64, value) }
}

fn peek(vaddr: u64) -> u64 {
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile(vaddr as *const u64) }
}

extern "C" fn main() -> ! {
    set_console(CONSOLE);
    println!();
    println!("[lazy] up");

    let mem = mem_create(PAGES);
    if mem < 0 {
        println!("[lazy] mem_create failed: {}", abi::error_name(mem));
        exit(1);
    }
    println!("[lazy] allocated {} pages ({} KiB) in slot {}", PAGES, PAGES * 4, mem);

    let r = map(mem as u32, BASE, abi::P_READ | abi::P_WRITE, true);
    if r != 0 {
        println!("[lazy] map failed: {}", abi::error_name(r));
        exit(1);
    }
    println!("[lazy] mapped all {} pages at {:#x}, lazily: no page-table entries yet", PAGES, BASE);

    // Three pages, spread across the region so a linear scan would be a poor
    // way to find them.
    let touched = [0u64, 200, 511];
    for (i, page) in touched.iter().enumerate() {
        let at = BASE + page * PAGE;
        touch(at, 0x000A_110C + i as u64);
    }
    for (i, page) in touched.iter().enumerate() {
        let at = BASE + page * PAGE;
        let want = 0x000A_110C + i as u64;
        if peek(at) != want {
            println!("[lazy] page {} read back {:#x}, wanted {:#x}", page, peek(at), want);
            exit(1);
        }
    }
    println!("[lazy] touched and verified {} of {} pages", touched.len(), PAGES);

    // The kernel's own checker has to be happy with a half-realised mapping:
    // invariant I5 permits a lazy mapping to have no entry, but still permits
    // no entry that no mapping authorises.
    if check() != 0 {
        println!("[lazy] the kernel is inconsistent with a lazy mapping in place");
        exit(1);
    }
    println!("[lazy] kernel invariants still hold with the mapping half realised");

    // A page outside the mapping is not lazily anything. It should kill us, so
    // do not touch one; unmapping and exiting is the honest end.
    if unmap(BASE) != 0 {
        println!("[lazy] unmap failed");
        exit(1);
    }
    if check() != 0 {
        println!("[lazy] inconsistent after unmapping");
        exit(1);
    }
    println!("[lazy] unmapped, still consistent");
    exit(0)
}
