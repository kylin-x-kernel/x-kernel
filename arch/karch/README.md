# karch

Lightweight architecture-specific low-level operations for the x-kernel project.

This crate provides a uniform API across all supported architectures (AArch64, x86_64, RISC-V, LoongArch64, ARM) for:

- **TLB flush**: `flush_tlb(vaddr: Option<VirtAddr>)`
- **Cache maintenance** (AArch64): `flush_icache_all()`,
  `clean_dcache_line_to_poc(vaddr)`, `clean_dcache_range_to_poc(start, size)`
- **DMA ordering**: `dma_read_barrier()` — orders prior CPU stores before a
  device reads the same memory via DMA (`dbar 0` on LoongArch64, no-op on
  cache-coherent architectures)
- **CPU control**: `stop_cpu() -> !` (terminal halt; never returns),
  `await_interrupts()`
- **Local interrupt management**:
  - `enable_local_irq()`, `disable_local_irq()`, `local_irq_enabled()`
  - `save_irq_and_disable() -> usize` — atomically save interrupt state and disable local interrupts
  - `restore_irq(flags: usize)` — restore previously saved interrupt state
- **Thread pointer (TLS)**: `read_thread_pointer()`, `write_thread_pointer(val)`
- **FP/SIMD enable** (AArch64, LoongArch64): `enable_fp()`
- **LSX extension** (LoongArch64): `enable_lsx()`
- **MMU / Page table root**:
  - `read_kernel_page_table() -> PhysAddr`
  - `read_user_page_table() -> PhysAddr`
  - `unsafe fn write_kernel_page_table(root_paddr: PhysAddr)`
  - `unsafe fn write_user_page_table(root_paddr: PhysAddr)`
- **Trap / exception vector** (AArch64, RISC-V, LoongArch64):
  - `unsafe fn write_trap_vector_base(addr: usize)`
- **Hypercall** (x86_64): `fn hypercall(nr: u64, a0: u64, a1: u64) -> i64`
- **Page walk controller** (LoongArch64): `unsafe fn write_pwc(pwcl: u32, pwch: u32)`

## Deprecated names

The following names are deprecated and will be removed in a future release. Use the replacements shown:

| Deprecated | Replacement |
|---|---|
| `enable_irq()` | `enable_local_irq()` |
| `disable_irq()` | `disable_local_irq()` |
| `irq_enabled()` | `local_irq_enabled()` |

## Design

`karch` is intentionally kept lightweight: it only depends on `memaddr`, `cfg-if`, and
architecture-specific register libraries (`aarch64-cpu`, `x86`/`x86_64`, `riscv`,
`loongArch64`). It has **no** OS-level dependencies, making it suitable as a low-level
building block for other crates.

## Features

- `arm-el2`: Enable AArch64 EL2 (hypervisor) variants of TLB flush, page table root, and trap vector operations.
- `smp`: Delegate remote instruction-cache flushes to the `kipi` provider. Without
  this feature, `karch` installs a uniprocessor no-op provider for remote flushes.
