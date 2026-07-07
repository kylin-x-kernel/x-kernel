# Runtime And Regression

Use this reference for:

- syscall regressions;
- user-space visible behavior changes;
- unit-test or harness failures;
- bugs that appear after boot under workload.

## Primary Goal

State the failing contract in one sentence
and map it to one subsystem owner.

## Execution Path

1. Identify the failing contract:
   syscall semantics, resource lifecycle, memory visibility,
   filesystem behavior, scheduling, or device interaction.
2. Reproduce with the smallest test or case.
3. Separate implementation noise from contract failure.
4. Decide whether the issue is:
   - a true runtime bug;
   - a test expectation bug;
   - a harness or environment mismatch;
   - an architecture-specific semantic difference.
5. Map the contract to the owning subsystem.

Do not start from broad kernel internals
when the user-visible failure can still be narrowed at the contract layer.

## Regression Questions

- What exact behavior changed?
- Is the regression visible to user space or only to a test harness?
- Does the same case fail on Linux or only on X-Kernel?
- Is there an obvious recent change window?
- Can the failure be reduced to one syscall family or resource type?

## Action Rules

- if exactly one test case fails,
  start from that case and its assertion text;
- if many tests fail in one subsystem family,
  look for a shared owner boundary instead of fixing cases one by one;
- if Linux and X-Kernel differ,
  state the expected contract before inspecting implementation;
- if the failure is harness-only,
  verify environment and suite wiring before changing kernel code.

## Performance Regressions

For an initial pass, focus on rough localization:

- confirm the regression with comparable commands and config;
- identify whether CPU, memory, I/O, scheduling, or locking is implicated;
- determine whether the slowdown is global or workload-specific;
- avoid speculative micro-optimizations before the hotspot class is known.

When locking is suspected and `KFEAT_LOCK_STAT` is available, continue with
`docs/ai/skills/performance-analysis/SKILL.md` and its
`references/lock-stat.md` sub-item for `/proc/lock_stat` workloads,
snapshot comparison, and contention reporting.

## Stop Condition

Stop this first pass once you can report:

- the failing contract;
- the smallest failing case;
- whether the issue is kernel, test, harness, or environment;
- the most likely owning subsystem.
