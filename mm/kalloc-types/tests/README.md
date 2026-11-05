# Hosted ownership checks

Run the default storage tests and the opt-in intrusive adapter tests separately:

```sh
RUSTFLAGS='' CARGO_ENCODED_RUSTFLAGS='' cargo +nightly-2026-03-08 test -p kalloc-types --target x86_64-unknown-linux-gnu
RUSTFLAGS='' CARGO_ENCODED_RUSTFLAGS='' cargo +nightly-2026-03-08 test -p kalloc-types --features intrusive-collections --target x86_64-unknown-linux-gnu
RUSTFLAGS='' CARGO_ENCODED_RUSTFLAGS='' MIRIFLAGS='-Zmiri-strict-provenance' cargo +nightly-2026-03-08 miri test -p kalloc-types --features intrusive-collections --target x86_64-unknown-linux-gnu
RUSTFLAGS='' CARGO_ENCODED_RUSTFLAGS='' MIRIFLAGS='-Zmiri-strict-provenance -Zmiri-tree-borrows' cargo +nightly-2026-03-08 miri test -p kalloc-types --features intrusive-collections --target x86_64-unknown-linux-gnu
```

The list probes use production KArcPointerOps and the real intrusive_collections
LinkedList with test-only link projections. They cover count transfer,
known-member removal and reinsertion, recovery of the rejected owner's count
when the library panics on duplicate insertion, independent links and normal
list teardown. No custom queue or identity allocation is involved.
The library's own `LinkedList::take` is tested for transferring all owners
without destruction, followed by deferred drop and reuse of the unlinked node.

These are not timer cancellation/generation tests. The adapter does not validate
which list owns a raw cursor, enforce lock ordering, defer final destruction or
guarantee complete list draining after a payload panic. Do not use fast_clear
for an owning timer queue: that library operation deliberately abandons owners.
