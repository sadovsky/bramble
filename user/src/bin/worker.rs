//! A program spawned by another program, not by the kernel.
//!
//! It creates its own endpoint and sends a capability to it back to whoever
//! started it. That is the whole of service registration in a capability
//! system: there is no name to publish and no registry to publish it in, only a
//! capability handed to exactly the party that should have it.

#![no_std]
#![no_main]

use bramble_user::{abi, endpoint_create, entry, exit, println, recv, send, set_console};

/// Granted by our parent, in the order it granted them.
const CONSOLE: u32 = 1;
const TO_PARENT: u32 = 2;

entry!(main);

extern "C" fn main() -> ! {
    set_console(CONSOLE);

    // An endpoint we own. When we die it dies, and every capability anyone
    // holds to it is revoked at that instant. That is the only shutdown
    // protocol this program has, and it does not have to implement it.
    let my_ep = endpoint_create();
    if my_ep < 0 {
        println!("[worker] could not create an endpoint: {}", abi::error_name(my_ep));
        exit(1);
    }
    println!("[worker] made an endpoint of my own in slot {}", my_ep);

    // Hand our parent a way to reach us.
    let mut msg = [0u64; abi::MSG_WORDS];
    msg[0] = 0x5E12;
    let sent = send(TO_PARENT, &msg, my_ep as u32);
    if sent != 0 {
        println!("[worker] could not reach my parent: {}", abi::error_name(sent));
        exit(1);
    }

    loop {
        let got = recv(my_ep as u32, &mut msg);
        if got < 0 {
            println!("[worker] recv failed: {}", abi::error_name(got));
            exit(1);
        }
        println!("[worker] received {}", msg[0]);
    }
}
