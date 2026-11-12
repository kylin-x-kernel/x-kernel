# kerrno — Security and reliability

## Scope, assets and trust

The complete scope is `src/lib.rs`, including the four exported macros (`k_err_type!`, `k_err!`, `k_bail!`, `ensure!`), hidden
macro support module and `cfg(unittest)` tests. The asset is correct error-domain
classification at the kernel/Linux boundary. An error value contains neither
credentials nor a resource handle and grants no permission. Subsystem callers
supply kinds, Linux errno or serialized signed codes; only the checked decoder
validates arbitrary serialized input.

## Boundaries and invariants

`KError` requires a recognized positive kernel-kind code or a negative Linux errno.
`try_from_i32` returns the input integer on zero, unknown code or failed checked
negation. Kernel-kind conversion to Linux errno is explicit and many-to-one;
callers must not negate `KError::code()` as if every code were already Linux errno.

`LinuxError` is imported from `linux_sysno`; `LinuxError::new` accepts any signed
integer. `From<LinuxError>` negates it without validating it, unlike the checked
internal-code decoder. Only positive errno is appropriate there. Passing zero
can make subsequent formatting/conversion panic; negative errno may silently
become a kernel kind or cause a panic, and `i32::MIN` may overflow on negation.
These are current limitations, not validation performed by `kerrno`.

There are no explicit `unsafe` operations, unsafe traits/impls, FFI, assembly,
raw pointer parameters or user-memory accesses anywhere in this crate. Error
conversion does not establish safety or authorization for an external operation.
Dependencies and logging backends are outside this analysis.

## Threats and responses

| ID | Asset / trigger / consequence | Response and remaining responsibility |
|---|---|---|
| T1 | Wrong sign domain emitted as Linux errno changes user-visible failure semantics | Convert through `LinuxError::from`; `KError::code` is explicitly internal. Adapters must apply the final Linux sign convention. |
| T2 | Unchecked malformed `LinuxError` creates an invalid carrier, causing panic or misclassification | Use positive errno and `try_from_i32` for serialized codes. `From<LinuxError>` remains unchecked and cannot be advertised as a validator. |
| T3 | Canonicalization loses distinctions such as `InvalidData` versus `InvalidInput` | Preserve the original semantic kind when that distinction matters; use normalization for comparisons that intentionally use canonical semantics. |
| T4 | A supplied warning message contains user data, credentials or addresses | Macro callers must redact before logging and avoid attacker-controlled high-volume warning loops. No redaction or rate limiting exists here. |

## Concurrency, failure handling and FMEA

Error values are immutable `Copy` integers with no synchronization requirements.
The macros invoke the logger, whose lock/context guarantees are external. The
crate neither retries operations nor unwinds resources on behalf of callers.
`Result` transfers the recovery choice to the owning subsystem.

Severity: 1 fatal, 2 serious, 3 moderate, 4 minor.

| ID | Failure / cause | Local effect | System effect | Severity | Handling |
|---|---|---|---|---|---|
| F1 | Unknown serialized code | `Err(original)` | Caller must reject or preserve external error | 3 | Checked decoder; no fallback success. |
| F2 | Invalid unchecked carrier | Panic or wrong kernel kind | Caller failure / potentially fatal kernel panic | 2 | Validate errno sign before `From<LinuxError>`. |
| F3 | Many-to-one errno round trip | Original detail lost | Less specific user-visible diagnostics | 4 | Keep semantic kind when detail is required. |
| F4 | Warning used in unsuitable context | Logger-dependent failure or contention | Blocking/latency under backend policy | 2 | Use pure constructors where logging is unsuitable. |

## Privacy and audit checklist

Pure errors contain only numeric kinds and static descriptions. Warning macros
can additionally expose supplied messages through the logging backend; `kerrno`
has no persistent storage, secret erasure, log access control or rate limiter.
Verify new kinds in both mapping directions and in `KIND_LINUX_PAIRS` tests.
`test_kerror_try_from_i32_and_formatting`,
`test_kerror_constants_and_try_from_i32_invalid_linux`, and canonicalization tests
cover checked rejection and domain behavior; host doctests cover the public
examples. They do not establish a safe contract for malformed unchecked errno or
validate the runtime logger's IRQ suitability.
