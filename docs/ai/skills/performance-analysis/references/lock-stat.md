# lock_stat (lock contention sub-item)

`klockstat` aggregates per lock-class statistics exposed via `/proc/lock_stat`.
Implementation docs: `util/klockstat/docs/design.md`, `util/klockstat/docs/security.md`.

## When to use

- workload involves multi-process / multi-thread / futex / fork traffic;
- kernel Mutex, RwLock, or Spin blocking is suspected;
- you need a `file:line` lock class before changing lock granularity.

Do not enable this for every performance task — turn it on only when lock
evidence is needed.

## Enable

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make menuconfig   # Task Scheduler → Task Diagnostics → Lock contention statistics
# or append to .config: CONFIG_KFEAT_LOCK_STAT=y
make olddefconfig
make build
```

Use **SMP** when investigating Spin `contentions` (UP records `acquisitions`
only).

## Read

```bash
cat /proc/lock_stat
```

| column | meaning |
|--------|---------|
| `location` | lock class init site (`file:line`) |
| `kind` | `Mutex`, `RwLock`, `SpinNoIrq`, … |
| `contentions` | times acquisition had to wait |
| `acquisitions` | successful lock takes |

- Default output is the top **5** rows by `contentions` (`DUMP_TOP_N`).
- **No reset API:** counters accumulate from boot. For a clean view, reboot and
  run one workload, or take before/after snapshots and compute deltas.

## Metric semantics

**Mutex / RwLock**

- short spin then CAS success → `acquisitions` only;
- sleep confirmed, before `block_on` → 1 `contentions`.

**SpinLock (SMP)**

- immediate success → `acquisitions` only;
- spun then acquired → `acquisitions` + `contentions`.

**SpinLock (UP)**

- `acquisitions` only.

Helper: `contention_rate ≈ contentions / acquisitions`.

## Interpretation patterns

| pattern | meaning | typical action |
|---------|---------|----------------|
| high `acquisitions`, low `contentions` | hot path, rarely blocks | often healthy; optimize only with latency evidence |
| highest `contentions` among peers | real contention | shrink critical section, shard, or split read/write |
| `mutex.rs:217` | aggregated `Mutex::default()` sites | find real callers, not the Default impl line |
| expected Spin missing | untracked `SpinLock::new()` | see tracking blind spots below |

## Workload → expected hotspots

| workload | common lock_stat entries |
|----------|--------------------------|
| concurrency soak (fork+mtx+futex) | `registry.rs` RwLock; `kfutex/table.rs` Mutex |
| fork storm | `registry.rs`; `aspace.rs` try_clone |
| fio / ext4 | pagecache, ext4 mutexes (often high acquisitions, low contentions) |
| scheduler-heavy | `thread/core.rs` CpuTime Mutex (high acquisitions) |

Script parameters: [workloads.md](workloads.md).

## Recommended sub-workflow

```
lock_stat sub-workflow:
- [ ] Confirm KFEAT_LOCK_STAT + SMP
- [ ] Baseline: cat /proc/lock_stat (or skip after fresh reboot)
- [ ] Run one focused workload
- [ ] cat /proc/lock_stat again
- [ ] Compute deltas; open matching source locations
- [ ] Separate hot-but-healthy from real bottlenecks
```

Guest example:

```bash
STRESSNG_MTX_PROCS=16 STRESSNG_FUTEX_PROCS=16 ./run-concurrency-soak.sh 60
cat /proc/lock_stat
```

## Tracking blind spots

**Tracked**

- `static_lock!` static Mutex / RwLock / Spin;
- heap `Mutex::new` / `RwLock::new` with `stats` (`#[track_caller]`).

**Often not tracked**

- `SpinLock::new()` / `SpinRaw::new()` (binds `NOOP_CLASS`);
- `Mutex::const_new(RawMutex::new(), …)`;
- `RawMutex::with_config` without `new_with_stats`.

Hot untracked examples: `run_queue.rs` `SpinRaw<Scheduler>`; `kfutex`
`WaitQueue` `SpinNoIrq`.

Call out blind spots in the report. For Spin visibility, propose `static_lock!`
or Spin `track_caller` parity with Mutex.

## Limitations

- top 5 only; rare contended locks may be hidden;
- cumulative counters; mixed workloads confuse attribution;
- diagnostic overhead; production images normally keep this disabled (see
  security.md).
