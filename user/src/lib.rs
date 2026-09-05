//! Bramble's userspace runtime.
//!
//! There is no libc, no allocator and no standard library. A program gets a
//! stack, an entry point, and a handful of slot numbers naming the objects it
//! was given. It cannot name anything else: there is no path to open, no global
//! namespace to walk, and no ambient authority to fall back on. If a capability
//! was not granted, the operation is not merely forbidden, it is unsayable.

#![no_std]

pub use bramble_abi as abi;

use core::fmt::{self, Write};

// ------------------------------------------------------------- syscalls ---

/// Our calling convention: number in `rax`, arguments in `rdi`, `rsi`, `rdx`
/// and `r10`. `r10` rather than `rcx` because the `syscall` instruction
/// clobbers `rcx` with the return address before the kernel ever sees it.
///
/// # Safety
/// The kernel validates every argument, but a wrong number or a pointer the
/// caller does not own still gets an error rather than a result.
#[inline(always)]
unsafe fn syscall(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

pub fn exit(code: i32) -> ! {
    // SAFETY: never returns; the kernel destroys this process.
    unsafe { syscall(abi::SYS_EXIT, code as u64, 0, 0, 0) };
    unreachable!()
}

pub fn yield_now() {
    // SAFETY: no pointer arguments.
    unsafe { syscall(abi::SYS_YIELD, 0, 0, 0, 0) };
}

/// Write to a device, if this slot holds a capability carrying `Write`.
pub fn write(slot: u32, bytes: &[u8]) -> i64 {
    // SAFETY: the pointer and length describe a slice we own; the kernel
    // checks it against our own `Maps` edges before touching it.
    unsafe {
        syscall(abi::SYS_WRITE, slot as u64, bytes.as_ptr() as u64, bytes.len() as u64, 0)
    }
}

/// Ask the root for a name. Needs a capability to the root carrying `Lookup`.
/// Returns a new slot holding what was found.
pub fn lookup(name: &str) -> i64 {
    // SAFETY: as above.
    unsafe { syscall(abi::SYS_LOOKUP, name.as_ptr() as u64, name.len() as u64, 0, 0) }
}

/// Copy the entire kernel state into `buf`. Returns the bytes written, or the
/// size needed if the buffer is too small.
pub fn inspect(buf: &mut [u8]) -> i64 {
    // SAFETY: the kernel checks the range is mapped writable by us.
    unsafe { syscall(abi::SYS_INSPECT, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) }
}

/// Send a message. `cap_slot` of zero sends no capability; otherwise the
/// capability in that slot is copied to the receiver, masked by what we hold.
/// Passing one requires `Grant` on the endpoint.
pub fn send(ep: u32, words: &[u64; abi::MSG_WORDS], cap_slot: u32) -> i64 {
    // SAFETY: the kernel checks the buffer against our own mappings.
    unsafe { syscall(abi::SYS_SEND, ep as u64, words.as_ptr() as u64, cap_slot as u64, 0) }
}

/// Receive a message. Returns the slot a transferred capability landed in, or
/// zero if the message carried none.
pub fn recv(ep: u32, words: &mut [u64; abi::MSG_WORDS]) -> i64 {
    // SAFETY: as above, and the buffer is writable by us.
    unsafe { syscall(abi::SYS_RECV, ep as u64, words.as_mut_ptr() as u64, 0, 0) }
}

/// The cycle counter. Under emulation this is not real cycles, so only ratios
/// between measurements taken the same way mean anything.
#[inline]
pub fn rdtsc() -> u64 {
    // SAFETY: rdtsc has no operands and no side effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// What rights does this slot carry? Lets a program find out what it may do
/// without having to fail first.
pub fn rights(slot: u32) -> i64 {
    // SAFETY: no pointer arguments.
    unsafe { syscall(abi::SYS_RIGHTS, slot as u64, 0, 0, 0) }
}

// ---------------------------------------------------------------- output ---

/// The slot programs print through. Set once at startup; there is no default,
/// because there is no such thing as a console you were not given.
static mut CONSOLE: u32 = 0;

pub fn set_console(slot: u32) {
    // SAFETY: single-threaded programs, set before any printing.
    unsafe { CONSOLE = slot };
}

pub fn console() -> u32 {
    // SAFETY: as above.
    unsafe { CONSOLE }
}

/// Formats into a fixed buffer and flushes when it fills or a line ends. No
/// allocator, so the buffer is the limit.
pub struct Console {
    buf: [u8; 256],
    len: usize,
}

impl Console {
    pub const fn new() -> Console {
        Console { buf: [0; 256], len: 0 }
    }
    pub fn flush(&mut self) {
        if self.len > 0 {
            write(console(), &self.buf[..self.len]);
            self.len = 0;
        }
    }
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = b;
            self.len += 1;
            if b == b'\n' {
                self.flush();
            }
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut c = Console::new();
    let _ = c.write_fmt(args);
    c.flush();
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

/// Render a rights bitmask the way the kernel's own dump does.
pub fn rights_str(bits: u32, out: &mut [u8; 8]) -> &str {
    const NAMES: [(u32, u8); 8] = [
        (abi::R_READ, b'R'),
        (abi::R_WRITE, b'W'),
        (abi::R_MAP, b'M'),
        (abi::R_SEND, b's'),
        (abi::R_RECV, b'r'),
        (abi::R_GRANT, b'g'),
        (abi::R_LOOKUP, b'l'),
        (abi::R_MANAGE, b'A'),
    ];
    let mut n = 0;
    for (bit, ch) in NAMES {
        if bits & bit != 0 {
            out[n] = ch;
            n += 1;
        }
    }
    if n == 0 {
        out[0] = b'-';
        n = 1;
    }
    core::str::from_utf8(&out[..n]).unwrap_or("?")
}

// ----------------------------------------------------------------- entry ---

/// Define a program's entry point.
///
/// The kernel jumps here rather than calling, so the stack is 16-byte aligned
/// where the ABI expects 8. Realigning and then calling fixes that, and gives
/// the real entry point an ordinary frame.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(naked)]
        #[no_mangle]
        pub extern "C" fn _start() -> ! {
            core::arch::naked_asm!(
                "and rsp, -16",
                "call {main}",
                "ud2",
                main = sym $main,
            )
        }
    };
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("user panic: {}", info.message());
    exit(101)
}
