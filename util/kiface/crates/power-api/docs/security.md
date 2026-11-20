# kiface-power-api Security

The unsafe caller/provider boundary is `SystemSleepIf` in `src/lib.rs`. The x86 implementation lives in `kplat-x86_64/src/suspend.rs`.
The PM coordinator owns exclusion and lifetime across these calls.

- `prepare` runs on the boot CPU. Serialize PM and exclude CPU hotplug until
  `finish` releases the wake resources.
- `park_cpu` runs on the target CPU with local IRQs and migration disabled.
  These exclusions and usable devices are required through the resumed
  return of `SystemSleepIf::park_cpu`, including the abort path. Keep both
  the `extern "C"` callback and the storage addressed by its raw argument
  valid for that entire call. The callback returning or acknowledging alone
  does not release the caller's lifetime obligation.
- `enter` requires other CPUs, devices, NMI sources, IRQs and clocks to be
  quiesced before firmware entry on the boot CPU.
- `restore_cpus` requires boot CPU IRQs to remain disabled and shared resources
  to remain excluded while saved remote contexts restart.
- `finish` requires all remote CPU callbacks to have joined before releasing
  ownership. A timeout alone does not establish that join.

`persistent_time` is a safe optional clock read. The forwarding import
`khal::power::SystemSleepIf` exposes the same contract to existing callers.
