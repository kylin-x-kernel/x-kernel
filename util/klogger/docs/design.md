# klogger — Design

## Purpose

`klogger` is the X-Kernel logging core. It implements the `log` crate's `Log`
trait and routes every record to two consumers:

- **the console**, filtered by a runtime filter (default level plus per-target
  overrides);
- **the kernel log ring buffer** (`LOGBUF`), which records everything that
  reaches the logger and is read back by `syslog(2)` / `dmesg`.

Console output goes through the `kiface_logging_api` providers: `LogSinkIf`
writes characters and `LogContextIf` supplies the time, CPU and task id.
`kruntime` links both providers before `init_klogger`. Subsystems depend on
`klogger`'s macros and filter API only, never on a concrete output channel.

## Background

A single global level cannot serve real work: `debug` is needed for one stack
while a global `debug` drowns the console in IPI and syscall lines, and lowering
the level afterwards used to leave no way to read back what happened (`syslog(2)`
was a stub returning 0).

Three decisions are therefore kept separate, the way Linux separates its console
level from its printk ring:

| Role | Whose | What it decides | Can it change at runtime? |
|------|-------|-----------------|---------------------------|
| **Build floor** | `LOG_LEVEL_*` (Kconfig) | the console default and the lowest level the logger runs at; `log`'s filter is never below it | the floor is fixed at boot; a runtime spec may raise the ceiling above it, never below |
| **Capture** | `LOGBUF` | which records are stored for `dmesg`: everything up to the effective ceiling | no separate knob |
| **Console** | `Filter` | which records are printed: a default level plus per-target overrides | yes, `/proc/sys/kernel/printk`, `/proc/sys/kernel/log_filter` |

The build level is what makes the feature affordable *by default*: with no spec
asking for more, a record above it is dropped by the `log` macro for the price of
a comparison, so a `warn` production image pays nothing for the `debug!`/`trace!`
sites compiled into it. A runtime request — a `log_filter` write, or whatever boot
source the platform wires up — may raise the ceiling and reveal them, and only
then is the higher-level work paid. It can never go the other way: quietening the
console does not take the logger below the build level, which is what keeps the
ring complete.

`log::max_level` is therefore `max(build level, console filter ceiling)`, published
in the same critical section as the filter it belongs to.

Because capture is independent of the console filter, a quiet console no longer
implies a quiet ring: `LOG_LEVEL_DEBUG` plus a console default of `warn` (for
example `quiet` or `loglevel=2` on the command line) prints warnings while
`dmesg` still holds every `debug` record.

## Scope

```
util/klogger/
├── src/
│   └── lib.rs        (build floor, filter, ring buffer, macros, level parsing)
├── ut-tests/
│   └── lib.rs        (guest cases for the spec grammar and the ring semantics)
├── docs/
│   ├── design.md
│   └── security.md
└── Cargo.toml
```

## Architecture

```
error!/warn!/info!/debug!/trace!
        │  (log crate: `level <= log::max_level()` == max(build floor, console ceiling))
        ▼
KernelLogger::log
        ├── 1. LineBuf assembles "<N>[time cpu:tid target:line] args\n"
        │       └──► LOGBUF (64 KiB ring, overwrite oldest) ──► syslog(2)/dmesg
        └── 2. filter_enabled(target, level)
                ├── yes ──► print_fmt ──► LogSinkIf::write_str
                └── no  ──► drop (the record is already in the ring)
```

| Component | Responsibility |
|-----------|----------------|
| `Filter` | the build floor, the console default level, and up to `MAX_FILTER_RULES` `target=level` overrides; longest-prefix match |
| `FILTER` | the global filter (cap + console filter), guarded by `SpinNoIrq` |
| ring `LOGBUF` | fixed 64 KiB byte ring plus a sequence-indexed slot array; append reserves a range with `fetch_add`, reads validate each record against its slot instead of taking a lock |
| `LOGBUF_TAIL`/`LOGBUF_SEQ` | the reservation watermark and the newest published sequence number; both are plain atomics |
| `LOGBUF_CLEARED` | clear watermark: records at or below it are invisible, which is how `clear` drops everything without zeroing bodies a reader may be copying |
| `LineBuf` | fixed 256-byte line writer; the last byte is reserved for the terminator and truncation never splits a character |
| `print_fmt` | serializes console output |

## Calling constraints / execution context

- Any logging macro may be called from any context: early boot (before
  `init_klogger` the `log` crate has no logger and the macro is a no-op),
  interrupt context, softirq, and ordinary task context.
- `FILTER` and the console lock are `SpinNoIrq`: their critical sections perform
  no user-memory access, no sleeping and no allocation. The ring is not a lock at
  all — append and read use atomics only, so the record path never disables
  interrupts and never waits for another CPU. `LineBuf` and the ring are
  fixed-size, so the record path never allocates.
- User copies for `syslog(2)` happen **outside** `klogger`: `ksyscall` allocates
  a kernel buffer, calls `logbuf_read_newest`, and only then writes to user space.
- `set_filter_spec` is callable at runtime, from the
  `/proc/sys/kernel/log_filter` write path, and at boot from the command line
  (`kruntime`). It replaces the console rule set, and
  the default level when the spec carries one, in a single critical section; it
  never touches the build floor. `set_console_default` is the level-only knob
  behind `/proc/sys/kernel/printk` and leaves the rule set alone.
- `init_klogger(build_level)` runs once on the primary CPU during boot and fixes
  the cap.

## State machine

A spec is validated as a whole and installed as a whole; there are no partial
updates. Its rule set always replaces the previous one; its default level
replaces the previous default only when the spec has a bare level token, so
`ksyscall=trace` retargets one module instead of silently resetting the rest of
the kernel to a hard-coded level:

```
build floor (LOG_LEVEL_*), fixed by init_klogger
    │
    ├── LOGBUF: records everything up to the cap; only `logbuf_clear` removes
    │
    └── console filter { default, rules[] }
            set_filter_spec(spec) ── validated ──► rules := spec rules
                                                 default := spec level, or kept
                                 └─ rejected ──► unchanged
            set_log_level_filter(level) ≡ set the default, drop every override
```

## Algorithms

### Handling one record

1. The `log` crate applies `log::max_level()`, i.e. `max(build floor, console
   ceiling)`. Records above it never reach the logger.
2. `Log::log` formats `<N>[time cpu:tid target:line] args` into a `LineBuf`.
   `<N>` is X-Kernel's own level number (`1..=5`, error..trace).
3. The terminating newline is appended by `LineBuf::terminate`, not by
   `writeln!`: the writer reserves one byte, so a record that fills the buffer is
   truncated to 255 bytes and still ends with `\n`. Truncation also stops at a
   character boundary, so the ring never holds a split code point.
4. The whole line is pushed into the ring as one record: the body range comes
   from a single `fetch_add` on the tail watermark (so writers on different CPUs
   never wait for each other), the body is copied, and only then is the record
   published by storing its slot's sequence number with `Release`. Records are
   the unit of storage, so the oldest are evicted a whole record at a time and
   every boundary a reader sees is real.
5. Only then does `filter_enabled(target, level)` decide whether the record also
   goes to the console. A dropped record still stays in the ring — that is what
   makes a quiet console and a complete log coexist.

### Reading and clearing the ring

`logbuf_read_newest(out)` copies the **newest whole records** that fit, oldest
first. Records are the unit of storage: the ring keeps a small index of record
lengths (bodies only — no framing bytes in the text `dmesg` prints), so the
reader picks the window from real boundaries and never has to guess one by
scanning for a newline, which a message body may legitimately contain. When even
the newest record does not fit, nothing is returned — the same outcome Linux's
`syslog_print_all` produces, and never half a record. With a destination smaller
than the ring, older records are skipped rather than returning a prefix of the
log.

The ring has no lock, so a writer can wrap onto the window while a reader copies
it. Both sides use the same three facts to stay correct:

1. **Publication is ordered.** A slot's sequence number is stored with `Release`
   after the body and the slot's location, and read with `Acquire`; seeing the
   sequence number therefore means the record is complete.
2. **Identity is the sequence number, not the position.** Writers reserve bodies
   with `fetch_add`, so byte order and publication order may differ. The reader
   walks sequence numbers, and each record is located through its own slot.
3. **Overwriting is detectable.** A body at `at` is intact while the tail
   watermark is below `at + LOGBUF_SIZE`. The reader copies newest first and
   re-checks each record against the tail, so a wrap costs the oldest records:
   the read returns the surviving suffix of the window instead of an empty
   result, and only a window whose newest record is already gone is retried.

Reading never consumes. `logbuf_clear` drops everything, while
`logbuf_read_newest_seq` also reports the sequence number of the newest record it
copied; `syslog(2)`'s `READ_CLEAR` passes it to `logbuf_clear_through`, which
drops every record at or below that watermark. Records logged after the read
snapshot — including those logged while the reader was copying — therefore stay
buffered, which is Linux's `clear_seq` behaviour.

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

A level above the build level raises the ceiling: the higher-level sites are
compiled in, so the request takes effect immediately (that is how
`make run LOG=debug` reveals `debug!` output on a `warn` image) and the cost of
those sites is paid from then on.

`filter_spec(buf)` is the inverse: it writes the canonical text of the active
filter into the caller's buffer and returns the length, or `None` when the buffer
is too small. `/proc/sys/kernel/printk` and the boot banner share it, so the
buffer they pass is `MAX_FILTER_SPEC`, which is derived from the grammar
(`MAX_LEVEL_NAME + MAX_FILTER_RULES * (1 + MAX_FILTER_TARGET + 1 +
MAX_LEVEL_NAME)`) and therefore covers every spec `set_filter_spec` accepts — a
spec that could be installed but not reported back would make the world-readable
node fail with `InvalidInput` and hide the active overrides from the boot banner.

The public entry points and their directions:

| Entry point | Direction |
|-------------|-----------|
| `init_klogger(build_level)` | `kruntime` fixes the build level (console default and floor) from `LOG_LEVEL_*`, once |
| `build_level_floor()` | reports the build level (the console default and the floor) |
| `effective_ceiling()` | reports `max(build level, console ceiling)`, i.e. what `log` currently admits |
| `set_filter_spec(spec)` | `procfs` (`/proc/sys/kernel/log_filter` write) and the boot path install a spec's rules, plus its default level when it has one; a rejected spec leaves the previous filter untouched |
| `set_console_default(level)` | `procfs`'s `/proc/sys/kernel/printk` write: the console level only, keeping the rule set |
| `set_log_level(spec)` | convenience wrapper that applies a spec and ignores a rejection; used by callers that only have a level string |
| `set_log_level_filter(level)` | set the console default and drop every override (`procfs` tests and internal resets) |
| `filter_spec(buf)` | `procfs` read side and the boot banner report the active spec |
| `current_log_level()` | reports the console filter's **default** level |
| `logbuf_len` / `logbuf_read_newest` / `logbuf_read_newest_seq` / `logbuf_clear_through` / `logbuf_clear` | `ksyscall`'s `syslog(2)` read path and the unit tests |

The two runtime nodes are deliberately separate, so neither has to carry the
other's syntax: `/proc/sys/kernel/printk` keeps its historical single-value shape
(a level number, X-Kernel's domain) and `/proc/sys/kernel/log_filter` carries the
spec text (`warn ksyscall=debug`). A level write does not disturb the module
filter, the way Linux's `console_loglevel` leaves dynamic debug alone.

## Concurrency model

| Shared state | Synchronization | Notes |
|--------------|-----------------|-------|
| filter `FILTER` | `SpinNoIrq<Filter>` | read on the logging hot path; rule lookup is a bounded scan of a fixed array |
| ring `LOGBUF` | atomics only (no lock) | append reserves with `fetch_add` and publishes a slot; a read validates slots and the tail watermark instead of excluding writers |
| console output | `SpinNoIrq<()>` inside `print_fmt` | keeps multi-CPU output from interleaving |
| `log` ceiling | `SpinNoIrq<Filter>` (same lock as the filter) | recomputed as `max(build floor, console ceiling)` whenever the filter changes, in the same critical section, so the two cannot disagree |

`SpinNoIrq` disables interrupts, so the hot path cannot deadlock against an
interrupt handler on the same lock, but it also means no critical section may
block, allocate, or log again. An NMI is the one context `SpinNoIrq` cannot
exclude: `Log::log` therefore drops a record raised in NMI context *before*
taking `FILTER` (`LogContextIf::in_nmi`), because the NMI could have interrupted a
same-CPU holder of the lock it is about to take. A CPU that is already inside
`log()` drops nested records through a per-CPU guard instead of deadlocking on a
console sink that logs. `LineText` formatting happens before the ring is
touched, the ring itself takes no lock, and `print_fmt`'s lock is never held
together with a ring append or read.

## Design decisions

- **Capture is not derived from the console filter**: the console decides what is
  shown, the ceiling (build floor or a raised console request) decides what can be produced, and the ring stores
  everything the latter allows. This is what makes "quiet console, complete log"
  true instead of aspirational.
- **The build level is a floor, not a ceiling**: quietening (or a `quiet` boot)
  cannot starve the ring, while an explicit request can still widen verbosity
  without a rebuild — the workflow `make run LOG=debug` relies on. The extra cost
  of a raised ceiling is paid only when it is asked for.
- **Spec instead of a single level**: `warn ksyscall=debug` retargets one stack
  and can be changed at runtime without rebuilding.
- **Reading returns the newest whole records** (Linux's `syslog_print_all`
  shape): a short destination gets the most recent records, not the oldest ones,
  and never a partial line.
- **Fixed ring and fixed line**: the record path must not allocate, and a runaway
  message must not lose its interesting prefix; a record is truncated at 255
  bytes plus its terminator.
- **X-Kernel level numbering** (`0..=5`, higher is more verbose): Linux's
  `SYSLOG_ACTION_CONSOLE_*` actions are deliberately not implemented because
  `console_loglevel` is the inverted domain and mixing the two would mislead
  callers.

## Drop / resource release

Nothing needs destructors: the filter and the ring are static storage and the
installed logger is `&'static`. The closest operation to a release is
`logbuf_clear`, which resets the indices without wiping the bytes; they disappear
as later records overwrite them.