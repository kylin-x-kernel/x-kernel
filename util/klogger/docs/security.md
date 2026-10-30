# klogger — Security and Reliability Analysis

## Trust model

```
kernel modules (trusted) ──logging macros──► klogger ──► LogSinkIf (console)
Kconfig LOG_LEVEL_* (build time) ──floor──► log::max_level (fixed at boot)
boot metadata (boot chain, semi-trusted) ──console spec──► set_filter_spec
privileged user ──/proc/sys/kernel/printk──► set_console_default
privileged user ──/proc/sys/kernel/log_filter──► set_filter_spec
```

`klogger` does not check privileges itself: the console-filter write paths are
gated by `procfs` (`cred().is_privileged()`). The module's own security goal is
that no input it is fed can corrupt its state or its memory.

## External boundary / attack surface

| Boundary | Source | Entry point |
|----------|--------|-------------|
| Boot metadata | `/chosen/x-kernel,log-level`, `loglevel=`/`quiet`/`debug`, Kconfig `LOG_FILTER` | `set_filter_spec` (via `kruntime`) |
| Build configuration | Kconfig `LOG_LEVEL_*` | `init_klogger`, fixes the floor |
| Privileged user input | `echo 4 > /proc/sys/kernel/printk` (level) | `set_console_default` (via `procfs`) |
| Privileged user input | `echo 'warn ksyscall=debug' > /proc/sys/kernel/log_filter` (spec) | `set_filter_spec` (via `procfs`) |
| Record body | formatted arguments of any kernel module | `Log::log` |
| Output channel | `kiface_logging_api::LogSinkIf` / `LogContextIf` providers | `print_fmt` |

The module handles no user pointers, MMIO, DMA, FFI or file-system data, and
contains no `unsafe` block.

## Unsafe inventory

None. All state manipulation is safe Rust: fixed arrays, modular indexing and
`SpinNoIrq` guards.

## Memory-safety invariants

1. `FilterRule.target_len <= MAX_FILTER_TARGET`, validated by `set_filter_spec`;
   `level_for` slices `&rule.target[..target_len]` and skips a rule whose bytes
   are not valid UTF-8 instead of panicking.
2. `FilterSpecWriter` copies only when `len + text.len() <= buf.len()` and
   returns `fmt::Error` otherwise, which `filter_spec` turns into `None`.

## Thread safety

- `FILTER` and the `print_fmt` lock are `SpinNoIrq`, so multi-CPU and
  interrupt-context access is safe; `Filter` is plain data.
- The locks are never nested: `Log::log` takes `FILTER` for the console decision
  and `print_fmt` takes the console lock after it was released.
- The `log` crate's ceiling is recomputed inside the same `FILTER` critical
  section that changes the filter, so the two cannot describe different states.

## Threat analysis

| ID | Threat | Severity | Trigger | Mitigation |
|----|--------|----------|---------|------------|
| T-01 | An invalid filter spec destroys logging availability (silent kernel) | Medium | Malformed spec from the device tree, the command line, a privileged write, or the compiled-in `LOG_FILTER` | The spec is validated as a whole before it is installed and a rejected one keeps the previous filter; every boot source reports its rejection through the unfiltered `kprintln!` path, `procfs` returns `VfsError::InvalidInput`, and empty specs are rejected as well |
| T-02 | A runtime spec raises the ceiling and makes hot-path sites pay | Low | `echo 'trace' > /proc/sys/kernel/log_filter`, or `loglevel=trace` on the command line | Raising is intentional — the sites are compiled in and the operator asked for them — but it is the only way the higher levels cost anything. The write is privilege-gated |
| T-03 | A log call from NMI context spins on `FILTER` held by the interrupted context | High | An NMI claims a hwirq with no registered handler while the same CPU is inside a `FILTER` holder, and the dispatch path logs that fact | `Log::log` asks `LogContextIf::in_nmi()` before taking any lock and drops the record; NMI/panic paths that need output use the platform's atomic console path. The unhandled-NMI log call that triggers this predates the filter, but the filter is never added to that path |
| T-05 | A privileged write is not the only path to the level | Medium | Non-privileged writes to the console nodes | `procfs` requires `cred().is_privileged()` on write; both nodes are owner-write, world-read |

## Failure mode and effects analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|----|--------------|-------|--------------|---------------|----------|------------|
| F-01 | The boot filter stays at the build default | DT, command line and Kconfig all invalid or missing | The console follows `LOG_LEVEL_*` | None | 4 | Every invalid source is reported through `kprintln!` and the resolver falls through to the next one — the command line keeps scanning past a rejected `loglevel=`, and a rejected DT or `LOG_FILTER` value lets the next layer apply — so the outcome is the documented build default, never silence |
| F-04 | The filter lock waits on the logging hot path | Logging from inside a `FILTER` critical section | Interrupts stay off while spinning | Potential hang | 1 | Critical sections only touch fixed-size data |
| F-07 | A configuration selects no log level | Kconfig choice left empty | `configured_log_level()` reports `error` | None | 4 | The choice has no `off` option, so an empty selection is a broken configuration: it resolves to `error` at the source, which makes the console default, the logger floor and the boot banner agree instead of leaving the console mute |

## Failure handling

- An invalid filter spec returns `FilterSpecError` and the previous filter keeps
  working. `kruntime` reports the rejection with `kprintln!` (unfiltered, so a low
  console level cannot hide the diagnostic), keeps scanning the command line past
  a rejected `loglevel=`, and falls through to the next boot source;
  `procfs`'s write path returns `VfsError::InvalidInput` to the writer.
- `init_klogger` panics if the global logger cannot be installed: that path has
  no error return and no recovery, so it is an explicit, documented exception.

## Privacy analysis

Record bodies may contain kernel addresses, module paths, task names and device
state; they appear on the kernel console, which is a physically reachable UART or
display. The module collects no user personal data and stores nothing.

## Known limitations

- There is no per-target *existence* control: a target can only be filtered once
  the record was produced, so the build level bounds what any spec can reveal.
- `filter_spec` needs a destination of `MAX_FILTER_SPEC` bytes and returns `None`
  when the caller's buffer is smaller.
- There is no rate limit: verbosity is bounded only by the build level and the
  spec.

## Audit checklist

- [ ] Does a new filter entry point reuse `set_filter_spec` instead of writing
      `FILTER` or `log::set_max_level` directly?
- [ ] Does the spec grammar still reject empty specs, over-long targets and too
      many rules, and install nothing on rejection?
- [ ] Is any new caller logging from inside a `FILTER` critical section?
