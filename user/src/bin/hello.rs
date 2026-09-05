//! Bramble's first user program.
//!
//! It demonstrates the one thing the design is really about: authority is an
//! edge. The program can write to the console because it holds a capability
//! carrying `Write`, and it cannot write through a second capability to the
//! *same device* that lacks the right. Nothing about the program changed
//! between those two calls; only the edge did.

#![no_std]
#![no_main]

use bramble_user::{abi, entry, exit, inspect, lookup, println, rights, rights_str, set_console, write};

/// Slots the kernel granted us, in the order it granted them.
const CONSOLE_RW: u32 = 1;
const CONSOLE_RO: u32 = 2;
const ROOT_LOOKUP: u32 = 3;

/// Big enough for the whole kernel state at this stage, in .bss.
static mut SNAPSHOT: [u8; 16384] = [0; 16384];

entry!(main);

extern "C" fn main() -> ! {
    // The plainest possible system call, before any formatting machinery runs.
    // Kept because it is the first thing to check when userspace goes wrong: if
    // this line appears and nothing else does, the fault is above the syscall
    // boundary rather than at it.
    write(CONSOLE_RW, b"\n[user] hello from ring 3\n");
    set_console(CONSOLE_RW);

    capabilities();
    naming();
    whole_kernel_state();

    println!("[user] exiting cleanly with code 0");
    exit(0)
}

/// Two capabilities to one device. Only one of them can write.
fn capabilities() {
    let mut a = [0u8; 8];
    let mut b = [0u8; 8];
    let mut c = [0u8; 8];
    println!(
        "[user] slot {} (console) carries {}, slot {} (same console) carries {}, slot {} (root) carries {}",
        CONSOLE_RW,
        rights_str(rights(CONSOLE_RW) as u32, &mut a),
        CONSOLE_RO,
        rights_str(rights(CONSOLE_RO) as u32, &mut b),
        ROOT_LOOKUP,
        rights_str(rights(ROOT_LOOKUP) as u32, &mut c)
    );

    // The same device, the same bytes, a different edge.
    let denied = write(CONSOLE_RO, b"this must never appear\n");
    if denied == abi::E_PERM {
        println!("[user] writing through the read-only capability was refused, as it should be");
    } else {
        println!("[user] FAULT: read-only capability returned {} instead of E_PERM", denied);
        exit(1);
    }

    // A slot that was never granted is not "permission denied", it is nothing
    // at all. There is no object on the other side to be denied by. Slot zero,
    // the null handle, behaves the same way.
    if write(0, b"nor this\n") != abi::E_BADHANDLE {
        println!("[user] FAULT: the null handle was accepted");
        exit(1);
    }
    let nothing = write(200, b"nor this either\n");
    if nothing == abi::E_BADHANDLE {
        println!("[user] an ungranted slot names nothing, so there is nothing to refuse");
    } else {
        println!("[user] FAULT: empty slot returned {}", nothing);
        exit(1);
    }

    // A pointer we do not own is refused too, and by the same mechanism: the
    // kernel checks it against our own Maps edges.
    let bad_ptr = unsafe {
        core::slice::from_raw_parts(0xffff_8000_0000_0000u64 as *const u8, 8)
    };
    if write(CONSOLE_RW, bad_ptr) == abi::E_FAULT {
        println!("[user] a pointer into the kernel's half was refused");
    } else {
        println!("[user] FAULT: kernel pointer was accepted");
        exit(1);
    }
}

/// A name is one query among many, not the privileged way to address anything.
fn naming() {
    let slot = lookup("console");
    if slot < 0 {
        println!("[user] lookup(\"console\") failed: {}", abi::error_name(slot));
        exit(1);
    }
    println!("[user] lookup(\"console\") granted a new capability in slot {}", slot);
    write(slot as u32, b"[user] and this line went through the looked-up capability\n");

    let missing = lookup("no-such-thing");
    if missing != abi::E_NOTFOUND {
        println!("[user] FAULT: lookup of a missing name returned {}", missing);
        exit(1);
    }
}

/// The whole kernel, as data, in one call.
fn whole_kernel_state() {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(SNAPSHOT) };
    let n = inspect(buf);
    if n < 0 {
        println!("[user] inspect failed: {}", abi::error_name(n));
        exit(1);
    }
    let header: abi::InspectHeader = unsafe { core::ptr::read_unaligned(buf.as_ptr().cast()) };
    if header.magic != abi::INSPECT_MAGIC {
        println!("[user] FAULT: snapshot magic was {:#x}", header.magic);
        exit(1);
    }
    println!(
        "[user] inspect: {} bytes, seq {}, {} nodes, {} edges, {} of {} frames free, {} ticks",
        n,
        header.seq,
        header.node_count,
        header.edge_count,
        header.free_frames,
        header.total_frames,
        header.ticks
    );

    // Count the graph by kind, from userspace, with no kernel help. This is the
    // property the whole design is for: one structure, one query, everything.
    let mut by_kind = [0u32; 8];
    for i in 0..header.node_count as usize {
        let at = header.node_offset as usize + i * core::mem::size_of::<abi::NodeRecord>();
        let rec: abi::NodeRecord =
            unsafe { core::ptr::read_unaligned(buf.as_ptr().add(at).cast()) };
        if (rec.kind as usize) < by_kind.len() {
            by_kind[rec.kind as usize] += 1;
        }
    }
    let mut edges_by_kind = [0u32; 7];
    for i in 0..header.edge_count as usize {
        let at = header.edge_offset as usize + i * core::mem::size_of::<abi::EdgeRecord>();
        let rec: abi::EdgeRecord =
            unsafe { core::ptr::read_unaligned(buf.as_ptr().add(at).cast()) };
        if (rec.kind as usize) < edges_by_kind.len() {
            edges_by_kind[rec.kind as usize] += 1;
        }
    }

    print_counts("[user] nodes:", &abi::NODE_KIND_NAMES, &by_kind);
    print_counts("[user] edges:", &abi::EDGE_KIND_NAMES, &edges_by_kind);

    // And the part a conventional kernel cannot answer in one place: which
    // capabilities does this process hold, and to what?
    let mut held = 0;
    for i in 0..header.edge_count as usize {
        let at = header.edge_offset as usize + i * core::mem::size_of::<abi::EdgeRecord>();
        let rec: abi::EdgeRecord =
            unsafe { core::ptr::read_unaligned(buf.as_ptr().add(at).cast()) };
        if rec.kind == 1 {
            held += 1;
        }
    }
    println!("[user] the whole system holds {} capabilities in total", held);
}

fn print_counts(label: &str, names: &[&str], counts: &[u32]) {
    let mut line = bramble_user::Console::new();
    use core::fmt::Write;
    let _ = write!(line, "{}", label);
    for (i, n) in counts.iter().enumerate() {
        if *n > 0 {
            let _ = write!(line, " {}x{}", n, names[i]);
        }
    }
    let _ = writeln!(line);
    line.flush();
}
