# General Workflow

Use this reference for the baseline diagnosis flow
when the problem type is not yet clear.

## Immediate Goal

Do not try to find the final root cause immediately.
The first-pass goal is to narrow the issue to one of these:

- one failure stage;
- one subsystem boundary;
- one reproducer;
- one recent change window.

Stop broad exploration once one of those is achieved.

## Execution Order

Run this sequence in order:

1. What is failing?
2. At what stage does it fail?
3. What is the first trustworthy signal?
4. What is the narrowest reproducer that still fails?
5. Which subsystem boundary does that signal touch first?

If any step cannot be answered,
collect one more artifact rather than jumping to code edits.

## Evidence Checklist

Capture the smallest useful evidence set:

- exact command used;
- target platform or defconfig;
- whether the issue is deterministic;
- the first failing line or signal;
- whether the issue is new or long-standing;
- any known recent change window.

## Recommended Narrowing Order

Use this order unless the symptom already points somewhere specific:

1. Narrow by stage.
2. Narrow by architecture or platform.
3. Narrow by subsystem boundary.
4. Narrow by reproducer.
5. Narrow by change window.

## Decision Rule

Use this simple routing rule:

- if the first failure is from build or link output,
  go to `build-and-config.md`;
- if the system never boots cleanly or stops making progress,
  go to `boot-hang-panic.md`;
- if a test, syscall, or workload fails after boot,
  go to `runtime-and-regression.md`;
- if ownership is still unclear after that,
  use `ownership-hints.md`.

## Useful Outputs To Preserve

Keep these artifacts in the diagnosis notes:

- build stderr or compiler error span;
- panic backtrace or trap register dump;
- boot log tail;
- failing test name and assertion text;
- timing numbers for before and after comparisons.

## Minimum Report Shape

When handing off or summarizing diagnosis progress,
report in this shape:

- symptom:
- reproducer:
- first confirmed signal:
- likely stage:
- likely owner:
- open question:
