//! A scrolling text console on the Limine framebuffer.
//!
//! Deliberately simple: no double buffering, no cursor, no escape sequences.
//! Its job in v1 is to prove the machine is alive and to show a status banner;
//! anything the host needs to parse goes over the serial port instead.

use core::fmt;
use spin::Mutex;

use crate::font::{glyph, GLYPH_H, GLYPH_W};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const FG: Rgb = Rgb(0xC8, 0xD0, 0xC8);
pub const BG: Rgb = Rgb(0x0C, 0x10, 0x0E);
pub const ACCENT: Rgb = Rgb(0x86, 0xC2, 0x32);
pub const ALERT: Rgb = Rgb(0xE0, 0x5A, 0x4A);

/// Where the pixels are and how they are laid out. Copied out of the Limine
/// response at boot so nothing later needs the bootloader's structures.
#[derive(Clone, Copy)]
pub struct Surface {
    pub addr: *mut u8,
    pub width: usize,
    pub height: usize,
    pub pitch: usize,
    pub bpp: usize,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

unsafe impl Send for Surface {}

pub struct Console {
    surface: Option<Surface>,
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    fg: Rgb,
}

impl Console {
    const fn new() -> Console {
        Console { surface: None, col: 0, row: 0, cols: 0, rows: 0, fg: FG }
    }

    pub fn attach(&mut self, s: Surface) {
        self.cols = s.width / GLYPH_W;
        self.rows = s.height / GLYPH_H;
        self.surface = Some(s);
        self.col = 0;
        self.row = 0;
        self.clear();
    }

    pub fn is_attached(&self) -> bool {
        self.surface.is_some()
    }

    pub fn set_colour(&mut self, c: Rgb) {
        self.fg = c;
    }

    #[inline]
    fn encode(s: &Surface, c: Rgb) -> u32 {
        (c.0 as u32) << s.red_shift | (c.1 as u32) << s.green_shift | (c.2 as u32) << s.blue_shift
    }

    pub fn put_pixel(&mut self, x: usize, y: usize, c: Rgb) {
        let s = match self.surface {
            Some(s) => s,
            None => return,
        };
        if x >= s.width || y >= s.height {
            return;
        }
        let v = Self::encode(&s, c);
        // SAFETY: the offset is inside the framebuffer the bootloader gave us,
        // bounds-checked immediately above.
        unsafe {
            let p = s.addr.add(y * s.pitch + x * (s.bpp / 8)) as *mut u32;
            p.write_volatile(v);
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Rgb) {
        for dy in 0..h {
            for dx in 0..w {
                self.put_pixel(x + dx, y + dy, c);
            }
        }
    }

    pub fn clear(&mut self) {
        let (w, h) = match self.surface {
            Some(s) => (s.width, s.height),
            None => return,
        };
        self.fill_rect(0, 0, w, h, BG);
        self.col = 0;
        self.row = 0;
    }

    fn scroll(&mut self) {
        let s = match self.surface {
            Some(s) => s,
            None => return,
        };
        let line = GLYPH_H * s.pitch;
        let total = s.height * s.pitch;
        // SAFETY: both ranges are inside the framebuffer; regions may overlap,
        // which is why this is `copy` and not `copy_nonoverlapping`.
        unsafe {
            core::ptr::copy(s.addr.add(line), s.addr, total - line);
        }
        self.fill_rect(0, s.height - GLYPH_H, s.width, GLYPH_H, BG);
        self.row = self.rows.saturating_sub(1);
    }

    pub fn draw_char(&mut self, x: usize, y: usize, ch: u8, c: Rgb) {
        let rows = glyph(ch);
        for (dy, bits) in rows.iter().enumerate() {
            for dx in 0..GLYPH_W {
                if bits & (0x80 >> dx) != 0 {
                    self.put_pixel(x + dx, y + dy, c);
                }
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row >= self.rows {
            self.scroll();
        }
    }

    pub fn write_byte(&mut self, b: u8) {
        if self.surface.is_none() {
            return;
        }
        match b {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                for _ in 0..4 {
                    self.write_byte(b' ');
                }
            }
            _ => {
                if self.col >= self.cols {
                    self.newline();
                }
                let (x, y) = (self.col * GLYPH_W, self.row * GLYPH_H);
                let fg = self.fg;
                self.draw_char(x, y, b, fg);
                self.col += 1;
            }
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            self.write_byte(b);
        }
        Ok(())
    }
}

pub static CONSOLE: Mutex<Console> = Mutex::new(Console::new());
