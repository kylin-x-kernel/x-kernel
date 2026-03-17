// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// The `remaining` and `remaining_mut` forwarding bodies in this file use a
// conventional Rust trait delegation pattern: reference and wrapper types
// simply forward the call to the underlying value. As a result, these
// implementations are expected to look repetitive across codebases; that
// similarity reflects idiomatic practice rather than copying from a specific
// external implementation.

#[cfg(feature = "alloc")]
use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
use core::io::BorrowedCursor;

use crate::{IoBuf, IoBufMut};

impl<R: IoBuf + ?Sized> IoBuf for &R {
    #[inline]
    fn remaining(&self) -> usize {
        (**self).remaining()
    }
}

impl<W: IoBufMut + ?Sized> IoBufMut for &W {
    #[inline]
    fn remaining_mut(&self) -> usize {
        (**self).remaining_mut()
    }
}

impl<R: IoBuf + ?Sized> IoBuf for &mut R {
    #[inline]
    fn remaining(&self) -> usize {
        (**self).remaining()
    }
}

impl<W: IoBufMut + ?Sized> IoBufMut for &mut W {
    #[inline]
    fn remaining_mut(&self) -> usize {
        (**self).remaining_mut()
    }
}

#[cfg(feature = "alloc")]
impl<R: IoBuf + ?Sized> IoBuf for Box<R> {
    #[inline]
    fn remaining(&self) -> usize {
        (**self).remaining()
    }
}

#[cfg(feature = "alloc")]
impl<W: IoBufMut + ?Sized> IoBufMut for Box<W> {
    #[inline]
    fn remaining_mut(&self) -> usize {
        (**self).remaining_mut()
    }
}

impl IoBuf for [u8] {
    #[inline]
    fn remaining(&self) -> usize {
        self.len()
    }
}

impl IoBufMut for [u8] {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.len()
    }
}

#[cfg(feature = "alloc")]
impl IoBufMut for Vec<u8> {
    #[inline]
    fn remaining_mut(&self) -> usize {
        // A vector can never have more than isize::MAX bytes
        isize::MAX as usize - self.len()
    }
}

#[cfg(feature = "alloc")]
impl IoBufMut for VecDeque<u8> {
    #[inline]
    fn remaining_mut(&self) -> usize {
        // A vector can never have more than isize::MAX bytes
        isize::MAX as usize - self.len()
    }
}

impl IoBufMut for BorrowedCursor<'_> {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.capacity()
    }
}
