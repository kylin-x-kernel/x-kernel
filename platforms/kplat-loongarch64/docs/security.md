# kplat-loongarch64 — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (module wiring and
default platform contracts), `src/init.rs` (boot handoff), `src/time.rs`
(stable counter), `src/irq.rs` plus `src/irq/eiointc.rs` and
`src/irq/pch_pic.rs` (interrupt hierarchy), `src/mp.rs` (SMP boot), and
`src/power.rs` (power control), plus guest tests in `ut-tests/irq.rs`,
`ut-tests/irq_eiointc.rs`, and `ut-tests/irq_pch_pic.rs`. No modules are excluded. Firmware
descriptions are consumed through `of` and validated there; the raw
IOCSR/MMIO accesses audited below are this crate's own boundary.

The controller tests use local recording backends without live IOCSR/MMIO
access. The non-SMP-only boot test reads ECFG without changing interrupt
state. All three test modules remain private and require `cfg(unittest)`;
the move adds no unsafe code or production trust boundary.

## Trust Model

Startup does not trust firmware's source masks: HWI1 is closed during
controller preparation, all EIOINTC enable banks are cleared, and both PCH
mask banks are set before the parent is reopened. Recording tests check the
mask-before-configuration sequence and both PCH bank offsets. Device activation
installs its HTVEC entry before unmasking. Pending state is retained; these
operations do not prove a device has stopped producing interrupts.

The device tree and the architectural registers are the two external
inputs. The DTB is structurally validated by `rs_fdtree`/`of`; the
values read from it (power-off register and byte, console node) are
trusted boot policy. Controller registers (EIOINTC IOCSR, PCH PIC MMIO)
are hardware state this crate owns exclusively after mapping. There is
no user-space input.

## External Boundaries

- **Device tree `syscon-poweroff` node**: yields the physical S5
  register address and the request byte. Checked properties: the node
  must carry a `regmap` phandle, `offset`, and `value`
  (`of::syscon_poweroff` returns `None` otherwise). Failure result: the
  terminal logs and halts instead of writing any register.
- **RTC MMIO aperture**: mapped from the build-time `RTC_PADDR` for
  `LS7A_RTC_SIZE` bytes; failure panics with the mapping error. The
  sampled value is validated by the `rtc_driver` LS7A backend (year
  consistency) before it reaches `ktime`.
- **Console device tree node**: parsed by `console_driver`; a parse
  failure panics in `early_driver_init` (a machine without a declared
  console cannot boot usefully).

## Unsafe Code

- `power.rs` `SleepControlRegister::write`: one volatile byte store to
  the GED sleep-control register mapped by `iomap_device` — the
  `SAFETY:` comment cites the exact one-byte mapping.
- `mp.rs`: CSR mailbox and IPI sends through `loongArch64` register
  helpers (safe wrappers over privileged instructions).
- `time.rs`: stable-counter reads through the `loongArch64` crate's
  safe register API.
- The EIOINTC/PCH PIC backends perform IOCSR/MMIO access through
  `loongArch64::iocsr` and `VirtAddr` pointers mapped at init; every
  public backend entry rejects out-of-range hwirq before any register
  access (`warn!` + return), so no access can compute an out-of-window
  offset.

## Protected Resources

- `PCH_PIC_BASE` (`LazyInit<VirtAddr>`): the 4 KiB PCH PIC aperture at
  the fixed platform address `0x1000_0000`; all PCH accesses are
  offset-checked against it.
- Each interrupt controller owns its register lock, protecting initialization
  and shared-word enable/disable RMW even when different IRQ lines are operated
  independently. `SpinNoIrq` excludes local IRQ/preemption and serializes CPUs
  in SMP builds. Locks do not nest across controllers or span kirq callbacks.
  NMI control is unsupported. EIO ISR completion remains a single W1C write,
  independent of the configuration lock; it must not become an ISR RMW.
- `NANOS_PER_TICK` (`LazyInit<u64>`): the counter-frequency conversion,
  computed once in `early_init`.
- The GED sleep-control mapping: one byte, mapped only in the
  `power_off` terminal path.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Malformed `syscon-poweroff` node | Medium — power-off writes a wrong register | Corrupt or hostile DTB | `of::syscon_poweroff` requires a resolvable regmap phandle and offset/value properties; absence degrades to halt. Residual risk: a valid-but-lying declaration is indistinguishable from firmware truth. |
| T-02 | Out-of-range external hwirq | Medium — writes into an unrelated controller vector | Bad IRQ number from above | Range gate in `IntrManagerIf::enable` against `PCH_PIC_IRQ_COUNT` plus backend-level re-checks (`eiointc`/`pch_pic` reject before access). |
| T-03 | Invalid RTC sample (torn year) | Low — wall clock not initialized | LS7A TOY counters straddling new-year | `rtc_driver` LS7A backend re-samples until the year halves agree and rejects persistent instability; `early_driver_init` propagates the failure as a panic. |
| T-04 | Spurious external IRQ line | Low — wasted IRQ cycle | Noise on a PCH PIC input | EIOINTC claim returns `None`; the dispatcher logs `"Spurious external IRQ"` and returns without dispatching. |
| T-05 | `boot_ap` for an unmapped logical CPU | Medium — AP boots with a wrong context | CPU-id table missing an entry | Named panic in the `CpuStartIf::boot_ap` provider before any mailbox write. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | RTC aperture mapping fails | Panic in `early_driver_init` | Boot fails | 2 | A machine whose RTC cannot be mapped cannot establish wall time. |
| F-02 | No `syscon-poweroff` declaration, or its register cannot be mapped | Warn + halt on power-off | Power stays on; system halted | 3 | Documented degradation path. |
| F-03 | Console node parse failure | Panic in `early_driver_init` | Boot fails | 2 | A machine without a usable console aborts rather than running headless silently. |
| F-04 | `notify_cpu` / `set_prio` called | `todo!()` panic | Offending caller aborts | 2 | Unimplemented capability (see Known Limitations); surfaced loudly instead of misrouting. |

## Known Limitations

- `IntrManagerIf::notify_cpu` and `set_prio` are `todo!()`
  placeholders: interrupt steering and priority routing are not
  implemented for this platform.
- `suspend_to_ram` is unsupported (`KError::OperationNotSupported`);
  the GED register could express S3 but no wakeup path exists.
- Only the QEMU virt-class EIOINTC + PCH PIC hierarchy is supported;
  other LoongArch interrupt topologies are out of scope.
- Locking does not establish remote-node IOCSR addressing. The current
  single-router/default-UP coverage does not validate AP control or multi-node
  register targeting; those need explicit routing and CPU/node ownership.

## Audit Checklist

- CPU0's HWI1 parent gate is enabled after EIOINTC/PCH PIC initialization;
  otherwise pending device interrupts cannot complete blocked I/O. Device
  registration still controls the downstream per-device masks.
- Backend hwirq range gates remain ahead of any IOCSR/MMIO access.
- Register-transaction locks cover the entire RMW, not separate reads/writes;
  retain lock order from IRQ line state to the controller and never the reverse.
- The RTC mapping size and address still come from `RTC_PADDR` /
  `LS7A_RTC_SIZE`, and the sample still flows through
  `rtc_driver::read` before `ktime`.
- Power-off still reads its register and byte from the firmware
  declaration rather than machine constants.
- Any new `kiface` provider method either has a real implementation or
  a recorded limitation here, not a silent `todo!()`.

## Local event restore boundary

The restore hook uses existing TCFG and kirq accessors on the initialized owning
CPU, with IRQs masked and competing programming quiesced. Controller access and
routing must already be restored. The hook leaves TCFG disabled, does not reset
the stable counter or conversion ratio, and does not deliver timer callbacks.
Final deadline arbitration remains a separate PM-ordered task-service operation.
Real suspend or lost-controller-state recovery is not established by unit tests.
