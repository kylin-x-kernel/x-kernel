# kio

[`std::io`][1] for `no_std` environment.

[1]: https://doc.rust-lang.org/std/io/index.html

### Features

- **alloc**:
  - Enables extra methods on `Read`: `read_to_end`, `read_to_string`.
  - Enables extra methods on `BufRead`: `read_until`, `read_line`, `split`, `lines`.
  - Enables implementations of kio traits for `alloc` types like `Vec<u8>`, `Box<T>`, etc.
  - Enables `BufWriter::with_capacity`. (If `alloc` is disabled, only `BufWriter::new` is available.)
  - Removes the capacity limit on `BufReader`. (If `alloc` is disabled, `BufReader::with_capacity` will panic if the capacity is larger than a fixed limit.)

### Differences to `std::io`

- Error types from `axerrno` instead of `std::io::Error`.
- No `IoSlice` and `*_vectored` APIs.

### Limitations

- Requires nightly Rust.

## License

Apache License 2.0 - see [LICENSE](../../LICENSE) for details.