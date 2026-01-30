// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt;

use crate::{Error, Result};

mod impls;

pub(crate) fn default_write_fmt<W: Write + ?Sized>(
    this: &mut W,
    args: fmt::Arguments<'_>,
) -> Result<()> {
    struct Adapter<'a, T: ?Sized + 'a> {
        inner: &'a mut T,
        error: Result<()>,
    }

    impl<T: Write + ?Sized> fmt::Write for Adapter<'_, T> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            match self.inner.write_all(s.as_bytes()) {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.error = Err(e);
                    Err(fmt::Error)
                }
            }
        }
    }

    let mut output = Adapter {
        inner: this,
        error: Ok(()),
    };
    match fmt::write(&mut output, args) {
        Ok(()) => Ok(()),
        Err(..) => {
            if output.error.is_err() {
                output.error
            } else {
                panic!(
                    "a formatting trait implementation returned an error when the underlying \
                     stream did not"
                );
            }
        }
    }
}

/// A trait for objects which are byte-oriented sinks.
///
/// See [`std::io::Write`] for more details.
pub trait Write {
    fn write(&mut self, buf: &[u8]) -> Result<usize>;

    fn flush(&mut self) -> Result<()>;

    fn write_all(&mut self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.write(buf) {
                Ok(0) => return Err(Error::WriteZero),
                Ok(n) => buf = &buf[n..],
                Err(e) if e.canonicalize() == Error::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<()> {
        default_write_fmt(self, args)
    }

    fn by_ref(&mut self) -> &mut Self
    where
        Self: Sized,
    {
        self
    }
}
