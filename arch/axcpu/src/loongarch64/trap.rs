use loongArch64::register::{
    badv,
    estat::{self, Exception, Trap},
};

use super::context::TrapFrame;
use crate::trap::PageFaultFlags;

core::arch::global_asm!(
    include_asm_macros!(),
    include_str!("trap.S"),
    trapframe_size = const (core::mem::size_of::<TrapFrame>()),
);

fn dispatch_irq_breakpoint(era: &mut usize) {
    debug!("Exception(Breakpoint) @ {era:#x} ");
    *era += 4;
}

fn dispatch_irq_page_fault(tf: &mut TrapFrame, access_flags: PageFaultFlags) {
    let vaddr = va!(badv::read().vaddr());
    if dispatch_irq_trap!(PAGE_FAULT, vaddr, access_flags) {
        return;
    }
    #[cfg(feature = "uspace")]
    if tf.fixup_exception() {
        return;
    }
    core::hint::cold_path();
    panic!(
        "Undispatch_irqd PLV0 Page Fault @ {:#x}, fault_vaddr={:#x} ({:?}):\n{:#x?}\n{}",
        tf.era,
        vaddr,
        access_flags,
        tf,
        tf.backtrace()
    );
}

#[unsafe(no_mangle)]
fn loongarch64_trap_handler(tf: &mut TrapFrame) {
    let _tf_guard = crate::TrapFrameGuard::new(tf);
    let estat = estat::read();

    match estat.cause() {
        Trap::Exception(Exception::LoadPageFault)
        | Trap::Exception(Exception::PageNonReadableFault) => {
            dispatch_irq_page_fault(tf, PageFaultFlags::READ)
        }
        Trap::Exception(Exception::StorePageFault)
        | Trap::Exception(Exception::PageModifyFault) => {
            dispatch_irq_page_fault(tf, PageFaultFlags::WRITE)
        }
        Trap::Exception(Exception::FetchPageFault)
        | Trap::Exception(Exception::PageNonExecutableFault) => {
            dispatch_irq_page_fault(tf, PageFaultFlags::EXECUTE);
        }
        Trap::Exception(Exception::Breakpoint) => dispatch_irq_breakpoint(&mut tf.era),
        Trap::Exception(Exception::AddressNotAligned) => unsafe {
            tf.emulate_unaligned().unwrap();
        },
        Trap::Interrupt(_) => {
            let interrupt_id: usize = estat.is().trailing_zeros() as usize;
            dispatch_irq_trap!(IRQ, interrupt_id);
        }
        trap => {
            panic!(
                "Undispatch_irqd trap {:?} @ {:#x}:\n{:#x?}\n{}",
                trap,
                tf.era,
                tf,
                tf.backtrace()
            );
        }
    }
}
