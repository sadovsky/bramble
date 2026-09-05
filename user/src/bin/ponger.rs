//! The other half. It starts with no console at all and is handed one over the
//! channel, which is the whole of capability transfer: authority arriving in a
//! message rather than being configured in advance.

#![no_std]
#![no_main]

use bramble_user::{abi, entry, exit, println, recv, send, set_console, write};

/// Receive from the pinger here.
const FROM_PINGER: u32 = 1;
/// Reply here.
const TO_PINGER: u32 = 2;

entry!(main);

extern "C" fn main() -> ! {
    // No console yet. Nothing this program does before the first message can
    // be seen, because it holds nothing that can be seen through.
    let mut msg = [0u64; abi::MSG_WORDS];
    let slot = recv(FROM_PINGER, &mut msg);
    if slot <= 0 {
        // Cannot even report this. Exit with a code the kernel will show.
        exit(2);
    }
    set_console(slot as u32);
    write(slot as u32, b"[ponger] I could not print until this arrived\n");
    println!("[ponger] received a console capability in slot {}, first word {:#x}", slot, msg[0]);

    loop {
        let got = recv(FROM_PINGER, &mut msg);
        if got < 0 {
            println!("[ponger] recv failed: {}", abi::error_name(got));
            exit(1);
        }
        if msg[0] == u64::MAX {
            break;
        }
        msg[0] += 1;
        let sent = send(TO_PINGER, &msg, 0);
        if sent != 0 {
            println!("[ponger] send failed: {}", abi::error_name(sent));
            exit(1);
        }
    }
    println!("[ponger] asked to stop, exiting");
    exit(0)
}
