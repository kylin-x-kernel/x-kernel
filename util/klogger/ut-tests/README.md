# Kernel Unit Tests

`lib.rs` retains the logger parsing and level-update tests. The crate root
mounts it with `unittest::path_mod_test!` under `cfg(unittest)`.
