# vmobj — Design Document

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.
## Positioning

`vmobj` provides the object-neutral common language of the memory-management main plane. It owns neither file-backed content nor anonymous content; what it is responsible for is:

- stable object identity;
- object-side mapped view / rmap records;
- object-driven view-hit invalidation work;
- neutral expression of invalidation requests from an object to an `MmSpace`.

Linux counterparts:

- file-backed path: `address_space` / `i_mmap`
- anonymous path: `anon_vma` / rmap

## Scope

- `src/lib.rs`
- It only defines the object/rmap language; it does not define the page cache, the anonymous page owner, the VMA tree, or page-table execution.

## Architecture

```text
kvfs::AddressSpace / anon objects
    -> MappingView / ObjectViewHit / ObjectInvalidateWork
         -> MmSpace apply side
```

The key principles here are:

- `MappingView` is the object-side rmap entry;
- `ObjectInvalidateWork` is the formal invalidation work item issued by the object owner;
- `MmSpace` only consumes this work and does not define its semantics.

## Design Decisions

1. Put `VmObjectId` in a separate crate instead of keeping it under `memspace`
   Reason: it is an object-neutral identity that both file-backed and anonymous objects need to reuse.
   It is now already a typed identity:
   - `VmObjectId::File(FileObjectId)`
   - `VmObjectId::Anon(AnonObjectId)`
   This way a file-backed `Mapping` and an anonymous object no longer fall into the same untyped integer namespace.

2. Put `MappingView` / `MappingViewRange` in a separate crate
   Reason: they describe the object -> mapped VMA relationship, which goes beyond the ownership of `pagecache` itself.

3. Express object-side invalidation with `ObjectViewHit` and `ObjectInvalidateWork`
   Reason: truncate, evict, hole-punch, and collapse-range should all go through a unified view-hit invalidation main path, rather than each issuing a bare range.

4. `MappingViewNotifier` only consumes `ObjectInvalidateWork`
   Reason: the notifier is a consumer of views, not the defining core of a view.

5. `MappingViewId` is allocated uniformly by `mm/vmobj`
   Reason: file-backed `kvfs::AddressSpace` and anonymous objects both need to register on the same
   object-view/rmap language; view identity must no longer be assigned privately by an individual object branch.

6. The coordinate invariants of `MappingViewRange` / `ObjectViewHit` / `ObjectInvalidateWork` are frozen
   inside the `vmobj` crate
   Reason: the core risk of object-driven invalidate is mixing up the object byte range and the VMA byte
   range. `MappingView::new()` now rejects empty or overflowing ranges;
   `ObjectViewHit::try_new()` only accepts object hits that fall entirely within the view;
   `ObjectInvalidateWork::new()` requires the work range to cover all hits. Callers may still use
   `MappingView::page_hit()` for clipping, but should not reinvent a separate
   range language in the file/anon branches.

## Frozen API Contract

- `VmObjectId` only expresses object identity and does not carry a dereferenceable object pointer.
- `MappingView` is the only object -> VMA view record.
- `ObjectViewHit` is the only object event -> VMA hit record.
- `ObjectInvalidateWork` is the batch invalidation work produced by the object owner.
- `ObjectInvalidateRequest` is the single-view request delivered to one `MmSpace` consumer.
- `MappingViewNotifier` may only consume `ObjectInvalidateWork` and the corresponding `ObjectViewHit`;
  the notifier should not define new file/anon range structures of its own.
- `ObjectInvalidateWork` carries a `VmObjectId`, and the notifier must derive an `ObjectInvalidateRequest`
  through `ObjectInvalidateWork::request_for_hit()`;
  the notifier should not separately store the object id and assemble the request by hand.
- Object-hit transformations such as page-aligned trimming are expressed with `ObjectViewHit` helpers,
  for example the page-aligned suffix of a file-backed truncate, and a separate set of views/hits is not reconstructed
  inside `filemap`.

`MappingViewNotifier::try_invalidate` is the synchronous revocation contract for
live backing-store transitions. True means the affected PTEs and TLB entries
are gone, false means the target MM is busy, and errors prohibit page release.
The default rejects this contract: deferred `invalidate` alone cannot prove
completion. Object owners reuse the same work/hit coordinates and registered
views; no second invalidation registry or completion-state table is introduced.
