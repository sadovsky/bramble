//! Descriptor tables and exception handlers.
//!
//! Nothing here is graph-structured and nothing here should be: this is the
//! hardware's own format, and the kernel's job is to satisfy it exactly.

use spin::Once;
use x86_64::registers::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// Interrupt stack table slot for the double-fault handler, so a blown kernel
/// stack still lands somewhere it can print from.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

const STACK_PAGES: usize = 5; // 20 KiB
static mut DF_STACK: [u8; STACK_PAGES * 4096] = [0; STACK_PAGES * 4096];

struct Selectors {
    kernel_code: SegmentSelector,
    kernel_data: SegmentSelector,
    #[allow(dead_code)]
    user_code: SegmentSelector,
    #[allow(dead_code)]
    user_data: SegmentSelector,
    tss: SegmentSelector,
}

static TSS: Once<TaskStateSegment> = Once::new();
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();
static IDT: Once<InterruptDescriptorTable> = Once::new();

pub fn init() {
    let tss = TSS.call_once(|| {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            // SAFETY: a private static used only as a stack; taking its address
            // is the only way to hand the CPU a stack pointer.
            let start = VirtAddr::from_ptr(&raw const DF_STACK);
            start + (STACK_PAGES * 4096) as u64
        };
        tss
    });

    let (gdt, sel) = GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        // User segments are appended now, in the order SYSCALL/SYSRET expects,
        // so that phase 5 does not have to rebuild the table.
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(tss));
        (gdt, Selectors { kernel_code, kernel_data, user_code, user_data, tss: tss_sel })
    });

    gdt.load();
    // SAFETY: the selectors index the GDT we just loaded.
    unsafe {
        CS::set_reg(sel.kernel_code);
        DS::set_reg(sel.kernel_data);
        ES::set_reg(sel.kernel_data);
        SS::set_reg(sel.kernel_data);
        x86_64::instructions::tables::load_tss(sel.tss);
    }

    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();
        idt.divide_error.set_handler_fn(divide_error);
        idt.debug.set_handler_fn(debug);
        idt.breakpoint.set_handler_fn(breakpoint);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        idt.general_protection_fault.set_handler_fn(general_protection);
        idt.page_fault.set_handler_fn(page_fault);
        idt.stack_segment_fault.set_handler_fn(stack_segment);
        idt.invalid_tss.set_handler_fn(invalid_tss);
        idt.segment_not_present.set_handler_fn(segment_not_present);
        // SAFETY: index 0 of the IST is the double-fault stack set up above.
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault)
                .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        }
        idt
    });
    idt.load();
}

fn dump(name: &str, frame: &InterruptStackFrame, code: Option<u64>) {
    crate::cprintln!(crate::fb::ALERT, "");
    crate::cprintln!(crate::fb::ALERT, "*** exception: {} ***", name);
    if let Some(c) = code {
        crate::println!("  error code {:#018x}", c);
    }
    crate::println!("  rip {:#018x}   cs  {:#06x}", frame.instruction_pointer.as_u64(), frame.code_segment.0);
    crate::println!("  rsp {:#018x}   ss  {:#06x}", frame.stack_pointer.as_u64(), frame.stack_segment.0);
    crate::println!("  rflags {:#018x}", frame.cpu_flags.bits());
    crate::println!("  cr2 {:#018x}", x86_64::registers::control::Cr2::read_raw());
}

macro_rules! simple_handler {
    ($name:ident, $label:expr) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            dump($label, &frame, None);
            crate::halt_forever();
        }
    };
    ($name:ident, $label:expr, code) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, code: u64) {
            dump($label, &frame, Some(code));
            crate::halt_forever();
        }
    };
}

simple_handler!(divide_error, "divide error");
simple_handler!(debug, "debug");
simple_handler!(invalid_opcode, "invalid opcode");
simple_handler!(general_protection, "general protection fault", code);
simple_handler!(stack_segment, "stack segment fault", code);
simple_handler!(invalid_tss, "invalid tss", code);
simple_handler!(segment_not_present, "segment not present", code);

extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    dump("breakpoint", &frame, None);
    // Breakpoints are recoverable: this is the one handler that returns.
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, code: PageFaultErrorCode) {
    dump("page fault", &frame, Some(code.bits()));
    crate::println!("  cause: {:?}", code);
    crate::halt_forever();
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    dump("double fault", &frame, Some(code));
    crate::halt_forever();
}
