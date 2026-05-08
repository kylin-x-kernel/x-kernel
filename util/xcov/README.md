# xcov

Pure Rust code coverage and profile-guided optimization (PGO) support for `no_std` and embedded programs.

This crate is a **pure Rust reimplementation** of [minicov](https://github.com/Amanieu/minicov), which originally uses a modified C version of the LLVM profiling runtime (from compiler-rt). xcov replaces all C code with idiomatic Rust while providing the same API and producing identical `.profraw` output.

All types of instrumentation using the LLVM profiling runtime are supported:

- Rust code coverage with `-C instrument-coverage`.
- Rust profile-guided optimization with `-C profile-generate`.
- Clang code coverage with `-fprofile-instr-generate -fcoverage-mapping`.
- Clang profile-guided optimization with `-fprofile-instr-generate`.
- Clang LLVM IR profile-guided optimization with `-fprofile-generate`.

Note that to profile both C and Rust code at the same time you must use Clang with the same LLVM version as the LLVM used by rustc. You can pass these flags to C code compiled with the `cc` crate using [environment variables](https://github.com/rust-lang/cc-rs#external-configuration-via-environment-variables).

## Usage

Note: This crate requires a recent nightly compiler.

1. Ensure that the following environment variables are set up:

```sh
export RUSTFLAGS="-Cinstrument-coverage -Zno-profiler-runtime"
```

Note that these flags also apply to build-dependencies and proc macros by default. This can be worked around by explicitly specifying a target when invoking cargo:

```sh
# Applies RUSTFLAGS to everything
cargo build

# Doesn't apply RUSTFLAGS to build dependencies and proc macros
cargo build --target x86_64-unknown-linux-gnu
```

2. Add the `xcov` crate as a dependency to your program:

```toml
[dependencies]
xcov = "0.1"
```

3. Before your program exits, call `xcov::capture_coverage` with a sink (such as `Vec`) and then dump its contents to a file with the `.profraw` extension:

```ignore
fn main() {
    // ...

    let mut coverage = vec![];
    unsafe {
        // Note that this function is not thread-safe! Use a lock if needed.
        xcov::capture_coverage(&mut coverage).unwrap();
    }
    std::fs::write("output.profraw", coverage).unwrap();
}
```

If your program is running on a different system than your build system then you will need to transfer this file back to your build system.

Sinks must implement the `CoverageWriter` trait. If the default `alloc` feature is enabled then an implementation is provided for `Vec<u8>`.

4. Use a tool such as [grcov] or `llvm-cov` to generate a human-readable coverage report:

```sh
grcov output.profraw -b ./target/debug/my_program -s . -t html -o cov_report
```

[grcov]: https://github.com/mozilla/grcov

## Profile-guided optimization

The steps for profile-guided optimization are similar. The only difference is the flags passed in `RUSTFLAGS`:

```sh
# First run to generate profiling information.
#
# The filename passed to profile-generate doesn't matter, but cc-rs complains
# if it is not provided.
export RUSTFLAGS="-Cprofile-generate=output.profraw -Zno-profiler-runtime"
cargo run --target x86_64-unknown-linux-gnu --release

# Post-process the profiling information.
# The rust-profdata tool comes from cargo-binutils.
rust-profdata merge -o output.profdata output.profraw

# Optimized build using PGO. xcov is not needed in this step.
export RUSTFLAGS="-Cprofile-use=output.profdata"
cargo build --target x86_64-unknown-linux-gnu --release
```

## Acknowledgements

This project is a pure Rust reimplementation of [minicov](https://github.com/Amanieu/minicov) by Amanieu d'Antras. The API and profraw output format are designed to be fully compatible.

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0).
