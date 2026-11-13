---
id: CG-031-intrusive-linked-lists
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [linkedlist]
---

# CG-031-intrusive-linked-lists: Prefer the project intrusive collection library

- **Rule**: Code SHOULD use alloc::collections::LinkedList only when elements cannot carry their own links.
- **Scope**: Linked-list selection in Rust kernel code.
- **Why**: The project-standard `intrusive-collections` crate avoids separate link-node allocation when intrusive ownership is possible.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. For each LinkedList hit, check whether `intrusive_collections::LinkedList` can represent the element ownership and lifecycle.
- **Exceptions**: The reason must explain why the element cannot carry links and why `intrusive-collections` is unsuitable. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
let queue = alloc::collections::LinkedList::<Entry>::new();
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
// Entry owns its list link; the adapter maps the link to the element.
intrusive_adapter!(EntryAdapter = Box<Entry>: Entry { link => LinkedListLink });
let queue = intrusive_collections::LinkedList::new(EntryAdapter::NEW);
```

Inserting an element that is already linked panics, and `cursor_mut_from_ptr`
requires the element to be linked in that exact list; prove both at each use.
