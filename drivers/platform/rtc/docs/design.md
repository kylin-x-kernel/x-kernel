# RTC Driver - Design

## Responsibility

The RTC driver owns persistent-clock discovery, register mapping, and
device-specific sampling. Its public read functions return normalized
`SystemTime` values and do not mutate global system time.

Platform initialization passes an accepted sample to `ktime`, which owns
the relationship between realtime and the monotonic clock. The driver remains
independent of timer queues and system timekeeping policy.

## Non-Responsibilities

- No wall-clock ownership: sampled values are handed to `ktime`, which
  owns realtime correlation; the driver never mutates system time.
- No timer or scheduling policy: the driver is a one-shot sampler, not
  a clock-event source.
- No device-tree parsing: discovery consumes `of` accessors.
- No NVMEM/RTC alarm support: only time sampling is implemented.

## Components

- `src/lib.rs` defines typed RTC configurations, device-tree discovery,
  mapping, dispatch, and timestamp range validation.
- `src/pl031.rs` samples an ARM PL031 MMIO RTC.
- `src/goldfish.rs` samples a Goldfish MMIO RTC.
- `src/cmos.rs` samples the x86 platform RTC through its port-I/O backend.
- `src/ls7a.rs` samples the Loongson LS7A RTC TOY counters. The year and
  date/time halves are read non-atomically, so it re-samples until the year
  is stable to avoid pairing an old year with a new-year date.
- `ut-tests/lib.rs` checks timestamp range validation; `ut-tests/ls7a.rs`
  checks scripted LS7A samples. Each is mounted as `tests` from its original
  source module with `unittest::path_mod_test!` under `cfg(unittest)`.
  The LS7A cases also require the existing `ls7a` feature. These are guest
  tests run through the configured kernel unittest runner.

```text
firmware/platform description -> RtcConfig -> mapping -> device sample
                                                        |
                                                        +-> SystemTime
                                                               |
                                                               +-> ktime
```

## Execution Context

Discovery and sampling are early-boot operations. Device-tree reads require
firmware metadata to be initialized, and MMIO discovery requires `memspace`
device mapping to be available. The API does not require a current userspace
process or scheduler services.

The returned `SystemTime` is a snapshot. The driver does not retain a device
object, periodically resample the RTC, or provide runtime clock adjustment.

## Concurrency

The driver has no shared mutable state. Platform initialization performs the
single expected read before ordinary concurrent realtime readers start using
the correlation published by `ktime`.

## Design Decisions

- Hardware integer timestamps are converted to `SystemTime` at the driver
  boundary; raw second counts are not exposed to higher layers.
- Unsigned RTC values outside the signed `SystemTime` range are rejected
  instead of saturated, so invalid external input cannot silently become a
  different wall-clock value.
- Missing RTCs are reported with `Option`; each platform decides whether that
  condition is fatal or whether epoch-plus-uptime fallback is acceptable.
- The LS7A TOY counters are not read atomically; a torn sample straddling
  new-year is avoided by re-reading until the year halves agree (a normal
  crossing resolves on the next sample), and a year that never stabilizes
  rejects the read with `None` (logged) so each platform applies its own
  failed-sample policy instead of publishing a torn value.
- LS7A TOY bit fields are wider than the valid calendar ranges, so register
  contents that decode to no real timestamp (e.g. month or day 0) are
  rejected with `None` instead of panicking; each platform decides whether
  that is fatal or acceptable.
