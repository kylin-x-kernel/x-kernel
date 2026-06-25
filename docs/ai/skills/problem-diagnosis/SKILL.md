---
name: problem-diagnosis
description: Use when locating, triaging, or narrowing down X-Kernel problems, especially build failures, boot failures, kernel panics, syscall regressions, test failures, hangs, performance regressions, or unclear ownership across subsystems. This skill provides a first-pass diagnosis workflow, category-specific investigation methods, and evidence collection guidance.
---

# Problem Diagnosis

Use this skill when an AI agent needs to:

- classify an observed problem before attempting a fix;
- narrow the failure stage, subsystem, or ownership boundary;
- choose an appropriate first-pass investigation method;
- collect the minimum useful evidence for follow-up debugging;
- avoid ad hoc, low-signal debugging loops.

This is an initial version of the diagnosis skill.
It provides a common baseline workflow and a small set of reusable
problem-type playbooks.

## Scope

This skill covers first-pass diagnosis for:

- build and configuration failures;
- boot failures and early hangs;
- kernel panic, trap, and abort paths;
- syscall or user-space regression failures;
- test-only regressions;
- hangs, stalls, and timeout-like symptoms;
- performance regressions needing rough localization.

This skill does not replace subsystem-specific design documents,
architecture bring-up notes, or deep root-cause debugging procedures.

## Content Structure

This skill is intentionally split into two layers:

- basic diagnosis tools:
  how to collect logs, add logs, enable QEMU-side traces,
  inspect test output, and use backtrace or disassembly helpers;
- problem-specific methods:
  how to localize build failures, boot failures, panics, hangs,
  syscall regressions, test failures, and performance regressions.

Read the tools layer first.
Then choose the matching problem-specific method.

## Standard Workflow

1. Restate the observed symptom precisely.
2. Classify the failure stage.
3. Reproduce with the narrowest reliable command.
4. Capture the first failing signal, not a later cascade.
5. Map the signal to a likely subsystem boundary.
6. Choose the relevant investigation playbook.
7. Record what is known, what is inferred, and what is still missing.

## Failure Stage Classification

Classify the issue into one of these stages first:

- configuration or build time;
- image generation or boot handoff;
- early kernel init;
- runtime kernel execution;
- syscall or user-space visible behavior;
- test harness integration only;
- performance or latency regression.

If the stage is unclear, do not jump to code changes.
First gather one concrete artifact:

- build error output;
- the last boot log line;
- panic or trap text;
- the exact failing test name;
- before/after performance numbers.

## References

Read these references in order:

- start with the basic diagnosis tools:
  [references/basic-tools.md](references/basic-tools.md)
- then use the common workflow and evidence checklist:
  [references/general-workflow.md](references/general-workflow.md)

After that, read the matching problem-specific file:

- for build and configuration issues:
  [references/build-and-config.md](references/build-and-config.md)
- for boot, hang, and panic issues:
  [references/boot-hang-panic.md](references/boot-hang-panic.md)
- for syscall, runtime regression, and test failures:
  [references/runtime-and-regression.md](references/runtime-and-regression.md)
- for rough ownership mapping by symptom:
  [references/ownership-hints.md](references/ownership-hints.md)

## Core Rules

### 1. Diagnose before editing

Do not start changing code before answering:

- what is the first confirmed failure signal;
- what stage the failure belongs to;
- which subsystem most likely owns the boundary.

If additional visibility is needed,
prefer adding narrow diagnostic logs first
instead of speculative code changes.

### 2. Prefer the narrowest reproducer

Prefer one of these over a broad full-system run when possible:

- one build target;
- one boot scenario;
- one unit test;
- one harness case;
- one syscall contract case.

### 3. Separate evidence from inference

When reporting diagnosis progress, distinguish:

- direct observations from logs, traces, or test output;
- inferred ownership or suspected root cause;
- open questions that still need data.

### 4. Stop broadening the search once the boundary is narrow enough

The immediate goal of this skill is not always the final root cause.
A successful first pass may only narrow the issue to:

- one subsystem;
- one execution stage;
- one recent change window;
- one contract boundary.
