//! The first program: it starts the others.
//!
//! Everything below here happens in userspace. The kernel loads exactly one
//! program and grants it exactly three capabilities; every process, endpoint
//! and grant after that is this program's doing, and dies with it.

#![no_std]
#![no_main]

use bramble_user::{
    abi, endpoint_create, entry, exit, grant, kill, lookup, println, recv, rights, send,
    set_console, spawn, start, yield_now,
};

const CONSOLE: u32 = 1;
const ROOT: u32 = 2;

entry!(main);

/// Start a worker and wait for it to hand back a capability to itself.
/// Returns (process slot, worker endpoint slot).
fn launch(image: u32, bootstrap: u32, generation: u32) -> (u32, u32) {
    let child = spawn(image);
    if child < 0 {
        println!("[init] spawn failed: {}", abi::error_name(child));
        exit(1);
    }
    let child = child as u32;

    // Its entire authority, decided here, before it runs a single instruction.
    let console_slot = grant(child, CONSOLE, abi::R_READ | abi::R_WRITE);
    let reply_slot = grant(child, bootstrap, abi::R_SEND | abi::R_GRANT);
    if console_slot < 0 || reply_slot < 0 {
        println!("[init] granting failed");
        exit(1);
    }
    println!(
        "[init] worker {} is process slot {}, granted console as {} and a reply channel as {}",
        generation, child, console_slot, reply_slot
    );

    if start(child) != 0 {
        println!("[init] start failed");
        exit(1);
    }

    // It answers with a capability to an endpoint it made itself.
    let mut msg = [0u64; abi::MSG_WORDS];
    let slot = recv(bootstrap, &mut msg);
    if slot <= 0 {
        println!("[init] the worker did not send back a way to reach it");
        exit(1);
    }
    println!("[init] worker {} sent back a capability to itself in slot {}", generation, slot);
    (child, slot as u32)
}

extern "C" fn main() -> ! {
    set_console(CONSOLE);
    println!();
    let mut r = [0u8; 8];
    println!(
        "[init] up, holding a console and the root (root rights: {})",
        bramble_user::rights_str(rights(ROOT) as u32, &mut r)
    );

    let image = lookup("worker");
    if image < 0 {
        println!("[init] cannot find the worker image: {}", abi::error_name(image));
        exit(1);
    }
    let image = image as u32;

    let bootstrap = endpoint_create();
    if bootstrap < 0 {
        println!("[init] cannot make an endpoint: {}", abi::error_name(bootstrap));
        exit(1);
    }
    let bootstrap = bootstrap as u32;

    // ---- a worker, a conversation, and a death ----
    let (child, worker_ep) = launch(image, bootstrap, 1);

    let mut msg = [0u64; abi::MSG_WORDS];
    msg[0] = 111;
    if send(worker_ep, &msg, 0) != 0 {
        println!("[init] could not talk to worker 1");
        exit(1);
    }

    println!("[init] killing worker 1");
    if kill(child) != 0 {
        println!("[init] kill failed");
        exit(1);
    }

    // Killing it revokes every capability to everything it owned, including the
    // endpoint it made. We can watch that happen from here: the slot we hold
    // empties out on its own, with nobody telling us to clean it up.
    let mut waited = 0;
    while rights(worker_ep) >= 0 {
        yield_now();
        waited += 1;
        if waited > 20000 {
            println!("[init] the capability to a dead worker never went away");
            exit(1);
        }
    }
    println!("[init] after {} yields, my capability to its endpoint is simply gone", waited);

    // And sending through it fails cleanly rather than blocking for ever, which
    // is the difference between a revoked capability and a dangling pointer.
    msg[0] = 222;
    let after = send(worker_ep, &msg, 0);
    if after != abi::E_BADHANDLE {
        println!("[init] sending to a dead worker returned {} instead of a bad handle", after);
        exit(1);
    }
    println!("[init] sending through the revoked capability failed cleanly");

    // ---- and again, to show the system is not merely surviving ----
    let (child2, worker_ep2) = launch(image, bootstrap, 2);
    msg[0] = 333;
    if send(worker_ep2, &msg, 0) != 0 {
        println!("[init] could not talk to worker 2");
        exit(1);
    }
    println!("[init] killing worker 2");
    kill(child2);
    while rights(worker_ep2) >= 0 {
        yield_now();
    }

    println!("[init] done; exiting, which is the only cleanup this program does");
    exit(0)
}
