# vmobj — Security Document
## Trust Boundary

`vmobj` only carries the object-neutral language layer. It neither modifies page tables directly nor owns object contents directly.

Its main security responsibilities are:

- object identity is never reused or confused;
- mapped view coordinates are stable and self-consistent;
- object-side invalidation work never mixes object coordinates with VMA coordinates.

## Key Constraints

1. `VmObjectId` is used only for stable identity comparison, never for dereferencing.
   It must retain object family information, so that file-backed and anonymous objects
   do not share the same raw integer namespace.
2. `MappingViewRange` stores both:
   - VMA coordinates
   - object coordinates
   Callers must not "guess" one set of coordinates from the other set of fields on their own.
3. An `ObjectViewHit` must fall within the range covered by its `MappingView`.
4. `ObjectInvalidateWork` is only an object-side work item; whether to zap PTEs is still re-checked by `MmSpace` against the current VMA/object relationship.
5. `MappingViewRange` must not be empty or overflow its end offset.
6. The object range of an `ObjectInvalidateWork` must cover every `ObjectViewHit` it contains.
7. Both file-backed and anonymous objects must reuse `ObjectViewHit` /
   `ObjectInvalidateWork`, and must not each define a second invalidation coordinate language.
8. `ObjectInvalidateWork.object()` is the sole source of the object identity derived from a request;
   a notifier must not save another copy of the object id and then recombine it with a hit.

## Audit Checklist

- Whether a new object owner uses `MappingViewSpec` / `MappingView` when registering a view.
- Whether `MappingView::page_hit()` or
  `ObjectViewHit::try_new()` is used when converting an object event into a VMA hit.
- Whether the notifier only delivers `ObjectInvalidateRequest`, rather than carrying a custom range.
- Whether call sites that construct `ObjectViewHit::new()` by hand have proven that the hit lies entirely within the view.
- Whether object-driven invalidation enters `MmSpace` only through `ObjectInvalidateRequest`,
  rather than leaving behind a file/page-index style side-channel entry point.

Live backing-page release requires synchronous notifier success for every
carried view hit. A queued notification is insufficient. Providers that cannot
prove completed PTE/TLB revocation reject `try_invalidate`; busy targets retain
backing ownership while callers release fault-exclusion locks before retrying.
