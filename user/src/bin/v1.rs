//! The v1 goal, as a program.
//!
//! Two processes running preemptively, communicating over an endpoint, with the
//! entire kernel state inspectable as a graph. This program starts the two
//! processes, talks to them, and then hands the whole of the kernel's state to
//! the outside world as bytes, twice, so a tool on the host can draw it, check
//! it and diff it.
//!
//! Nothing here is privileged. It holds a console and the root with `Lookup`,
//! and everything else it does is built out of those two capabilities.

#![no_std]
#![no_main]

use bramble_user::{
    abi, check, dump_hex, endpoint_create, entry, exit, grant, inspect, kill, lookup, println,
    recv, send, set_console, spawn, start, yield_now,
};

const CONSOLE: u32 = 1;
const ROOT: u32 = 2;

/// In .bss, so the kernel's loader zeroes it and no allocator is involved.
static mut SNAPSHOT: [u8; 32768] = [0; 32768];

entry!(main);

fn launch(image: u32, bootstrap: u32, n: u32) -> (u32, u32) {
    let child = spawn(image);
    if child < 0 {
        println!("[v1] spawn {} failed: {}", n, abi::error_name(child));
        exit(1);
    }
    let child = child as u32;
    grant(child, CONSOLE, abi::R_READ | abi::R_WRITE);
    grant(child, bootstrap, abi::R_SEND | abi::R_GRANT);
    if start(child) != 0 {
        println!("[v1] start {} failed", n);
        exit(1);
    }
    let mut msg = [0u64; abi::MSG_WORDS];
    let slot = recv(bootstrap, &mut msg);
    if slot <= 0 {
        println!("[v1] worker {} never called back", n);
        exit(1);
    }
    (child, slot as u32)
}

fn snapshot(label: &str) {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(SNAPSHOT) };
    let n = inspect(buf);
    if n < 0 {
        println!("[v1] inspect failed: {}", abi::error_name(n));
        exit(1);
    }
    if n as usize > buf.len() {
        println!("[v1] snapshot needs {} bytes, buffer is {}", n, buf.len());
        exit(1);
    }
    dump_hex(label, &buf[..n as usize]);
}

extern "C" fn main() -> ! {
    set_console(CONSOLE);
    println!();
    println!("[v1] up");

    // Before anything else: ask the kernel to prove it is consistent with
    // itself. A program can do this. That is the whole point of the design.
    if check() != 0 {
        println!("[v1] the kernel says its own invariants do not hold");
        exit(1);
    }
    println!("[v1] the kernel checked its own invariants at my request: all hold");

    let image = lookup("worker");
    if image < 0 {
        println!("[v1] no worker image: {}", abi::error_name(image));
        exit(1);
    }
    let bootstrap = endpoint_create();
    if bootstrap < 0 {
        println!("[v1] no endpoint: {}", abi::error_name(bootstrap));
        exit(1);
    }
    let (a, a_ep) = launch(image as u32, bootstrap as u32, 1);
    let (b, b_ep) = launch(image as u32, bootstrap as u32, 2);
    println!("[v1] two workers running, reachable on slots {} and {}", a_ep, b_ep);

    // Talk to both, then let them settle back into blocking on their own
    // endpoints. At this point the graph should show two threads parked on two
    // endpoints, this thread running, and the kernel's own thread queued.
    let mut msg = [0u64; abi::MSG_WORDS];
    msg[0] = 1;
    send(a_ep, &msg, 0);
    msg[0] = 2;
    send(b_ep, &msg, 0);
    for _ in 0..16 {
        yield_now();
    }
    snapshot("quiet");

    // Now wake one of them and look again *without* yielding first. `send`
    // hands the message over and queues the receiver, but does not switch to
    // it, so the second snapshot catches that worker mid-transition: its
    // `Waiting` edge gone, a `Ready` edge in its place. Two pictures of the
    // same system, one message apart.
    msg[0] = 3;
    send(a_ep, &msg, 0);
    snapshot("one-worker-woken");

    if check() != 0 {
        println!("[v1] invariants broken after snapshotting");
        exit(1);
    }
    println!("[v1] still consistent after two snapshots");

    kill(a);
    kill(b);
    let _ = ROOT;
    println!("[v1] done");
    exit(0)
}
