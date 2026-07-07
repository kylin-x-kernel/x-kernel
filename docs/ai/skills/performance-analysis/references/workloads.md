# Workloads

Guest workloads live in the external **Starry Test Harness** repository.
See `docs/ai/skills/test-harness/SKILL.md` for checkout path and run flow.

## Quick selection

| goal | harness case | guest script (under `tests/stress-ng/`) |
|------|--------------|----------------------------------------|
| mutex / futex / sem contention | `stress-ng-concurrency-soak` | `run-concurrency-soak.sh` |
| registry / clone pressure | `stress-ng-fork-storm-soak` | `run-fork-storm-soak.sh` |
| CPU / vm / io mixed | `stress-ng-soak` | `run-soak.sh` (if present) |
| block device randrw | `fio-soak-ext4` | fio scripts in harness |
| metadata + IO | `fs-soak-ext4` | fsstress + fio |
| network throughput | `iperf3-soak-guest-client` | iperf3 scripts |

## Harness run pattern

```bash
CASES=stress-ng-concurrency-soak ARCH=aarch64 make longevity-prereqs
export XKERNEL_REMOTE=/path/to/x-kernel
CASES=stress-ng-concurrency-soak ARCH=aarch64 make longevity-test run
```

For local iteration, prefer single `CASES=...` over the full longevity matrix.

## Guest-side overrides

Concurrency soak (sync / futex pressure):

```bash
STRESSNG_FORK_PROCS=4 STRESSNG_MTX_PROCS=16 STRESSNG_FUTEX_PROCS=16 \
STRESSNG_SEM_PROCS=8 STRESSNG_CPU_PROCS=2 \
  ./run-concurrency-soak.sh 60
```

Fork storm (process table / aspace):

```bash
STRESSNG_FORK_PROCS=8 STRESSNG_FORK_MAX=64 \
STRESSNG_FORKHEAVY_PROCS=4 STRESSNG_FORKHEAVY_MAX_PROCS=128 \
  ./run-fork-storm-soak.sh 120
```

## Pairing workloads with diagnostics

| after this workload | consider |
|---------------------|----------|
| concurrency soak | [lock-stat.md](lock-stat.md) — kfutex, registry |
| fork storm | lock_stat — registry, aspace |
| fio / fs soak | lock_stat — pagecache, ext4; also compare fio metrics |
| scheduler guest bench | lock_stat optional; focus on bench numbers |

## SMP note

Parallel contention (fork, futex, multi-threaded mtx) needs **SMP**.
Single-core guests under-report blocking that only appears cross-CPU.
