# RTC Driver - Design

## Responsibility

The RTC driver owns persistent-clock discovery, register mapping, and
device-specific sampling. Its public read functions return normalized
`SystemTime` values and do not mutate global system time.

Platform initialization passes an accepted sample to `ktime`, which owns
the relationship between realtime and the monotonic clock. The driver remains
independent of timer queues and system timekeeping policy.

## Components

- `src/lib.rs` defines typed RTC configurations, device-tree discovery,
  mapping, dispatch, and timestamp range validation.
- `src/pl031.rs` samples an ARM PL031 MMIO RTC.
- `src/goldfish.rs` samples a Goldfish MMIO RTC.
- `src/cmos.rs` samples the x86 platform RTC through its port-I/O backend.

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
