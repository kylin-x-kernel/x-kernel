# x86 S3 safety boundary

Firmware, FACS and the boot-selected AP page are trusted boot inputs. FACS
signature, minimum extent and alignment are checked before field access. The
installed assembly extent and every low-page patch are bounded. One ACTIVE
transaction owns the FACS fields, fixed context slots, IRQ snapshot and page
backup. CPU topology and device discovery/removal must remain stable.

Each CPU exclusively writes its context with IRQs and migration disabled.
Before sleep, NMI producers are quiesced. Through local APIC reconstruction,
the PM barrier retains IRQ exclusion and exclusive ownership of all LAPIC
sources; no ICR writer may span reset. The permanent APIC mapping, CPU-local
runtime handle and fixed error handler remain live across sleep. Wake assembly
restores APIC-base access and CPU-local storage before
`x86_apic::restore_local_after_reset` reconstructs masked timer/LINT state.
The BSP publishes the target context before SIPI and waits for its release/
acquire acknowledgment before changing the shared page. After wake, all APs
are first initialized together so unclaimed firmware APs cannot execute through
page reuse. Saved stacks and contexts outlive callback join; no low entry may
remain active when WakePage restores its backup.

INIT/SIPI operations return submission and completion failures through boot/PM
interfaces. A completion timeout does not cancel the command. Boot and recovery
owners must retain their trampoline, saved stacks and CPU barriers on failure;
they may not advance to another AP, reuse the shared wake page or call finish
until late delivery and every remote callback have been settled. The platform
returns the error without selecting the coordinator's fatal/recovery policy.

The coordinator independently owns producer freezing, device drain, DMA
exclusion, local event/clock tokens and RTC settlement. The platform hook does
not establish these by itself. Unknown firmware or missing S3 support refuses
before entry. Unexpected restoration failure is fatal; releasing frozen tasks
with unusable hardware is not a recovery strategy. Physical machines, CPU
hotplug, alternate firmware, AVX/extended context support, and firmware-specific
`_PTS`/`_WAK` methods are outside this QEMU backend's validated contract.

The test-only register-clobber wrapper retains its Rust caller registers on
its own stack. Its transfer callback never returns through the C ABI; it jumps
into the production restore entry with a live aligned SavedCpu. Tests exclude
IRQs/migration and retain both stacks/context until the wrapper returns. No
Rust frame observes the intentionally corrupted callee-saved values.
