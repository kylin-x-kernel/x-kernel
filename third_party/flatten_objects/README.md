# flatten_objects

[![Crates.io](https://img.shields.io/crates/v/flatten_objects)](https://crates.io/crates/flatten_objects)
[![Docs.rs](https://docs.rs/flatten_objects/badge.svg)](https://docs.rs/flatten_objects)
[![CI](https://github.com/arceos-org/flatten_objects/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/arceos-org/flatten_objects/actions/workflows/ci.yml)

`FlattenObjects` is a container that stores numbered objects.

Objects can be added to the `FlattenObjects`, a unique ID will be assigned
to the object. The ID can be used to retrieve the object later.

# Example

```rust
use flatten_objects::FlattenObjects;

let mut objects = FlattenObjects::<u32, 20>::new();

// Add `23` 10 times and assign them IDs from 0 to 9.
for i in 0..=9 {
    objects.add_at(i, 23).unwrap();
    assert!(objects.is_assigned(i));
}

// Remove the object with ID 6.
assert_eq!(objects.remove(6), Some(23));
assert!(!objects.is_assigned(6));

// Add `42` (the ID 6 is available now).
let id = objects.add(42).unwrap();
assert_eq!(id, 6);
assert!(objects.is_assigned(id));
assert_eq!(objects.get(id), Some(&42));
assert_eq!(objects.remove(id), Some(42));
assert!(!objects.is_assigned(id));
```
