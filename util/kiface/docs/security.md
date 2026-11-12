# kiface Security Notes

## Scope

This analysis covers the entire crate: `src/lib.rs` (the two public
attribute macros) and its private modules `src/args.rs`, `src/interface.rs`,
`src/provide.rs`, `src/naming.rs`, `src/errors.rs`, and `src/validator.rs`,
including the code those macros generate. The test fixtures under
`test_crates/` are separate crates exercised by host integration tests
and are excluded from this analysis; they contain no production wiring.
External responsibility: the macros emit no runtime component, so runtime
isolation, scheduling, and memory safety of provider bodies belong to the
crates that write them.

## Unsafe And FFI Boundary

The crate itself is safe Rust, but its macros generate the kernel's
one-symbol-per-method FFI boundary (all in the expansion of the two
attributes):

- Interface side (`src/interface.rs`): a hidden module containing an
  `unsafe extern "Rust"` block whose functions carry
  `#[link_name = "__kiface_{namespace}_{Interface}_{method}"]`, plus one
  facade method per interface method containing
  `unsafe { extern_fn(args) }` with a `SAFETY:` comment.
- Provider side (`src/provide.rs`): each method is emitted as
  `#[unsafe(export_name = "__kiface_{namespace}_{Interface}_{method}")]
  [unsafe] extern "Rust" fn ...` with its original body and safety qualifier.

Safety of the generated unsafe rests on two locally checked facts:
(1) `src/provide.rs` checks a `fn` or `unsafe fn` pointer against the facade
signature and compares the generated per-method safety marker. This rejects
qualifier mismatches even where safe-to-unsafe pointer coercion would otherwise
succeed. A provider that does not match fails to compile; and
(2) both symbol names derive from the same `src/naming.rs` rule
(`__kiface_{namespace}_{Interface}_{method}`, or `__kiface_{Interface}_{method}` when no namespace is given), so the definition and
provider sides can only meet when namespace, interface name, and method
name agree. The remaining risk is linking more or fewer than one provider,
covered by the invariants below.

`kiface` is a build-time wiring mechanism. It does not provide an isolation
boundary; all providers run in the same kernel address space as their callers.
For an unsafe facade method, its caller must uphold the interface owner's
`# Safety` contract before crossing the generated extern boundary. The provider
may rely on that contract but must use explicit justified unsafe blocks for
unsafe operations in its body. The macros do not establish IRQ exclusion,
validate raw-pointer provenance or enforce dynamic execution context.

## Invariants

- Each interface method must have exactly one linked provider.
- The provider method signature must match the facade method signature.
- The final image must include the provider crate whenever an interface method is
  called.
- Interface methods must not rely on registration order or multiple providers.

Duplicate providers for the same interface method export the same symbol and
should fail during linking. Missing providers fail when the generated facade call
references an unresolved symbol.

`optional` interfaces are reserved but not implemented. They must not silently
reuse the required-interface path, because that would turn an expected
best-effort dependency into a hard link dependency.

## Current Restrictions

The `interface` implementation rejects generics, receivers, `const` methods,
async methods, explicit extern methods, variadic methods, and default bodies.
These restrictions keep the generated ABI surface small while the crate is used
to replace existing single-provider `crate_interface` and accidental `linkme`
entry wiring.
