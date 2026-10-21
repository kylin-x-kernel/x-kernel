# DICE Host Tests

`runner.rs` is an automatically discovered Cargo integration target. Cases
live in `cases/`, and shared input vectors live in `support/`. All 19 original
host cases use public DICE interfaces; the library no longer mounts test
modules or exports test vectors. Android handover wire labels are explicit
test expectations rather than imports of private implementation constants.

Run `cargo test -p xdice --test runner` in a bare host Cargo environment.
In a kernel-configured worktree, clear `RUSTFLAGS` and
`CARGO_ENCODED_RUSTFLAGS` and select the host target explicitly. No Kconfig
allocator or kernel scheduler is needed.
