# kiface Security Notes

`kiface` is a build-time wiring mechanism. It does not provide an isolation
boundary; all providers run in the same kernel address space as their callers.

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

The first `interface` implementation rejects generics, receivers, unsafe
methods, async methods, extern methods, variadic methods, and default bodies.
These restrictions keep the generated ABI surface small while the crate is used
to replace existing single-provider `crate_interface` and accidental `linkme`
entry wiring.
