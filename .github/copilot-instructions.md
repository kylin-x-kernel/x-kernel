## Repository focus
- X-Kernel is a multi-architecture Rust OS/kernel tree with platform glue under `platforms/`, architecture HAL code under `arch/`, runtime/bootstrap under `boot/` and `core/kruntime/`, and major subsystems under `drivers/`, `mm/`, `fs/`, `net/`, `process/`, and `tee/`.
- Prefer changing the shared driver/HAL layer before adding platform-local forks. Platform crates should mostly provide `BootHandler` glue, firmware/static discovery, and board-specific fallback values.

## Build and validation
- Always copy a platform defconfig to `.config` and then run `make defconfig` before build, run, lint, or unit-test commands. Common entries are `platforms/kplat-aarch64/qemu_defconfig`, `platforms/kplat-aarch64/qemu_crosvm_defconfig`, `platforms/kplat-x86_64/qemu_defconfig`, and `platforms/kplat-x86_64/qemu_csv_defconfig`.
- Main commands:
  - `make build`
  - `make run`
  - `make UNITTEST=y run`
  - `make clippy`
  - `cargo +nightly-2026-03-08 fmt --all`
- `make build`, `make run`, `make clippy`, and unit-test Make targets require a prepared `.config`; copying a defconfig without running `make defconfig` is not sufficient.
- x86 image generation depends on LinuxBoot/UEFI helpers and host tools such as `x86_64-linux-musl-gcc`, `rust-objcopy`, `mtools`, `dosfstools`, `xz`, and `python3`.
- QEMU-based validation is the normal path for the qemu machines (aarch64, x86_64, riscv64) and QEMU defconfig feature variants such as x86_64 CSV.

## Architecture and ownership
- `boot/` contains early boot code and boot protocol handoff. `boot/x86-boot-common` and `boot/x86_64-boot-stub` handle x86 early boot; `boot/kernel-boot` handles common boot handoff.
- `core/kruntime` is the runtime entry that brings up memory management, registers boot-time runtime mappings, then calls platform/driver early init.
- `arch/khal` owns cross-architecture kernel HAL contracts such as `khal::irq`, `khal::time`, `khal::firmware`, and memory helpers.
- `drivers/` owns concrete hardware drivers and shared subsystems. If a capability already exists in `drivers/console`, `drivers/timer`, `drivers/irq`, `drivers/pci`, or `drivers/rtc`, extend it there instead of reintroducing platform-local copies.
- `mm/memspace` owns runtime device mappings and fixed device regions. Do not reintroduce the removed `memaddr` iomap layer for runtime mappings.

## IRQ, timer, and console rules
- Route interrupt description and wakeup logic through `khal::irq`. Reuse descriptor/domain helpers (`gic_*`, `plic_*`, `io_apic_*`) and generic wakeup registration instead of platform-local IRQ hook schemes.
- For x86, APIC/IOAPIC discovery is firmware-driven. Use `x86_apic::init_from_firmware()` when firmware discovery is intended.
- For x86 local timers, the APIC timer vector is an internal x86/APIC choice, not a platform config input. x86 platforms should only provide a nominal frequency hint; they should not add back a platform `TIMER_IRQ` setting.
- For AArch64 and RISC-V timers, follow the existing split between firmware/device-tree discovery and platform-static fallback already used by the touched platform.
- Console input IRQ handlers should only ack/wake the consumer path. Do not buffer console bytes in IRQ context unless the existing console path explicitly requires it.
- x86 boot/runtime console uses ioport; MMIO boot-console runtime re-registration only applies to MMIO transports.

## Change strategy
- Preserve the current layering:
  - firmware description -> `khal`
  - shared hardware logic -> `drivers`
  - board/platform glue -> `platforms`
- Prefer Rust-idiomatic design over C-style or ad-hoc procedural expansion. Model invariants with types, enums, traits, and ownership rather than extra boolean flags, sentinel integers, or loosely-related helper functions.
- When touching memory or boot mappings, keep boot-only mappings and runtime `memspace` mappings conceptually separate.
- When touching x86 timer/APIC code, keep the distinction between:
  - firmware-provided controller topology/resources
  - kernel-chosen local timer vector
  - runtime-detected or fallback CPU/TSC frequency
- Follow existing naming and helper patterns instead of adding parallel abstractions.

## Practical review checklist
- Check the relevant platform `defconfig` before changing platform boot/init code.
- If a change touches boot, IRQ, timer, console, PCI, or memory-management code, inspect the neighboring platform implementation for the other architectures before introducing a one-off behavior.
- Trust these instructions first; search only when the local subsystem behavior is still unclear or the touched code path has recently diverged.
