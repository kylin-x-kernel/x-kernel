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

impl From<MemorySetError> for axerrno::AxError {
    fn from(err: MemorySetError) -> Self {
        match err {
            MemorySetError::InvalidParam => axerrno::AxError::InvalidInput,
            MemorySetError::AlreadyExists => axerrno::AxError::AlreadyExists,
            MemorySetError::BadState => axerrno::AxError::BadState,
        }
    }
}

/// A [`Result`] type with [`MemorySetError`] as the error type.
pub type MemorySetResult<T = ()> = Result<T, MemorySetError>;
