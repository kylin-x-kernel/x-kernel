# Ordinary IRQ Entry Validation

Use the repository build workflow and a disposable disk image. These checks
cover the entry mask instructions and normal boot, not firmware fault injection.

## Existing x86 And AArch64 Entry Contracts

Runtime's preparation observation checks that the entry mask is preserved;
it does not rely on scheduler initialization to establish that mask.

The x86 mask source depends on the loader path, not just the shared kernel
entry assembly:

- Direct Multiboot2 loading enters `_start32` in `arch/x86_64/multiboot.S`.
  `_start32`, `bsp_entry32` and `bsp_entry64` do not execute `cli`; they preserve
  the loader-supplied IF=0 required by the
  [Multiboot2 specification, section 3.3, i386 machine state](https://www.gnu.org/software/grub/manual/multiboot2/html_node/Machine-state.html).
- The repository LinuxBoot/UEFI loaders instead call
  `x86-boot-common::jump_to_kernel`, which executes `cli` before jumping to the
  loaded kernel's 64-bit entry. The Ticket05 x86 QEMU runtime test used
  `-kernel .../kernel.bzimg`, i.e. the LinuxBoot path, not direct Multiboot2
  loading. Compiling the shared `multiboot.S` does not identify the loader path.
- Secondary CPUs execute `cli` in `arch/x86_64/ap_start.S` before entering the
  common AP startup sequence.

On AArch64, the raw entry requires the Linux ARM64 boot protocol
state: `Documentation/arch/arm64/booting.rst`, "CPU mode", requires all DAIF
interrupt classes masked. The EL2/EL3-to-EL1 transition also explicitly installs
masked SPSR bits. Direct EL1 and VHE entry preserve the firmware contract rather
than adding a runtime mask. These source-level checks do not validate firmware
that violates the entry protocol.

## RISC-V

```sh
cp platforms/kplat-riscv64/qemu_defconfig .config
make defconfig
make UNITTEST=y clippy
make unittest UNITTEST_CRATE=irq_driver,kruntime XKMAKE_ARGS='--no-net --no-vsock'
rust-objdump -d --disassemble-symbols=_start,_start_secondary target/xkmake/kplat-riscv64/release/kernel.elf
```

Both entry symbols must begin with `csrci sstatus, 2`, before boot-stack setup
or Rust calls. The default configuration exercises four harts and runtime's
secondary-service/early-task startup-order tests. The IRQ-driver test checks
that later source activation preserves the global mask on its executing hart.

## LoongArch

```sh
cp platforms/kplat-loongarch64/qemu_defconfig .config
make defconfig
make UNITTEST=y clippy
make unittest UNITTEST_CRATE=kplat_loongarch64 XKMAKE_ARGS='--no-net --no-vsock'
rust-objdump -d --disassemble-symbols=_start,_start_secondary target/xkmake/kplat-loongarch64/release/kernel.elf
```

The primary's fixed header occupies `_start.._start+0x40`; do not interpret it
as executable instructions. At `_start+0x40`, and at `_start_secondary`, verify
zero is exchanged into CRMD using mask 4 before DMW, stack or Rust setup. The
exchange must not overwrite unrelated CRMD fields. The default configuration
is single-CPU: this run does not establish AP boot correctness; the secondary
sequence is checked in the generated code only.

For both targets, require the test summary, `COVERAGE_PERSISTED`, successful
coverage extraction and QEMU exit. Firmware normally supplies masked IRQs, so
passing these runs does not demonstrate an initially enabled pending interrupt
being suppressed. Real hardware, NMI and machine-mode delivery are not covered.
