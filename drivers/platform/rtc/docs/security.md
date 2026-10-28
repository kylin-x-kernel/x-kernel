# RTC Driver - Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (typed configs,
device-tree discovery, mapping, dispatch, timestamp validation) plus
the backend modules `src/pl031.rs`, `src/goldfish.rs`, `src/cmos.rs`,
and `src/ls7a.rs`, plus the guest fixtures in `ut-tests/lib.rs` and
`ut-tests/ls7a.rs`. No modules are excluded; backend register access is
audited as part of the boundary descriptions below. The fixtures check
timestamp bounds and scripted LS7A samples without accessing real MMIO;
the LS7A cases retain the backend's `ls7a` feature gate.

Device-tree descriptions, MMIO register contents, and platform RTC values are
external inputs. Platform initialization chooses which discovered or static
configuration is allowed to establish realtime.

## External Boundaries

- Device-tree compatible strings and register ranges select an RTC backend and
  the physical MMIO area to map.
- PL031 and Goldfish registers provide unsigned Unix-second samples.
- The x86 RTC backend reads platform I/O registers through `x86_rtc`.
- The LS7A backend reads the TOY year and packed date/time counters; because
  the two registers are not sampled atomically, a torn read is detected by
  re-sampling until the year halves match.

The driver normalizes these values to `SystemTime`. `ktime` publishes the
accepted sample and owns all later realtime correlation.

## Unsafe Code

`pl031::read_mapped` constructs `arm_pl031::Rtc` from a raw pointer. Its safety
depends on `read_from_device_tree` mapping the complete firmware-described
register range through `memspace::iomap_device` and retaining that mapping for
the sampling call. A zero virtual address is rejected before pointer creation.

The Goldfish and x86 helper crates encapsulate their own register access; their
returned values are still treated as untrusted numeric input here.

`ls7a::read_mapped` dereferences volatile pointers to the mapped LS7A aperture.
Its safety depends on the caller (platform init) mapping the full register range
through `memspace::iomap_device` and passing the resulting virtual address. A
zero virtual address is rejected before any pointer dereference.

## Invariants

- Higher layers receive `SystemTime`, never an untyped hardware counter.
- RTC values larger than `i64::MAX` seconds are rejected.
- The driver does not own or mutate the global realtime correlation.
- Dispatch only accepts backend and transport combinations enabled for the
  current build; unsupported combinations panic during platform setup.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Malformed firmware MMIO range | High | Firmware describes an invalid PL031, Goldfish, or LS7A region | Mapping is centralized in `memspace`; pointer construction only uses the returned mapped address. Residual risk depends on firmware validation and mapping lifetime. |
| T-02 | RTC value outside the semantic range | Medium | Hardware returns more than `i64::MAX` Unix seconds, or LS7A TOY bits that decode to no valid calendar date (e.g. month or day 0) | Conversion returns `None`; the value is never saturated or published. |
| T-03 | Wrong but representable wall-clock value | Medium | Misconfigured or compromised RTC returns plausible seconds | The driver preserves the sample exactly; platform policy may reject absence but currently has no authenticity source. Residual risk is accepted. |
| T-04 | Torn year/date sample across new-year | Medium | An LS7A read straddles the year rollover, pairing an old year with a new-year date | `ls7a` re-samples until the year halves agree; a persistently unstable year rejects the read with `None` and is logged. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | No supported RTC is discovered | `read_from_device_tree` returns `None` | Platform either stops initialization or uses epoch-plus-uptime fallback | 3 | Explicit platform policy at the call site. |
| F-02 | RTC sample is out of range | Device read returns `None` | Realtime is not initialized from that device | 3 | Checked conversion to signed `SystemTime`. |
| F-03 | Unsupported kind/transport pair | Driver panics | Boot fails | 2 | Configuration and feature selection must match the platform backend. |
| F-04 | Device-tree RTC range cannot be mapped | `read_from_device_tree` logs the mapping error and returns `None` | Same platform policy as F-01 | 3 | No pointer is constructed without a successful mapping. |

## Known Limitations

The driver takes a boot-time snapshot only. It does not verify RTC authenticity,
detect later drift, or support runtime resampling and clock adjustment.

## Audit Checklist

- Confirm every MMIO backend receives an address returned by the device mapper.
- Confirm new hardware timestamp formats are normalized at the driver boundary.
- Confirm invalid samples return `None` rather than wrapping or saturating.
- Confirm platform handling of a missing RTC matches that platform's boot policy.

## Repeated PM sampling

RtcClock retains an initialized RtcConfig and implements PersistentClock. The
unsafe from_config constructor rejects unmapped/null MMIO and disabled backend combinations;
private binding state prevents source replacement through the handle. Repeated
reads reuse existing backend access, including the unsafe PL031 construction
and LS7A volatile accesses listed above. Platform code must retain the mapping
for the entire handle/session lifetime and restore hardware access after sleep,
not merely for boot setup. The unsafe constructor contract requires callers to establish register storage,
alignment, lifetime and access permissions before accepted bindings can expose
safe repeated reads. from_device_tree is the new production unsafe call site:
it obtains an enabled, mapped configuration from mapped_config_from_device_tree
and retains the kernel mapping. Metadata tests use real aligned backing storage
or rejected configurations. No arbitrary non-null virtual address is certified
as valid by the numeric constructor checks.

Sampling uses no timekeeper locks and does not mutate system time. PM must
serialize other RTC/PIO users and prevent writes across a measurement. A valid
numeric sample does not authenticate the clock or prove cross-host-suspend
continuity. Resolution reflects the returned timestamp: one millisecond for
LS7A, one second for PL031, Goldfish and CMOS. Advertising finer precision than
the driver preserves would understate truncation; advertising a coarser quantum
would unnecessarily widen the sleep-estimator's deficit allowance.
Constructor/metadata tests do not read test addresses and do not verify hardware
mapping lifetime, transport concurrency or real suspend behavior.
