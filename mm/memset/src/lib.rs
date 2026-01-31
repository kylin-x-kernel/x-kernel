// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual memory set management and mapping utilities.
#![cfg_attr(not(test), no_std)]
extern crate alloc;

mod area;
mod backend;
mod set;

pub use self::{area::MemoryArea, backend::MemorySetBackend, set::MemorySet};

/// Error type for memory set operations.
#[derive(Debug, Eq, PartialEq)]
pub enum MemorySetError {
    /// Invalid parameter (e.g., `addr`, `size`, `flags`, etc.)
    InvalidParam,
    /// The given range overlaps with an existing mapping.
    AlreadyExists,
    /// The backend page table is in a bad state.
    BadState,
}

impl From<MemorySetError> for kerrno::KError {
    fn from(err: MemorySetError) -> Self {
        match err {
            MemorySetError::InvalidParam => kerrno::KError::InvalidInput,
            MemorySetError::AlreadyExists => kerrno::KError::AlreadyExists,
            MemorySetError::BadState => kerrno::KError::BadState,
        }
    }
}

/// A [`Result`] type with [`MemorySetError`] as the error type.
pub type MemorySetResult<T = ()> = Result<T, MemorySetError>;
