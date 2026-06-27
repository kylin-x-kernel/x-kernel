// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! xcover — coverage / PGO runtime for `no_std` and embedded programs.
//!
//! Provides a safe Rust API for profile data capture, merge, and reset.
//! ABI symbols required by LLVM instrumentation are exported as
//! `#[unsafe(no_mangle)]` but are not part of the Rust API surface.

#![no_std]

// Internal modules — not publicly accessible.
mod abi;
mod image;
mod merge;
mod parse;
mod record;
mod runtime;
mod serialize;
mod state;
mod sync;
mod value;

// Platform module still needed for section access by abi layer.
mod platform;

// === Public API ===

/// Returns `true` if code coverage instrumentation is enabled
/// (the current image contains profiling data sections).
pub fn is_enabled() -> bool {
    #[cfg(feature = "alloc")]
    {
        runtime::has_image()
    }
    #[cfg(not(feature = "alloc"))]
    {
        false
    }
}

/// Trait for writing profile data to an arbitrary destination.
pub trait ProfileWriter {
    /// Writes all bytes from `bytes`, or returns an error.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), ProfileError>;
}

/// Structured error type for profile operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileError {
    /// Coverage instrumentation is not enabled.
    NotEnabled,
    /// Input data is malformed.
    MalformedInput,
    /// Input data is incompatible with the current binary.
    IncompatibleInput,
    /// Output buffer is too small.
    OutputTooSmall,
    /// The writer callback failed.
    WriterFailed,
    /// The runtime is busy (concurrent access).
    RuntimeBusy,
    /// Arithmetic overflow during size calculation.
    ArithmeticOverflow,
}

impl core::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProfileError::NotEnabled => f.write_str("coverage instrumentation not enabled"),
            ProfileError::MalformedInput => f.write_str("malformed profile input"),
            ProfileError::IncompatibleInput => f.write_str("incompatible profile input"),
            ProfileError::OutputTooSmall => f.write_str("output buffer too small"),
            ProfileError::WriterFailed => f.write_str("writer callback failed"),
            ProfileError::RuntimeBusy => f.write_str("profile runtime busy"),
            ProfileError::ArithmeticOverflow => f.write_str("arithmetic overflow"),
        }
    }
}

/// Captures the current profile data and writes it in LLVM `.profraw`
/// format to the given writer.
///
/// This function is not thread-safe: concurrent calls or concurrent execution
/// of instrumented code may produce inaccurate profile data.
#[cfg(feature = "alloc")]
pub fn write_profraw(writer: &mut dyn ProfileWriter) -> Result<(), ProfileError> {
    let snapshot = runtime::snapshot().ok_or(ProfileError::NotEnabled)?;
    serialize::encode(&snapshot, writer)
}

/// Merges the given profile data (in `.profraw` format) into the current
/// runtime counters.
///
/// Concurrent calls and concurrent execution of instrumented code are
/// sound: all mutations go through atomic operations on the live image.
#[cfg(feature = "alloc")]
pub fn merge_profraw(bytes: &[u8]) -> Result<(), ProfileError> {
    let parsed = parse::parse_profraw(bytes)?;

    let image = runtime::image_or_init()?;

    // Check compatibility using the already-parsed data.
    parse::check_compatibility(image, &parsed)?;

    // Merge.
    merge::merge(image, &parsed)
}

/// Resets all profiling counters to their initial state.
///
/// Concurrent calls and concurrent execution of instrumented code are
/// sound: reset is implemented via atomic stores.
pub fn reset() {
    #[cfg(feature = "alloc")]
    runtime::reset();
}

/// Returns a signature value unique to the current load module.
#[cfg(feature = "alloc")]
pub fn module_signature() -> u64 {
    match runtime::image_or_init() {
        Ok(image) => merge::get_load_module_signature(image),
        Err(_) => 0,
    }
}

#[cfg(not(feature = "alloc"))]
pub fn module_signature() -> u64 {
    0
}

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
impl ProfileWriter for alloc::vec::Vec<u8> {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), ProfileError> {
        self.extend_from_slice(bytes);
        Ok(())
    }
}
