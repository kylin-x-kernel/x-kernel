---
name: performance-analysis
description: Analyze X-Kernel performance using workloads, baselines, and in-kernel diagnostics. Use when investigating latency regressions, soak benchmarks, throughput drops, scheduler or IO hotspots, lock_stat contention, or comparing before/after kernel changes with evidence.
---

# Performance Analysis

Use this skill when an AI agent needs to:

- structure a performance investigation with reproducible workloads;
- choose the right measurement method for the suspected bottleneck;
- collect before/after evidence before recommending optimizations;
- interpret guest benchmarks and in-kernel diagnostics;
- report findings with clear limits on what the data proves.

This skill is for **performance localization and evidence collection**.
For first-pass failure triage without a perf hypothesis, start with
`docs/ai/skills/problem-diagnosis/SKILL.md`.

## Scope

| method | when to use | reference |
|--------|-------------|-----------|
| **Workload / soak** | need reproducible CPU/IO/fork/concurrency pressure | [references/workloads.md](references/workloads.md) |
| **lock_stat** | suspect Mutex/RwLock/Spin blocking or hot locks | [references/lock-stat.md](references/lock-stat.md) |
| **Baseline comparison** | verify a regression or optimization claim | below |

Additional tools (tracing, scheduler snapshots, etc.) may be added as
sub-references under `references/` over time. Do not invent tools that are
not documented in this repository.

## Prerequisites

1. Prepare `.config` per `docs/ai/skills/build-workflow/SKILL.md`.
2. Match **platform, SMP CPU count, and defconfig** across compared runs.
3. For guest workloads, load `docs/ai/skills/test-harness/SKILL.md`.

## Standard Workflow

```
Performance analysis:
- [ ] 1. State the symptom and success criterion (latency? throughput? tail?)
- [ ] 2. Classify suspected domain: CPU / IO / fork / sync / net / scheduler
- [ ] 3. Pick the narrowest workload (see workloads.md)
- [ ] 4. Capture baseline measurement
- [ ] 5. Apply one change or run target workload
- [ ] 6. Capture post measurement under the same conditions
- [ ] 7. Add in-kernel evidence if needed (e.g. lock_stat)
- [ ] 8. Report: observation → inference → recommended next step
```

### Step 1 — State the question

Good questions:

- "Did registry locking regress under fork storm?"
- "Is fio slow because of pagecache mutex contention?"
- "Does concurrency soak block on kfutex or registry?"

Bad questions (too vague):

- "Make it faster"
- "Check performance"

### Step 2 — Classify the domain

| domain | typical signals | first workload |
|--------|-----------------|----------------|
| sync / futex / pthread | mtx/futex stress; user hangs | concurrency soak |
| process lifecycle | fork/clone cost; PID table | fork storm |
| block / FS IO | fio QPS/IOPS drop | fio-soak-ext4 |
| metadata | create/unlink heavy | fs-soak-ext4 |
| scheduling | wakeup latency; hackbench | schbench / hackbench (guest) |
| locking (kernel) | unknown hotspot | lock_stat + targeted soak |

### Step 3 — Run a narrow workload

See [references/workloads.md](references/workloads.md).
Prefer one harness `CASES=...` or one guest script per iteration.

### Step 4 — Baseline comparison rules

- use the **same** duration, process counts, and env vars;
- note guest wall-clock vs harness timeout;
- for lock_stat: counters are cumulative — reboot, or take before/after
  snapshots and compute deltas (see lock-stat reference);
- separate **observation** (numbers) from **inference** (root cause).

### Step 5 — Add in-kernel diagnostics when warranted

Use lock_stat when:

- workload implicates kernel synchronization;
- latency regression correlates with multi-threaded or multi-process load;
- you need to name a `file:line` lock class before editing code.

Full procedure: [references/lock-stat.md](references/lock-stat.md).

Do not enable `KFEAT_LOCK_STAT` for every perf task — only when lock
evidence is needed.

### Step 6 — Report

Use [references/report-template.md](references/report-template.md).

## Core Rules

### 1. Measure before optimizing

Do not shrink critical sections, add caching, or change algorithms without
a workload and at least one measurement axis.

### 2. One variable per iteration

Change one of: kernel patch, Kconfig option, workload params, CPU count.
Stacking changes destroys attribution.

### 3. High traffic ≠ bottleneck

Especially for lock_stat: prioritize **contentions** and latency evidence,
not raw `acquisitions` alone.

### 4. Know method limits

| method | proves | does not prove |
|--------|--------|----------------|
| soak scripts | stability / pressure / rough regression | exact root cause |
| lock_stat | which lock classes waited | hold time distribution |
| guest benchmark | end-to-end user-visible latency | internal kernel line-level cause |

### 5. Stop when localized

A successful pass may only narrow to:

- one subsystem;
- one resource class (IO vs sync vs scheduler);
- one lock class or one harness case.

That is enough to hand off to a focused code change.

## Related Skills

- `docs/ai/skills/problem-diagnosis/SKILL.md` — triage before perf deep-dive
- `docs/ai/skills/test-harness/SKILL.md` — longevity / soak execution
- `docs/ai/skills/code-guidelines/performance-and-resources.md` — review checklist
- `docs/ai/skills/code-guidelines/concurrency.md` — lock API conventions
