//! Bramble: a kernel whose entire state is one typed directed graph.
//!
//! Phase 0 (see `docs/PLAN.md`): come up under Limine, get a serial port and a
//! framebuffer console, install descriptor tables, and prove the exception
//! path works. No graph yet; that is phase 2.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

mod cpu;
mod dump;
mod elf;
mod fb;
mod font;
mod frames;
mod inspect;
mod ipc;
mod paging;
mod percpu;
mod print;
mod proc;
mod reaper;
mod sched;
mod selftest;
mod serial;
mod syscall;
mod state;
mod sync;
mod time;
mod vm;

use limine::request::{
    ExecutableCmdlineRequest, FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest,
    RequestsEndMarker, RequestsStartMarker, StackSizeRequest,
};
use limine::memory_map::EntryType;
use limine::BaseRevision;

// The bootloader scans this segment for requests. `#[used]` keeps the linker
// from dropping statics nothing in Rust reads.

#[used]
#[link_section = ".requests_start_marker"]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[link_section = ".requests"]
static BASE_REVISION: BaseRevision = BaseRevision::new();

/// Debug builds put large frames on the stack; ask for room.
const STACK_SIZE: u64 = 256 * 1024;

#[used]
#[link_section = ".requests"]
static STACK: StackSizeRequest = StackSizeRequest::new().with_size(STACK_SIZE);

#[used]
#[link_section = ".requests"]
static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

#[used]
#[link_section = ".requests"]
static MEMORY_MAP: MemoryMapRequest = MemoryMapRequest::new();

#[used]
#[link_section = ".requests"]
static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[link_section = ".requests"]
static CMDLINE: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

#[used]
#[link_section = ".requests"]
static MODULES: ModuleRequest = ModuleRequest::new();

#[used]
#[link_section = ".requests_end_marker"]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

/// Disable interrupts and stop. Every unrecoverable path ends here.
pub fn halt_forever() -> ! {
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}

/// Substring search, for a kernel with no `str` helpers to lean on.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

fn entry_type_name(t: EntryType) -> &'static str {
    match t {
        EntryType::USABLE => "usable",
        EntryType::RESERVED => "reserved",
        EntryType::ACPI_RECLAIMABLE => "acpi-reclaim",
        EntryType::ACPI_NVS => "acpi-nvs",
        EntryType::BAD_MEMORY => "bad",
        EntryType::BOOTLOADER_RECLAIMABLE => "boot-reclaim",
        EntryType::EXECUTABLE_AND_MODULES => "kernel+modules",
        EntryType::FRAMEBUFFER => "framebuffer",
        _ => "unknown",
    }
}

/// Draw the wordmark and a rule across the top of the framebuffer.
fn draw_banner() {
    const TITLE: &[u8] = b"bramble";
    let mut c = fb::CONSOLE.lock();
    if !c.is_attached() {
        return;
    }
    // Three stacked bars, evoking a bramble: dense, tangled, all one thing.
    for (i, y) in [6usize, 10, 14].iter().enumerate() {
        let w = 200 - i * 40;
        c.fill_rect(24, *y, w, 2, fb::ACCENT);
    }
    for (i, ch) in TITLE.iter().enumerate() {
        c.draw_char(24 + i * (font::GLYPH_W * 2), 24, *ch, fb::ACCENT);
        c.draw_char(24 + i * (font::GLYPH_W * 2) + 1, 24, *ch, fb::ACCENT);
    }
    c.fill_rect(24, 46, 400, 1, fb::ACCENT);
    // Put the text cursor below the banner.
    for _ in 0..5 {
        c.write_byte(b'\n');
    }
}

#[no_mangle]
extern "C" fn kmain() -> ! {
    serial::init();

    assert!(BASE_REVISION.is_supported(), "bootloader is too old for this kernel");

    if let Some(response) = FRAMEBUFFER.get_response() {
        if let Some(f) = response.framebuffers().next() {
            fb::CONSOLE.lock().attach(fb::Surface {
                addr: f.addr(),
                width: f.width() as usize,
                height: f.height() as usize,
                pitch: f.pitch() as usize,
                bpp: f.bpp() as usize,
                red_shift: f.red_mask_shift(),
                green_shift: f.green_mask_shift(),
                blue_shift: f.blue_mask_shift(),
            });
        }
    }

    draw_banner();
    cprintln!(fb::ACCENT, "bramble: one graph, no hierarchy");
    println!("phase 0: boot, serial, framebuffer, descriptor tables");
    println!();

    cpu::init();
    percpu::init();
    syscall::init();
    println!("cpu:  gdt, tss, idt, per-cpu base and syscall entry ready");

    if let Some(hhdm) = HHDM.get_response() {
        println!("hhdm: physical memory mapped at {:#018x}", hhdm.offset());
    }

    if let Some(fbr) = FRAMEBUFFER.get_response() {
        if let Some(f) = fbr.framebuffers().next() {
            println!("fb:   {}x{} at {} bpp", f.width(), f.height(), f.bpp());
        }
    }

    if let Some(mm) = MEMORY_MAP.get_response() {
        let entries = mm.entries();
        let mut usable = 0u64;
        let mut total = 0u64;
        for e in entries {
            total += e.length;
            if e.entry_type == EntryType::USABLE {
                usable += e.length;
            }
        }
        println!();
        println!("memory map: {} entries, {} MiB usable of {} MiB", entries.len(), usable >> 20, total >> 20);
        for e in entries {
            println!(
                "  {:#014x}..{:#014x}  {:>9} KiB  {}",
                e.base,
                e.base + e.length,
                e.length >> 10,
                entry_type_name(e.entry_type)
            );
        }
    }

    if let Some(m) = MODULES.get_response() {
        println!();
        println!("modules: {}", m.modules().len());
        for f in m.modules() {
            println!("  {:?}  {} KiB", f.path(), f.size() >> 10);
        }
    }

    // The milestone's third leg: a deliberate fault must print a register dump
    // rather than triple-faulting the machine.
    let cmdline = CMDLINE.get_response().map(|c| c.cmdline()).unwrap_or(c"");
    println!();
    println!("cmdline: {:?}", cmdline);
    if contains(cmdline.to_bytes(), b"faulttest") {
        println!();
        cprintln!(fb::ALERT, "cmdline requested faulttest: executing ud2");
        // SAFETY: deliberately raising #UD to exercise the handler.
        unsafe { core::arch::asm!("ud2") };
    }

    // ---- phase 2: physical memory, then the graph itself ----

    let hhdm = HHDM.get_response().expect("bootloader gave no hhdm").offset();
    paging::set_hhdm(hhdm);
    let memmap = MEMORY_MAP.get_response().expect("bootloader gave no memory map");
    let modules: &[&limine::file::File] =
        MODULES.get_response().map(|m| m.modules()).unwrap_or(&[]);

    // SAFETY: the offset and the map come from the bootloader that loaded us.
    let allocator = unsafe { frames::FrameAllocator::new(memmap.entries(), hhdm) };
    let bitmap = allocator.bitmap_region();
    println!();
    println!(
        "frames: {} total, {} free, bitmap at {:#014x} ({} pages)",
        allocator.total_frames(),
        allocator.free_frames(),
        bitmap.phys,
        bitmap.pages
    );
    *state::FRAMES.lock() = Some(allocator);

    // Prove the allocator works before anything depends on it.
    {
        let mut fa = state::FRAMES.lock();
        let fa = fa.as_mut().expect("allocator");
        let before = fa.free_frames();
        let a = fa.alloc().expect("one frame");
        let b = fa.alloc_contiguous(4).expect("four contiguous frames");
        assert_eq!(fa.free_frames(), before - 5);
        fa.free_contiguous(a, 1);
        fa.free_contiguous(b, 4);
        assert_eq!(fa.free_frames(), before, "frame allocator leaked");
        println!(
            "frames: alloc/free round trip clean ({} used, {} free)",
            fa.used_frames(),
            fa.free_frames()
        );
    }

    state::populate(memmap.entries(), modules, bitmap, 0x3F8).expect("boot graph");
    println!("graph: boot nodes created");
    state::refresh_frame_counts();
    state::assert_consistent("boot");
    println!("graph: checker clean");
    println!();

    dump::dump_graph();

    // ---- phase 3: address spaces and the page-table invariant ----
    println!();
    selftest::address_spaces();
    println!();
    dump::dump_graph();

    // ---- phase 4: threads and preemption ----
    println!();
    cpu::init_interrupt_controller();
    selftest::threads_and_preemption();
    println!();
    dump::dump_graph();

    // ---- phase 5: userspace ----
    println!();
    selftest::userspace(modules);
    println!();
    dump::dump_graph();

    // ---- phase 6: ipc ----
    println!();
    selftest::ipc(modules);
    println!();
    dump::dump_graph();

    // ---- phase 7: lifecycle from userspace ----
    println!();
    selftest::lifecycle(modules);
    println!();
    dump::dump_graph();

    // ---- phase 8: v1 ----
    println!();
    selftest::v1(modules);
    println!();
    dump::dump_graph();

    // ---- phase 9: lazy mapping and the range index ----
    println!();
    selftest::lazy_mapping(modules);
    println!();
    dump::dump_graph();

    println!();
    selftest::shared_memory(modules);
    println!();
    dump::dump_graph();

    println!();
    cprintln!(fb::ACCENT, "phase 9 complete. halting.");
    halt_forever();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // SAFETY: this path never returns.
    unsafe {
        serial::force_unlock();
        fb::force_unlock();
    }
    cprintln!(fb::ALERT, "");
    cprintln!(fb::ALERT, "*** kernel panic ***");
    println!("{}", info);
    dump::dump_graph_best_effort();
    halt_forever();
}
