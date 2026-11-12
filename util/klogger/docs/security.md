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
gated by `procfs`, whose 0644 node requires VFS write access and whose write
closure additionally requires the opened file credential's effective UID to be
zero (Linux `test_perm` sysctl semantics). The module's own security goal is
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

None. All state manipulation is safe Rust: bounded fixed arrays and
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
| T-05 | A privileged write is not the only path to the level | Medium | Non-privileged writes to the console nodes | `procfs` requires VFS write access and effective UID zero on the opened file credential; both nodes are owner-write, world-read, and the write closure rejects other writers with `EPERM` |

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

## Kernel log ring and `syslog(2)`

Once capture exists the durable copy of the log is the in-memory ring, so the ring
and the read syscall add their own external boundary, failure modes and limits.

### External boundary

| Boundary | Source | Entry point |
|----------|--------|-------------|
| Unprivileged user read | `syslog(2)` `READ_ALL` / `SIZE_BUFFER` | `sys_syslog` |
| Privileged user read | `syslog(2)` `READ_CLEAR` / `SIZE_UNREAD`, `CLEAR` | `sys_syslog` (`CAP_SYSLOG`) |
| Kernel write | every `log` macro invocation | `KernelLogger::log` → `LogBuf` |

### Threat analysis

| ID | Threat | Severity | Trigger | Mitigation |
|----|--------|----------|---------|------------|
| T-04 | A log flood overwrites boot evidence | Medium | More than 64 KiB of records | The ring overwrites the oldest bytes and reads return the newest records, so a full ring keeps the recent past; no rate limit (see Known limitations) |
| T-06 | Unauthorized reads leak kernel addresses and paths | Medium | An unprivileged `syslog(2)` call | `READ_ALL`/`SIZE_BUFFER` are open to every caller (Linux's default with `dmesg_restrict` off) so `dmesg` works for users; clearing, read-and-clear and the unread count require `CAP_SYSLOG`, as in Linux. A `dmesg_restrict` equivalent is not implemented (known limitation) |
| T-07 | An over-long record loses its prefix or corrupts line boundaries | Low | A module prints a very long message | Truncation keeps the `<N>` prefix, reserves one byte for the terminator (so the record always ends with `\n`), and stops at a character boundary |
| T-08 | A read starts inside a record or a multi-byte character, so `dmesg` prints invalid UTF-8 | Low | A ring wrap, a short destination, or a record body containing a newline | The window is chosen from the record index, so it always starts at a record boundary; records are character-aligned, and a destination smaller than the newest record returns nothing instead of a fragment |
| T-09 | A record body forges an `<n>` prefix and confuses `dmesg` filtering | Low | A module prints a message starting with `<4>` | The prefix is generated by the logger; `dmesg` only parses it at line start |
| T-10 | A nested record deadlocks on the logger's locks | Medium | A console sink that logs, or a fault inside the logger, while the same CPU is inside `log()` | A per-CPU guard drops the nested record instead of taking `SpinNoIrq` twice; the ring and the console format lock are never held together, so a sink cannot re-enter a held lock |
| T-11 | A read holds a lock while copying 64 KiB | Low | A `dmesg` read with a full-size destination | There is no read lock: the copy takes atomics only, so a `dmesg` read never disables interrupts and never blocks a writer. The reader validates each record it copies and restarts at most `LOGBUF_READ_RETRIES` times |
| T-12 | `READ_CLEAR` destroys records the caller never received | Low | A destination smaller than the ring | This matches Linux: `syslog_print_all` skips the older records that do not fit and advances its clear cursor past them, so `dmesg -c` clears what it drained. The cursor is the sequence number the read stopped at, so records logged after the snapshot survive the clear even when a concurrent writer appends during the copy. The cursor is the sequence number the read stopped at, so records logged after the snapshot survive the clear even when a concurrent writer appends during the copy. Callers that want the whole buffer size it from `SIZE_BUFFER`, as `dmesg` does |

### Failure mode and effects analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|----|--------------|-------|--------------|---------------|----------|------------|
| F-02 | The ring stays full and old records are lost | Boot log volume above 64 KiB | Only the newest records remain | Missing evidence | 3 | Reads return the newest records, so the recent past survives; a full-size destination (`SIZE_BUFFER`) takes all of it |
| F-03 | A `syslog(2)` copy to user space fails | Unwritable destination | `EFAULT`, records kept | `dmesg` retries | 3 | The read copies into a kernel buffer first and `READ_CLEAR` clears only after a successful copy — and only up to the watermark the read reported, so a concurrent record is never lost |
| F-08 | A read and a concurrent push disagree about the window | A push wraps the ring onto the window mid-copy | The read returns the newest whole records that survived | `dmesg` shows the recent past rather than nothing | 3 | Copies are validated per record against the slot and the tail watermark, so a wrap costs the oldest records — the ones a newest-first copy has not reached yet |
| F-05 | The console provider is not ready | `LogSinkIf` not linked, or before `init_klogger` | Records only reach the ring once logging is up | No serial output, `dmesg` still works | 4 | The `log` macros are no-ops without a logger; `kruntime` links the provider early |
| F-06 | A record exceeds the line budget | A very long formatted message | Truncated to 255 bytes plus `\n` | The tail of the message is lost | 4 | Truncation preserves the prefix, the level tag and the terminator; the console still prints the full text |

### Concurrency invariants

- The ring is a byte buffer plus a record index; every window the reader is given
  starts at a record boundary, so no read begins mid-record or mid-character.
- A read takes no lock and never disables interrupts. A writer can wrap the ring
  onto the window while the copy runs, so every record is validated against its
  slot and the tail watermark; a wrap costs the oldest records, and the read
  returns the newest whole records that survived (restarting at most
  `LOGBUF_READ_RETRIES` times, and only when even the newest record is gone).
- The ring takes no lock and the console format lock is never held together with a
  ring append or read, so a console sink that logs cannot re-enter a held lock.
- `usage` and `buffer_size` are answered without copying the records, so a probe
  of the buffer size is cheap even when the ring is full.

### Known limitations

- No rate limit: a log flood overwrites the oldest bytes; reads still return the
  newest records.
- No `dmesg_restrict` equivalent: `READ_ALL` is open to every caller, matching
  Linux's default, and clearing is gated by `CAP_SYSLOG`, as in Linux.
- A destination smaller than the newest record returns nothing instead of a
  fragment, so a caller that wants a partial tail must size its buffer.
- A read takes no lock at all, so `dmesg` cannot stall a writer; it can only lose
  the oldest records to a wrap.
