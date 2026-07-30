# Basic Diagnosis Tools

Use this reference before applying a problem-specific method.
It describes the baseline tools for collecting evidence in X-Kernel.

## Tool Selection Rule

Choose the lightest tool that can answer the next question:

- start with existing logs;
- then add narrow logs;
- then use backtrace, QEMU-side logs, or debugger-oriented flows;
- only after that consider broad instrumentation.

## 1. Read Existing Logs First

Before adding new instrumentation,
check whether the existing outputs already contain the first failing signal.

Common places to inspect:

- kernel console output from `make run`;
- QEMU-side logs produced by `make run QEMU_LOG=y`;
- harness logs such as `suite.log` and `cases/<name>.log`;
- unit-test output from `make UNITTEST=y run` or `make unittest`.

Before running `make run`, `make UNITTEST=y run`, or `make unittest`,
follow `docs/ai/skills/build-workflow/SKILL.md` and prepare `.config` from the
target platform defconfig.

Default preparation for the normal ARM64 QEMU platform:

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
```

Useful commands and flows already supported by the repository:

```bash
make run
make run QEMU_LOG=y
make UNITTEST=y run
make unittest
make debug
make disasm
make justrun QEMU_ARGS="-s -S"
```

Notes:

- `QEMU_LOG=y` writes QEMU-side diagnostics to `qemu.log`;
- harness logs are described by the `test-harness` skill;
- `make debug` and `QEMU_ARGS="-s -S"` are for debugger-oriented sessions,
  not the default first pass.

### Stop Condition

Stop here and move to a problem-specific method
if you already have all three:

- the first failing signal;
- the failing stage;
- a likely subsystem boundary.

## 2. Add Logs Narrowly

When existing logs are insufficient,
add the smallest possible log statements around the suspected boundary:

- before and after a state transition;
- before returning an unexpected error;
- around resource acquire or release;
- at the entry and exit of a suspicious path;
- around one-time init or teardown checkpoints.

Do not spray logs across unrelated modules.
Add them where they help answer one specific question.

### Question-Driven Logging

Before inserting a log,
state the question it is meant to answer:

- did execution reach this checkpoint?
- which branch was taken?
- what value caused rejection or failure?
- did a resource get created, mapped, woken, or released?
- did we return the expected errno or error variant?

If a log does not answer one of these,
it is probably too vague.

### Logging Rules

Follow the project logging conventions:

- first-party OSTD-based code should use `ostd::log` macros such as
  `debug!`, `info!`, `warn!`, and `error!`;
- do not use `println!` or the third-party `log` crate directly
  in normal production code;
- early boot code may use early output helpers before logging is initialized;
- keep crate-local log style and `__log_prefix` conventions consistent.

### Log Level Guidance

- use `error!` for serious but recoverable failures;
- use `warn!` for fallback paths or degraded behavior;
- use `info!` for infrequent progress checkpoints and init milestones;
- use `debug!` for development diagnostics and state transitions;
- use `trace!` only for very high-frequency detail when a concrete need exists.

### What Good Temporary Logs Look Like

Prefer logs that answer one of these:

- which branch was taken;
- which object or identifier was involved;
- which address, size, or errno-like value was observed;
- whether progress reached a specific checkpoint;
- whether cleanup or wakeup actually happened.

Avoid logs that merely restate control flow without useful data.

### Where To Add Logs First

Prefer these boundaries first:

- public API entry or exit;
- error return sites;
- state transition points;
- ownership transfer points;
- one-time init checkpoints;
- wakeup, block, map, unmap, open, close, or exec boundaries.

### Stop Condition

Once the new logs identify the failing checkpoint or owner boundary,
stop adding more logs and switch back to diagnosis.

## 3. Add Stage Markers For Boot Or Hang Problems

For boot issues, add sparse progress markers at major checkpoints:

- before logger init;
- after allocator init;
- after memory setup;
- after scheduler init;
- before launching a specific subsystem or workload.

The goal is to identify the last confirmed progress point,
not to print every line of execution.

### Stop Condition

When you know the last confirmed progress point
and the next missing checkpoint,
the boot search space is already narrow enough for the next step.

## 4. Use Backtrace And Trap Output

For panic, trap, or fatal assertion paths,
preserve the panic text, trap context, and backtrace output first.

Relevant existing capabilities:

- `task/ktask` contains snapshot and backtrace dump support;
- `util/backtrace` provides stack unwinding and symbolication support;
- some panic or trap paths already print full context.

When a backtrace is available:

- record the first non-generic frame;
- separate the trapping site from cleanup or panic-handling frames;
- check whether the frame sequence points to one subsystem boundary.

### When To Prefer Backtrace Over More Logs

Prefer backtrace first when:

- the failure is a panic, trap, or fatal assertion;
- the system already tells you where it died;
- adding more logs would only duplicate the crash site.

## 5. Use Test Output As A Contract Signal

For runtime regressions,
the failing test name and assertion text are often more valuable
than broad kernel logs.

Start with:

- which case failed;
- the exact assertion or errno mismatch;
- whether the same case passes on Linux;
- whether the issue is guest-visible or harness-only.

If the failure is syscall-focused,
load the `test-harness` skill and inspect suite and case logs first.

### Stop Condition

If the failing contract can be stated in one sentence,
do not keep widening logging until that contract is mapped to an owner.

## 6. Keep Diagnostic Edits Temporary And Focused

Temporary diagnosis logs should be:

- narrow in scope;
- easy to remove;
- clearly tied to one hypothesis;
- placed at stable ownership boundaries when possible.

After localization is complete,
either remove temporary logs
or convert only the truly durable ones into production-quality diagnostics.

## Minimal Tool Playbook

Use this quick routing table:

1. If there is already a clear build error:
   use build output, not new logs.
2. If boot stops or goes silent:
   add sparse stage markers.
3. If there is a panic or trap:
   preserve backtrace before editing.
4. If one test fails after boot:
   start from the failing assertion and case log.
5. If ownership is still unclear:
   add one narrow log at the next subsystem boundary.
