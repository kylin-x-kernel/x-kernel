---
id: CG-048-direct-owner-conversion
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\binto_boxed_(?:slice|str)\b'
  - '\b(?:Box|KBox)\s*::\s*into_inner\b'
  - '\b(?:Box|KBox|Arc|Rc|KArc)\s*::\s*new(?:_nowait|_with_flags)?\s*\(\s*\*'
---

# CG-048-direct-owner-conversion: Avoid temporary owners used only for conversion

- **Rule**: Code SHOULD construct the final owner directly instead of allocating
  an intermediate owner used only to transfer the same value into it, when an
  existing safe API preserves the required ownership and failure contract.
- **Scope**: Only these local conversion chains, including an adjacent,
  single-use temporary binding, in changed Rust production or test code:
  1. Standard `Vec<T> -> Box<[T]> -> Arc<[T]>` or `Rc<[T]>`.
  2. Standard `String -> Box<str> -> Arc<str>` or `Rc<str>`.
  3. A freshly constructed sized `Box<T>` or `KBox<T>` immediately unpacked
     into another owner of the same `T`, with no intervening use or escape.
  Cases 1 and 2 cover only the default allocator. Case 3 requires checking
  the concrete destination constructor; it does not assume `From<KBox<T>>`
  exists for `Arc` or `KArc`. Other container optimizations are out of scope.
- **Why**: A boxed slice/string can add a shrink step before the shared owner
  allocates its own storage. Unpacking a freshly allocated sized owner into
  another allocation can also introduce unnecessary allocation and release.
  These are redundant ownership transitions, not proof of a measured slowdown.
- **How to check**:
  1. Treat trigger matches as candidates, not violations. Resolve the actual
     types, methods and final payload type; read the local producer and consumer.
     Matching is line-based, so triggers match individual operation names rather
     than requiring the whole conversion chain on one line.
     A destination constructor receiving `*value` is also only a candidate:
     copying a borrowed value or consuming a non-temporary owner is not proof
     of a redundant allocation.
  2. For cases 1 and 2, confirm the pinned standard library provides the direct
     `From<Vec<T>>` or `From<String>` implementation for the final owner. Suggest
     that conversion without moving it across locks or changing its source.
  3. For case 3, prove the intermediate owner was created in this local chain,
     never published, borrowed for an operation, pinned, queued or passed through
     an ownership-taking API. Identify the existing direct constructor.
  4. For `KBox`, compare allocation flags, fallibility, error mapping, evaluation
     order, capture destruction and resource admission. Removing a redundant
     allocation may remove its OOM opportunity; it must not turn the remaining
     allocation infallible, permit reclaim, or move failure past admission.
     If this cannot be established, do not report a violation.
  5. Report the source/intermediate/destination types and the direct replacement.
     Do not claim zero allocation, guaranteed extra copying or a speedup. No
     hot-path evidence is needed for this narrowly defined SHOULD rule.
- **Exceptions**: Keep owners required by an API, stable-address or DST contract,
  reservation-before-admission protocol, or independent lifetime. An existing
  owner recovered after task execution, RCU retirement or resource acquisition
  is not a freshly allocated temporary. `Arc<Box<T>>` and `Arc<KBox<T>>` retain
  the inner owner and are not equivalent to `Arc<T>`. Custom allocators,
  similarly named custom methods, and tests intentionally exercising owner
  conversion/drop/failure behavior are excluded. Lack of a safe direct API is
  not a reason to introduce raw-pointer conversions or redesign a subsystem.
- **Counter-example**:

```rust,ignore
let shared: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
let temporary = KBox::new(value)?;
let shared_value = KArc::new(KBox::into_inner(temporary))?;
let temporary_box = Box::new(other_value);
let other_shared = Arc::new(*temporary_box);
```

The intermediate owners have no use beyond conversion; the second example
assumes identical error propagation and default allocation policy, with no
resource admission between the two constructors.

- **Example**:

```rust,ignore
let shared: Arc<[u8]> = Arc::from(bytes);
let shared_value = KArc::new(value)?;
let other_shared = Arc::new(other_value);
```
