//! COM1 as a 16550 UART. The kernel's primary output channel: QEMU captures it
//! and the host tools parse it, so the graph dump goes here rather than to the
//! framebuffer.

use core::fmt;
use spin::Mutex;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

pub struct SerialPort {
    base: u16,
    ready: bool,
}

impl SerialPort {
    const fn new(base: u16) -> SerialPort {
        SerialPort { base, ready: false }
    }

    /// Configure for 115200 8N1 with FIFOs on.
    pub fn init(&mut self) {
        unsafe {
            Port::<u8>::new(self.base + 1).write(0x00); // interrupts off
            Port::<u8>::new(self.base + 3).write(0x80); // DLAB on
            Port::<u8>::new(self.base).write(0x01); // divisor 1 => 115200
            Port::<u8>::new(self.base + 1).write(0x00);
            Port::<u8>::new(self.base + 3).write(0x03); // 8N1, DLAB off
            Port::<u8>::new(self.base + 2).write(0xC7); // FIFO on, clear, 14-byte
            Port::<u8>::new(self.base + 4).write(0x0B); // RTS/DSR, OUT2
        }
        self.ready = true;
    }

    fn can_send(&self) -> bool {
        unsafe { Port::<u8>::new(self.base + 5).read() & 0x20 != 0 }
    }

    pub fn write_byte(&mut self, b: u8) {
        if !self.ready {
            return;
        }
        // Bounded spin: a missing UART must not hang the kernel.
        let mut spins = 0u32;
        while !self.can_send() && spins < 100_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        unsafe { Port::<u8>::new(self.base).write(b) };
    }

    /// Non-blocking read of one byte, if the receiver holds one. Unused until
    /// the console device node exists in phase 5.
    #[allow(dead_code)]
    pub fn read_byte(&mut self) -> Option<u8> {
        if !self.ready {
            return None;
        }
        let status = unsafe { Port::<u8>::new(self.base + 5).read() };
        if status & 1 == 0 {
            return None;
        }
        Some(unsafe { Port::<u8>::new(self.base).read() })
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
        Ok(())
    }
}

pub static SERIAL: Mutex<SerialPort> = Mutex::new(SerialPort::new(COM1));

pub fn init() {
    SERIAL.lock().init();
}
