# kerrno

[![Crates.io](https://img.shields.io/crates/v/kerrno)](https://crates.io/crates/kerrno)
[![Docs.rs](https://docs.rs/kerrno/badge.svg)](https://docs.rs/kerrno)
[![CI](https://github.com/arceos-org/kerrno/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/arceos-org/kerrno/actions/workflows/ci.yml)

Generic error code representation.

It provides two error types and the corresponding result types:

- [`AxError`] and [`AxResult`]: A generic error type similar to
  [`std::io::ErrorKind`].
- [`LinuxError`] and [`LinuxResult`]: Linux specific error codes defined in
  `errno.h`. It can be converted from [`AxError`].

[`AxError`]: https://docs.rs/kerrno/latest/kerrno/enum.AxError.html
[`AxResult`]: https://docs.rs/kerrno/latest/kerrno/type.AxResult.html
[`LinuxError`]: https://docs.rs/kerrno/latest/kerrno/enum.LinuxError.html
[`LinuxResult`]: https://docs.rs/kerrno/latest/kerrno/type.LinuxResult.html
[`std::io::ErrorKind`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html
