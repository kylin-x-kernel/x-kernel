# Performance Analysis Report Template

```markdown
# Performance analysis: [title]

## Question
[One sentence: symptom, hypothesis, success criterion]

## Environment
- Platform / defconfig: [e.g. aarch64-qemu-virt]
- SMP CPUs: [N]
- Kernel change under test: [commit / patch / none]
- Workload: [harness CASE or guest script, duration, env overrides]

## Measurements

### Workload results
| metric | baseline | after | delta |
|--------|----------|-------|-------|
| [e.g. soak exit, fio IOPS, bench latency] | | | |

### lock_stat (optional sub-item)
Fill in when `KFEAT_LOCK_STAT` is enabled; otherwise remove this section.

#### Baseline
[paste /proc/lock_stat or "fresh boot, skipped"]

#### After workload
[paste /proc/lock_stat]

#### Delta
| location | kind | Δ contentions | Δ acquisitions |
|----------|------|---------------|----------------|
| … | … | … | … |

## Findings

### Primary bottleneck class
[CPU / IO / sync / fork / scheduler / inconclusive]

### Evidence
- **Observation:** [numbers from measurements]
- **Inference:** [likely owner / mechanism]
- **Confidence:** [high / medium / low — and why]

### Hot-but-healthy (if any)
[high traffic, low blocking — e.g. registry acquisitions with low contentions]

### Blind spots
[untracked Spin, DUMP_TOP_N=5, cumulative counters, harness limits]

## Assessment
[regression confirmed | no regression | localized to subsystem X | need more data]

## Recommended next steps
1. [focused change or follow-up measurement]
2. [verification command]

## Method limits
[which conclusions are not proven by this pass]
```
