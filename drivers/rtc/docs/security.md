# RTC Driver - Security And Reliability

## Trust Model

Device-tree descriptions, MMIO register contents, and platform RTC values are
external inputs. Platform initialization chooses which discovered or static
configuration is allowed to establish realtime.

## External Boundaries

- Device-tree compatible strings and register ranges select an RTC backend and
  the physical MMIO area to map.
- PL031 and Goldfish registers provide unsigned Unix-second samples.
- The x86 RTC backend reads platform I/O registers through `x86_rtc`.

The driver normalizes these values to `SystemTime`. `ktime` publishes the
accepted sample and owns all later realtime correlation.

## Unsafe Code

`pl031::read_mapped` constructs `arm_pl031::Rtc` from a raw pointer. Its safety
depends on `read_from_device_tree` mapping the complete firmware-described
register range through `memspace::iomap_device` and retaining that mapping for
the sampling call. A zero virtual address is rejected before pointer creation.

The Goldfish and x86 helper crates encapsulate their own register access; their
returned values are still treated as untrusted numeric input here.

## Invariants

- Higher layers receive `SystemTime`, never an untyped hardware counter.
- RTC values larger than `i64::MAX` seconds are rejected.
- The driver does not own or mutate the global realtime correlation.
- Dispatch only accepts backend and transport combinations enabled for the
  current build; unsupported combinations panic during platform setup.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Malformed firmware MMIO range | High | Firmware describes an invalid PL031 or Goldfish region | Mapping is centralized in `memspace`; pointer construction only uses the returned mapped address. Residual risk depends on firmware validation and mapping lifetime. |
| T-02 | RTC value outside the semantic range | Medium | Hardware returns more than `i64::MAX` Unix seconds | Conversion returns `None`; the value is never saturated or published. |
| T-03 | Wrong but representable wall-clock value | Medium | Misconfigured or compromised RTC returns plausible seconds | The driver preserves the sample exactly; platform policy may reject absence but currently has no authenticity source. Residual risk is accepted. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | No supported RTC is discovered | `read_from_device_tree` returns `None` | Platform either stops initialization or uses epoch-plus-uptime fallback | 3 | Explicit platform policy at the call site. |
| F-02 | RTC sample is out of range | Device read returns `None` | Realtime is not initialized from that device | 3 | Checked conversion to signed `SystemTime`. |
| F-03 | Unsupported kind/transport pair | Driver panics | Boot fails | 2 | Configuration and feature selection must match the platform backend. |

## Known Limitations

The driver takes a boot-time snapshot only. It does not verify RTC authenticity,
detect later drift, or support runtime resampling and clock adjustment.

## Audit Checklist

- Confirm every MMIO backend receives an address returned by the device mapper.
- Confirm new hardware timestamp formats are normalized at the driver boundary.
- Confirm invalid samples return `None` rather than wrapping or saturating.
- Confirm platform handling of a missing RTC matches that platform's boot policy.
