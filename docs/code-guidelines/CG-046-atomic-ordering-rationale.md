---
id: CG-046-atomic-ordering-rationale
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\bOrdering::(?:Relaxed|Acquire|Release|AcqRel|SeqCst)\b'
  - '\b(?:compare_exchange(?:_weak)?|fence)\s*\('
---

# CG-046-atomic-ordering-rationale: Explain nontrivial atomic memory ordering

- **Rule**: When atomic correctness depends on publication or a memory
  ordering relationship, code SHOULD explain the published data, its observer
  and the relationship in comments or applicable surrounding documentation.
- **Scope**: Nontrivial atomic orderings, compare-exchange operations and
  fences. An adequate type or module argument may cover individual operations.
  Not this rule: [CG-017](CG-017-unsafe-impl-send-sync-argument.md) owns the
  Send/Sync concurrency argument and [CG-016](CG-016-safety-comment-substance.md)
  owns concrete proof for unsafe operations. This rule owns ordering rationale;
  actual ordering sufficiency and bad interleavings remain concurrency review.
- **Why**: An ordering name alone does not identify what is published or
  why another accessor observes it safely.
- **How to check**:
  1. Read the operation and applicable type/module ordering argument.
  2. When correctness depends on publication, identify the data, the observer
     and the ordering relationship. Require the explanation to name them.
  3. Reuse adequate surrounding documentation and distinguish independent counters
     from publication. Do not turn a missing ritual comment into a finding.
- **Exceptions**: Independent counters need no ritual ordering comment. Existing
  adequate type/module rationale needs no repetition at each operation.
- **Counter-example**:

```rust,ignore
ready.store(true, Ordering::Release);
// No applicable explanation identifies the data or acquiring observer.
```

The ordering has no rationale connecting publication to its observer.

- **Example**:

```rust,ignore
// Publishes the initialized payload to readers that observe ready == true
// with Acquire before reading payload; no writer changes it after publication.
ready.store(true, Ordering::Release);
```
