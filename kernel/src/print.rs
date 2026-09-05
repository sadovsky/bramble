//! `print!` and `println!` that go to the serial port and, when it exists, the
//! framebuffer console. Serial is the channel the host actually reads.

use core::fmt::{self, Write};

use crate::fb::{Rgb, CONSOLE};
use crate::serial::SERIAL;

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    // Interrupts off around the two locks so a handler cannot deadlock on them.
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _ = SERIAL.lock().write_fmt(args);
        let mut c = CONSOLE.lock();
        if c.is_attached() {
            let _ = c.write_fmt(args);
        }
    });
}

#[doc(hidden)]
pub fn _print_coloured(colour: Rgb, args: fmt::Arguments) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _ = SERIAL.lock().write_fmt(args);
        let mut c = CONSOLE.lock();
        if c.is_attached() {
            c.set_colour(colour);
            let _ = c.write_fmt(args);
            c.set_colour(crate::fb::FG);
        }
    });
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::print::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

/// A line in an accent colour on the framebuffer; plain on serial.
#[macro_export]
macro_rules! cprintln {
    ($colour:expr, $($arg:tt)*) => (
        $crate::print::_print_coloured($colour, format_args!("{}\n", format_args!($($arg)*)))
    );
}
