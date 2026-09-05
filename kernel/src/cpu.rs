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

pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub user_data: SegmentSelector,
    pub tss: SegmentSelector,
}

/// The task state segment is mutable after boot: `rsp0` is the stack the CPU
/// switches to when an interrupt arrives while ring 3 is running, so it has to
/// track whichever thread is current.
static mut TSS: TaskStateSegment = TaskStateSegment::new();
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();
static IDT: Once<InterruptDescriptorTable> = Once::new();

/// Point the CPU at the kernel stack to use for the next entry from ring 3.
///
/// # Safety-relevant invariant
/// Must be called on every switch to a thread that can reach user mode, before
/// that thread runs. Getting it wrong means an interrupt lands on the previous
/// thread's stack, which is silent corruption.
pub fn set_kernel_stack(top: u64) {
    // SAFETY: single core, and callers hold interrupts off. The reference does
    // not escape.
    unsafe {
        (&raw mut TSS).as_mut().expect("tss static").privilege_stack_table[0] =
            VirtAddr::new(top);
    }
    crate::percpu::set_kernel_rsp(top);
}

pub fn selectors() -> &'static Selectors {
    &GDT.get().expect("gdt is initialised").1
}

pub fn init() {
    // SAFETY: single core, called once before anything else can touch it.
    let tss: &'static TaskStateSegment = unsafe {
        (&raw mut TSS).as_mut().expect("tss static").interrupt_stack_table
            [DOUBLE_FAULT_IST_INDEX as usize] = {
            // A private static used only as a stack; taking its address is the
            // only way to hand the CPU a stack pointer.
            let start = VirtAddr::from_ptr(&raw const DF_STACK);
            start + (STACK_PAGES * 4096) as u64
        };
        (&raw const TSS).as_ref().expect("tss static")
    };

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
        idt[crate::time::IRQ_TIMER].set_handler_fn(timer_interrupt);
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

/// Remap the interrupt controller above the exception vectors and mask
/// everything. Separate from `init` so the IDT can be in place first.
pub fn init_interrupt_controller() {
    crate::time::init_pic();
}

fn dump(name: &str, frame: &InterruptStackFrame, code: Option<u64>) {
    // SAFETY: every caller halts or kills a process afterwards. Without this a
    // fault taken while printing deadlocks silently.
    unsafe {
        crate::serial::force_unlock();
        crate::fb::force_unlock();
    }
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

/// A page fault from ring 3 is the process's problem, not the kernel's.
///
/// Destroying it is one `begin_delete`: the address space, its page tables, its
/// memory and its threads are all owned by the process, so detaching one edge
/// makes the lot unreachable and the reaper returns it later. There is no
/// cleanup path to write, and that is the ownership tree earning its keep.
extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, code: PageFaultErrorCode) {
    let from_user = code.contains(PageFaultErrorCode::USER_MODE);
    let addr = x86_64::registers::control::Cr2::read_raw();

    // A fault inside a lazy mapping is not an error, it is the mapping being
    // realised. Only if the graph does not authorise this address does the
    // process die.
    if from_user
        && !code.contains(PageFaultErrorCode::PROTECTION_VIOLATION)
        && crate::vm::fault_in(addr)
    {
        return;
    }

    if from_user {
        // SAFETY: the faulting process is about to be destroyed.
        unsafe {
            crate::serial::force_unlock();
            crate::fb::force_unlock();
        }
        crate::cprintln!(crate::fb::ALERT, "");
        crate::cprintln!(
            crate::fb::ALERT,
            "*** killing a process: page fault at {:#x} from ring 3 ***",
            addr
        );
        crate::println!("  rip {:#018x}  cause {:?}", frame.instruction_pointer.as_u64(), code);
        crate::sched::exit_current_process(-11);
    }
    dump("page fault", &frame, Some(code.bits()));
    crate::println!("  cause: {:?}", code);
    crate::halt_forever();
}

/// The timer. Everything here is O(1) by design: count the tick, acknowledge
/// the controller, and only then consider switching. An interrupt handler that
/// did unbounded work under the graph lock would be the end of the locking
/// discipline (DESIGN 3.8 rule 4).
extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    crate::time::tick();
    crate::sched::on_tick();
    // Acknowledge before switching: the thread we switch to must be able to
    // receive the next tick.
    crate::time::eoi(crate::time::IRQ_TIMER);
    if crate::sched::take_need_resched() {
        crate::sched::schedule();
    }
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    dump("double fault", &frame, Some(code));
    crate::halt_forever();
}
