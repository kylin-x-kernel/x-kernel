use kplat::memory::{PhysAddr, pa, p2v, va, v2p};
static mut SECONDARY_STACK_TOP: usize = 0;
const CPU_SPIN_TABLE: [PhysAddr; 4] = [pa!(0xd8), pa!(0xe0), pa!(0xe8), pa!(0xf0)];
#[unsafe(naked)]
unsafe extern "C" fn modify_stack_and_start() {
    core::arch::naked_asm!("
        ldr     x0, ={secondary_boot_stack}     // the secondary CPU hasn't set the TTBR1
        mov     x1, {phys_virt_offset}
        sub     x0, x0, x1                      // minus the offset to get the phys addr of the boot stack
        ldr     x0, [x0]                        // x0 will be set to SP in the beginning of _start_secondary
        b       {start_secondary}",
        secondary_boot_stack = sym SECONDARY_STACK_TOP,
        phys_virt_offset = const crate::config::plat::PHYS_VIRT_OFFSET,
        start_secondary = sym crate::boot::_start_secondary,
    );
}
pub fn start_secondary_cpu(cpu_id: usize, stack_top: PhysAddr) {
    let entry_paddr = v2p(va!(modify_stack_and_start as usize)).as_usize();
    let stack_top_ptr = &raw mut SECONDARY_STACK_TOP;
    unsafe { stack_top_ptr.write_volatile(stack_top.as_usize()) };
    axcpu::asm::flush_dcache_line(va!(stack_top_ptr as usize));
    let spintable_vaddr = p2v(CPU_SPIN_TABLE[cpu_id]);
    let release_ptr = spintable_vaddr.as_mut_ptr() as *mut usize;
    unsafe { release_ptr.write_volatile(entry_paddr) };
    axcpu::asm::flush_dcache_line(spintable_vaddr);
    aarch64_cpu::asm::sev();
}
