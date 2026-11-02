# kio

Synchronous byte I/O for a `no_std` environment, using X-Kernel's `kerrno`
errors. Start with [`Read`], [`Write`], [`BufRead`], and [`Seek`], or import
[`prelude`] for their extension traits. [`Cursor<T>`] adapts in-memory buffers;
[`BufReader<R>`], [`BufWriter<W>`], and [`LineWriter<W>`] buffer other streams.
[`Read::take`], [`Read::chain`], and [`copy`] compose these interfaces.
[`IoBuf`] and [`IoBufMut`] add remaining-byte queries and single-chunk transfers.
This is a subset of [`std::io`][1], not a file-descriptor or device provider.

## Example

This bounded in-memory transfer needs no allocator, scheduler, or device setup.

```rust
use kio::{Cursor, Read, Seek, Write};

let mut output = Cursor::new([0u8; 4]);
output.write_all(b"rust").unwrap();
output.rewind().unwrap();
let mut prefix = [0u8; 2];
output.read_exact(&mut prefix).unwrap();
assert_eq!(&prefix, b"ru");
assert_eq!(output.position(), 2);
```

Buffered output must be explicitly flushed to observe write errors:

```rust
use kio::{BufReader, BufRead, BufWriter, Cursor, Write};

let mut input = BufReader::with_capacity(4, Cursor::new(b"ok\n")).unwrap();
assert_eq!(input.fill_buf().unwrap(), b"ok\n");
input.consume(3); // Never consume more than the returned slice length.

let mut storage = [0u8; 4];
let mut output = BufWriter::new(Cursor::new(&mut storage[..]));
output.write_all(b"ok").unwrap();
output.flush().unwrap();
let cursor = output.into_inner().unwrap();
assert_eq!(&cursor.get_ref()[..2], b"ok");
```

## Execution and data contracts

Operations call the supplied reader, writer, or closure synchronously and inherit
its blocking and execution-context requirements. `kio` supplies no locks,
timeouts, permissions, or cancellation. Non-allocating memory adapters can run
without a current process; heap-backed operations require an initialized allocator.
Generic streams must be reviewed before use during early boot or in interrupts.

`read` and `write` may make partial progress. Exact/all helpers are not
transactions, and copy success does not flush buffered output. Dropping a buffered
writer attempts to drain it but discards errors; its inner writer may still panic.
Use `flush` to observe errors and `into_parts` to recover pending output without I/O.


[1]: https://doc.rust-lang.org/std/io/index.html

### Features

- **alloc**:
  - Enables extra methods on `Read`: `read_to_end`, `read_to_string`.
  - Enables extra methods on `BufRead`: `read_until`, `read_line`, `split`, `lines`.
  - Enables implementations of kio traits for `alloc` types like `Vec<u8>`, `KBox<T>`, etc.
  - Enables `BufWriter::with_capacity`. (If `alloc` is disabled, only `BufWriter::new` is available.)
  - Removes the capacity limit on `BufReader`. (If `alloc` is disabled, `BufReader::with_capacity` returns `InvalidInput` if the capacity is larger than a fixed limit.)

### Differences to `std::io`

- Error types from `kerrno` instead of `std::io::Error`.
- No `IoSlice` and `*_vectored` APIs.

### Limitations

- Requires nightly Rust.

## License

Apache License 2.0; see the repository license files.

Reader constructors return `Result`. With `alloc`, default constructors require
sleepable, reclaim-safe task context; `with_capacity_with_flags` provides explicit
allocation permissions. Without `alloc`, capacity validation remains fallible
but no allocator is used:

```rust
# #[cfg(not(feature = "alloc"))]
# {
use kio::{BufReader, Cursor, Error, Read};
assert!(matches!(
    BufReader::with_capacity(kio::DEFAULT_BUF_SIZE + 1, Cursor::new(b"")),
    Err(Error::InvalidInput)
));
let mut reader = BufReader::with_capacity(2, Cursor::new(b"abc")).unwrap();
let mut bytes = [0; 3];
reader.read_exact(&mut bytes).unwrap();
assert_eq!(&bytes, b"abc");
assert_eq!(BufReader::with_capacity(0, Cursor::new(b"")).unwrap().capacity(), 0);
# }
```
