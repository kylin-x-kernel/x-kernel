# Boot, Hang, And Panic

Use this reference for:

- boot stalls;
- no-output or partial-output boot failures;
- kernel panic, trap, abort, or fatal assertion paths;
- runtime hangs where forward progress stops.

## Primary Goal

Narrow the issue to one of these:

- logging never started;
- boot stopped between two checkpoints;
- crash or trap site is known;
- runtime hang belongs to one progress boundary.

## Execution Path

1. Identify the last confirmed progress signal.
2. Determine whether the failure is:
   - before normal logging starts;
   - during early init;
   - after scheduler or runtime services are active;
   - in a specific workload after boot.
3. Capture panic, trap, or abort text verbatim when available.
4. Map the last progress point to the owning init phase or subsystem.
5. Distinguish crash from deadlock, livelock, or silent wait.

If there is no progress signal at all,
the next action is to add sparse stage markers,
not broad tracing.

## Useful Clues

- last log line before silence;
- repeating log pattern suggesting retry or livelock;
- trap or page-fault context suggesting memory ownership issues;
- panic during teardown suggesting lifecycle or cleanup problems;
- platform-only failures suggesting HAL, driver, or boot glue ownership.

## Action Rules

- if logging never starts,
  instrument early boot checkpoints first;
- if the last log line is stable across runs,
  inspect the next boundary after that line;
- if the machine repeats output,
  suspect retry loops, wakeup bugs, or livelock before silent deadlock;
- if the panic includes a backtrace,
  start from the first non-generic frame rather than adding more logs;
- if only one architecture or platform fails,
  prioritize `arch/`, `platforms/`, and hardware-facing ownership.

## Initial Questions

- Did logging ever start?
- Is the machine stuck or repeatedly making progress?
- Does the same image fail on only one architecture or platform?
- Did the failure start after a scheduler, memory, driver, or boot change?

## Stop Condition

Stop this first pass once you know either:

- the last confirmed progress point and the next missing checkpoint; or
- the crash site and its likely owner boundary.
