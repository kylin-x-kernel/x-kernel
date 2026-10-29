# klogger — Design

## Purpose

`klogger` is the X-Kernel logging core. It implements the `log` crate's `Log`
trait, formats records, and decides which of them reach the console.

Output goes through the `kiface_logging_api` providers: `LogSinkIf` writes
characters and `LogContextIf` supplies the time, CPU and task id. `kruntime`
links both providers before `init_klogger`. Subsystems depend on `klogger`'s
macros and filter API only, never on a concrete output channel.

## Background

A single global level cannot serve real work: `debug` is needed for one stack
while a global `debug` drowns the console in IPI and syscall lines. Two roles are
therefore kept apart:

| Role | Whose | What it decides | Runtime change |
|------|-------|-----------------|----------------|
| **Build floor** | `LOG_LEVEL_*` (Kconfig) | the console default and the lowest level the logger runs at; `log`'s filter is never below it | fixed at boot; a spec may raise the ceiling above it, never below |
| **Console filter** | `Filter` | which records are printed: a default level plus per-target overrides | yes — `/proc/sys/kernel/printk`, `/proc/sys/kernel/log_filter` |

The build level is what makes this affordable by default: with no spec asking for
more, a record above it is dropped by the `log` macro for the price of a
comparison. A runtime spec may raise the ceiling and reveal the higher-level sites
compiled into the image (`make run LOG=debug` on a `warn` image), and only then is
that work paid. It can never go the other way, so quietening the console never
takes the logger below the build level.

`log::max_level` is `max(build floor, console filter ceiling)`.

## Scope

```
util/klogger/
├── src/
│   └── lib.rs        (build floor, filter, macros, level parsing)
├── ut-tests/
│   └── lib.rs        (guest cases for the spec grammar and the ceiling)
├── docs/
│   ├── design.md
│   └── security.md
└── Cargo.toml
```

## Architecture

```
error!/warn!/info!/debug!/trace!
        │  (log crate: `level <= log::max_level()`, i.e. max(build floor, console ceiling))
        ▼
KernelLogger::log
        └── filter_enabled(target, level) ──► print_fmt ──► LogSinkIf::write_str
```

| Component | Responsibility |
|-----------|----------------|
| `Filter` | the build floor, the console default level, and up to `MAX_FILTER_RULES` `target=level` overrides; longest-prefix match |
| `FILTER` | the global filter, guarded by `SpinNoIrq` |
| `print_fmt` | serializes console output |

## Calling constraints / execution context

- Any logging macro may be called from any context: early boot (before
  `init_klogger` the `log` crate has no logger and the macro is a no-op),
  interrupt context, softirq, and ordinary task context.
- `FILTER` and the console lock are `SpinNoIrq`; their critical sections perform
  no user-memory access, no sleeping and no allocation.
- `set_filter_spec` is callable at runtime, from the
  `/proc/sys/kernel/log_filter` write path, and at boot from the command line
  (`kruntime::resolve_log_level`). It replaces the console rule set, and
  the default level when the spec carries one, in a single critical section; it
  never touches the build floor. `set_console_default` is the level-only knob
  behind `/proc/sys/kernel/printk` and leaves the rule set alone.
- `init_klogger(build_level)` runs once on the primary CPU during boot and fixes
  the floor.

## State machine

A spec is validated as a whole and installed as a whole; there are no partial
updates. Its rule set always replaces the previous one; its default level
replaces the previous default only when the spec has a bare level token, so
`ksyscall=trace` retargets one module instead of silently resetting the rest of
the kernel to a hard-coded level:

```
build floor (LOG_LEVEL_*), fixed by init_klogger
    └── console filter { default, rules[] }
            set_filter_spec(spec) ── validated ──► rules := spec rules
                                                 default := spec level, or kept
                                 └─ rejected ──► unchanged
            set_log_level_filter(level) ≡ set the default, drop every override
            set_console_default(level)  ≡ set the default, keep the overrides
```

## Algorithms

### Parsing a filter spec

A spec is `[<level>] [<target>=<level>]...`, with tokens separated by whitespace
or commas:

1. a token containing `=` splits into `target` and `level`; `level` must be
   `0..=5` or `off/error/warn/info/debug/trace`; `target` is trimmed, its
   redundant trailing `*` removed, and its length must be `1..=MAX_FILTER_TARGET`;
   at most `MAX_FILTER_RULES` rules are accepted;
2. a token without `=` sets the default level, the last one winning; a spec with
   no such token keeps the level that is already in force;
3. a spec with neither a rule nor a bare level is invalid;
4. only after the whole spec validates is it installed in one `FILTER` critical
   section. Any invalid token returns `FilterSpecError` and leaves the previous
   spec untouched.

`filter_spec(buf)` is the inverse: it writes the canonical text of the active
filter into the caller's buffer and returns the length, or `None` when the buffer
is too small. `/proc/sys/kernel/log_filter` and the boot banner share it, so the
buffer they pass is `MAX_FILTER_SPEC`, derived from the grammar
(`MAX_LEVEL_NAME + MAX_FILTER_RULES * (1 + MAX_FILTER_TARGET + 1 +
MAX_LEVEL_NAME)`) and therefore covers every spec `set_filter_spec` accepts — a
spec that could be installed but not reported back would make the world-readable
node fail with `InvalidInput` and hide the active overrides from the banner.

The public entry points and their directions:

| Entry point | Direction |
|-------------|-----------|
| `init_klogger(build_level)` | `kruntime` fixes the build level (console default and floor), once |
| `build_floor_from(level)` | parses the configured level; an empty choice resolves to `error` |
| `build_level_floor()` | reports the build level |
| `effective_ceiling()` | reports `max(build floor, console ceiling)`, i.e. what `log` admits |
| `set_filter_spec(spec)` | `procfs` (`/proc/sys/kernel/log_filter` write) and `kruntime` (boot) install a spec's rules plus its default level |
| `set_console_default(level)` | `procfs`'s `/proc/sys/kernel/printk` write: the console level only |
| `set_log_level_filter(level)` | set the console default and drop every override |
| `filter_spec(buf)` | `procfs` read side and the boot banner report the active spec |
| `current_log_level()` | reports the console filter's default level |

## Concurrency model

| Shared state | Synchronization | Notes |
|--------------|-----------------|-------|
| filter `FILTER` | `SpinNoIrq<Filter>` | read on the logging hot path; rule lookup is a bounded scan of a fixed array |
| console output | `SpinNoIrq<()>` inside `print_fmt` | keeps multi-CPU output from interleaving |
| `log` ceiling | published from the same `FILTER` critical section | `max(build floor, console ceiling)`, so filter and ceiling cannot disagree |

`SpinNoIrq` disables interrupts, so the hot path cannot deadlock against an
interrupt handler on the same lock, but it also means no critical section may
block, allocate, or log again. An NMI is the one context `SpinNoIrq` cannot
exclude: `Log::log` therefore drops a record raised in NMI context *before*
taking `FILTER` (`LogContextIf::in_nmi`), because the NMI could have interrupted a
same-CPU holder of the lock it is about to take.

## Design decisions

- **The build level is a floor, not a ceiling**: quietening cannot take the logger
  below it, while an explicit request can widen verbosity without a rebuild — the
  workflow `make run LOG=debug` relies on.
- **Spec instead of a single level**: `warn ksyscall=debug` retargets one stack
  and can be changed at runtime.
- **Fixed line**: a record is truncated at 255 bytes plus its terminator, and
  truncation stops at a character boundary.
- **X-Kernel level numbering** (`0..=5`, higher is more verbose): Linux's
  `SYSLOG_ACTION_CONSOLE_*` actions are deliberately not implemented because
  `console_loglevel` is the inverted domain.

## Drop / resource release

Nothing needs destructors: the filter is static storage and the installed logger
is `&'static`.
