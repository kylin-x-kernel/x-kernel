# Local range-allocation extension

Source: `arceos-org/flatten_objects`, published crate `flatten_objects` 0.2.4.
The original manifest, authors, license expression, README, implementation and
tests are retained. This import is used under the upstream Apache-2.0 option;
the included `LICENSE` supplies that license text.

Imports and example comment spacing follow the repository formatter.
The only semantic library extension is `FlattenObjects::add_in_range(minimum, end, value)`.
It clips the exclusive end to capacity, checks the inclusive minimum, searches
the existing bitmap with `next_false_index`, and delegates insertion to `add_at`.
No second bitmap, new unsafe code, or changed object lifecycle is introduced.
Failure returns the original object and leaves slots/count unchanged.

The crate remains a standalone upstream-style workspace; kernel code consumes
it as a path dependency. Run its upstream and range tests with
`cargo test --manifest-path third_party/flatten_objects/Cargo.toml`.
