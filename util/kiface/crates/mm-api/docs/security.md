# MM API Security

This analysis covers the entire crate, `src/lib.rs`. The memspace provider
owns kernel-layout locking and mapping validation; kdma owns the allocation
and caller-side device-quiescence obligations.

The caller retains an exclusively owned page-aligned kernel allocation and
quiesces device access before restoring ordinary attributes. The provider uses
the existing kernel layout lock and propagates protection errors. Coherent
maps READ|WRITE|UNCACHED|SHARED; Normal maps READ|WRITE, preserving the previous
two call-site behaviors. This contract cannot grant EXECUTE or USER access.
It does not change pre-existing allocation/rollback policy on protection or
platform-sharing failure; those remain separate lifecycle work.
