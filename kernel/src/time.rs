//! The legacy 8259 interrupt controller and 8253 timer.
//!
//! v1 uses the PIC and PIT rather than the local APIC. They are simpler, they
//! need no MMIO mapping, and on one core they do the same job. The design
//! calls for a LAPIC timer, and that is the right call when SMP arrives,
//! because the PIC does not scale past one core; until then this is fifty
//! lines instead of three hundred.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// Where we remap the hardware interrupts to. The first 32 vectors belong to
/// the CPU's own exceptions, so the PIC's defaults would collide with them.
pub const IRQ_BASE: u8 = 0x20;
pub const IRQ_TIMER: u8 = IRQ_BASE;

const PIT_CH0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
const PIT_HZ: u32 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Remap both PICs above the exception vectors and mask everything.
pub fn init_pic() {
    // SAFETY: fixed legacy port addresses, written in the documented order.
    unsafe {
        let mut c1 = Port::<u8>::new(PIC1_CMD);
        let mut d1 = Port::<u8>::new(PIC1_DATA);
        let mut c2 = Port::<u8>::new(PIC2_CMD);
        let mut d2 = Port::<u8>::new(PIC2_DATA);

        c1.write(0x11); // begin initialisation, expect 4 command words
        c2.write(0x11);
        d1.write(IRQ_BASE); // master vector offset
        d2.write(IRQ_BASE + 8); // slave vector offset
        d1.write(4); // slave is on master's line 2
        d2.write(2); // slave identity
        d1.write(0x01); // 8086 mode
        d2.write(0x01);

        d1.write(0xFF); // mask everything for now
        d2.write(0xFF);
    }
}

/// Unmask one master-PIC line.
pub fn unmask(irq: u8) {
    // SAFETY: reading and writing the PIC's mask register.
    unsafe {
        let mut d1 = Port::<u8>::new(PIC1_DATA);
        let mask = d1.read() & !(1 << irq);
        d1.write(mask);
    }
}

/// Tell the PIC the interrupt has been handled. Must happen before a context
/// switch, or the next tick never arrives on the thread we switch to.
pub fn eoi(vector: u8) {
    // SAFETY: the documented end-of-interrupt command.
    unsafe {
        if vector >= IRQ_BASE + 8 {
            Port::<u8>::new(PIC2_CMD).write(0x20);
        }
        Port::<u8>::new(PIC1_CMD).write(0x20);
    }
}

/// Programme channel 0 as a rate generator at roughly `hz`.
pub fn init_timer(hz: u32) {
    let divisor = (PIT_HZ / hz).clamp(1, 65535) as u16;
    // SAFETY: fixed legacy port addresses.
    unsafe {
        Port::<u8>::new(PIT_CMD).write(0x34); // channel 0, lo/hi byte, mode 2
        let mut ch0 = Port::<u8>::new(PIT_CH0);
        ch0.write((divisor & 0xFF) as u8);
        ch0.write((divisor >> 8) as u8);
    }
}

#[inline]
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Read the cycle counter. Under TCG this is emulated, so it is only useful for
/// comparing two measurements taken the same way (DESIGN Q8).
#[inline]
pub fn rdtsc() -> u64 {
    // SAFETY: rdtsc has no operands and no side effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}
