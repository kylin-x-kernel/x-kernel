# Performance And Resources

Use this file when the change touches hot paths,
allocation behavior, copying, ownership, or cleanup.

## Performance

- avoid O(n) algorithms on hot paths unless justified;
- avoid casual atomics in frequently executed code;
- minimize unnecessary copies and allocations;
- do not optimize prematurely without evidence,
  but do not ignore obvious structural costs either;
- make semantic costs visible in naming and API shape.
- when claiming a performance improvement,
  be prepared to justify it with evidence.

## Resource Management

- use RAII and ownership-based cleanup by default;
- avoid lifetime splits that make cleanup obligations ambiguous;
- keep acquire/release symmetry easy to audit;
- when a resource must outlive a scope boundary,
  make that ownership transfer explicit.

## When Reviewing

Check specifically for:

- linear scans in interrupt, scheduler, packet, or page-fault adjacent paths;
- hidden heap allocation in frequently repeated operations;
- redundant copies of large buffers or descriptors;
- cleanup paths that depend on manual caller discipline
  when ownership could encode the rule directly.
