//! One half of Bramble's first conversation between two processes.
//!
//! The two programs share nothing: no memory, no files, no names. They can
//! reach each other only because each was handed a capability to the same pair
//! of endpoints. Take either capability away and there is no channel left, and
//! no way for them to find one.

#![no_std]
#![no_main]

use bramble_user::{abi, entry, exit, println, rdtsc, recv, send, set_console};

const CONSOLE: u32 = 1;
/// Send to the ponger on this one.
const TO_PONGER: u32 = 2;
/// Receive replies on this one. Two endpoints, so a message can never be
/// collected by the process that sent it.
const FROM_PONGER: u32 = 3;

/// User programs are always built optimised, but they run inside whichever
/// kernel is under test, and a debug kernel is roughly twenty times slower.
/// Nothing here can know which, so this is a compromise: long enough to average
/// over, short enough not to make a debug boot interminable.
const ROUNDS: u64 = 500;

entry!(main);

extern "C" fn main() -> ! {
    set_console(CONSOLE);
    println!("[pinger] up, holding a console and two endpoint capabilities");

    // First, hand the ponger a capability to the console over the channel. It
    // starts with no way to print at all.
    let mut msg = [0u64; abi::MSG_WORDS];
    msg[0] = 0xC0FFEE;
    let r = send(TO_PONGER, &msg, CONSOLE);
    if r != 0 {
        println!("[pinger] handing over the console failed: {}", abi::error_name(r));
        exit(1);
    }
    println!("[pinger] sent the ponger a capability to my console");

    // A round trip is send + recv on each side: four system calls and two
    // context switches.
    let start = rdtsc();
    for i in 0..ROUNDS {
        msg[0] = i;
        let sent = send(TO_PONGER, &msg, 0);
        if sent != 0 {
            println!("[pinger] send failed at round {}: {}", i, abi::error_name(sent));
            exit(1);
        }
        let got = recv(FROM_PONGER, &mut msg);
        if got < 0 {
            println!("[pinger] recv failed at round {}: {}", i, abi::error_name(got));
            exit(1);
        }
        if msg[0] != i + 1 {
            println!("[pinger] round {} came back as {}", i, msg[0]);
            exit(1);
        }
    }
    let elapsed = rdtsc() - start;

    // A system call that does almost nothing, as the baseline to subtract.
    let start = rdtsc();
    for _ in 0..ROUNDS {
        bramble_user::rights(CONSOLE);
        bramble_user::rights(CONSOLE);
    }
    let null_calls = rdtsc() - start;

    println!("[pinger] {} round trips completed, every reply correct", ROUNDS);
    println!("[pinger] round trip          {} cycles", elapsed / ROUNDS);
    println!("[pinger] two null syscalls   {} cycles", null_calls / ROUNDS);

    // Tell the ponger to stop.
    msg[0] = u64::MAX;
    send(TO_PONGER, &msg, 0);
    println!("[pinger] done");
    exit(0)
}
