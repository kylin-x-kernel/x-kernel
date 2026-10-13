// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel error kinds and Linux errno conversion without heap allocation.
//!
//! [`KError`] stores positive kernel-kind codes and negative Linux errno codes.
//! [`KError::code`] is this internal tagged representation, not a syscall return
//! value: syscall adapters convert through [`LinuxError`] and negate its code.
//! [`KResult`] is the common result alias; [`k_err!`] and [`k_err_type!`] also
//! emit warning logs. [`KError::canonicalize`] normalizes mapped Linux errors,
//! but conversion is lossy where several kernel kinds share one Linux code.
//!
//! Use [`KError::try_from_i32`] for untrusted serialized codes. The re-exported
//! [`LinuxError::new`] is unchecked; passing zero or a negative errno to
//! `KError::from` can violate the representation expected by formatting and
//! conversion routines. Neither type performs authorization or user-memory I/O.

// Kernel error types and errno conversions.
//
// # Code Similarity Compliance Notice
//
// This module implements standard POSIX errno error codes and their Rust
// idiomatic representations. Code similarities with other projects are due to:
//
// 1. **Standard Error Code Definitions**: POSIX/Linux errno values are
//    industry-standard constants (e.g., EINVAL, EACCES, EPERM) that must
//    match across all implementations for compatibility.
//
// 2. **Rust Idiomatic Patterns**: Common Rust patterns for error handling,
//    including:
//    - `From`/`TryFrom` trait implementations for error conversions
//    - Enum-based error kinds with discriminants
//    - Display/Debug implementations following std::io::Error conventions
//    - Result type aliases (`KResult<T>`)
//
// 3. **Design References**: Implementation inspired by and compatible with:
//    - [std::io::ErrorKind] https://doc.rust-lang.org/std/io/enum.ErrorKind.html
//    - [ArceOS axerrno crate] SPDX-License-Identifier: Apache-2.0
//    - Linux kernel errno definitions
//
// These similarities are **expected and necessary** for:
// - Cross-platform error code compatibility
// - Idiomatic Rust error handling conventions
// - Standard library API consistency
//
// The code patterns (enum definitions, trait implementations, macro patterns)
// are common Rust idioms and not subject to copyright as functional requirements.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use core::fmt;

pub use linux_sysno::Errno as LinuxError;
use strum::EnumCount;

/// The error kind type used by x-kernel.
///
/// Similar to [`std::io::ErrorKind`].
///
/// [`std::io::ErrorKind`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html
#[repr(i32)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, EnumCount)]
pub enum KErrorKind {
    /// A socket address could not be bound because the address is already in use elsewhere.
    AddrInUse = 1,
    /// The socket is already connected.
    AlreadyConnected,
    /// An entity already exists, often a file.
    AlreadyExists,
    /// Program argument list too long.
    ArgumentListTooLong,
    /// Bad address.
    BadAddress,
    /// Bad file descriptor.
    BadFileDescriptor,
    /// Bad internal state.
    BadState,
    /// Broken pipe
    BrokenPipe,
    /// The connection was refused by the remote server.
    ConnectionRefused,
    /// The connection was reset by the remote server.
    ConnectionReset,
    /// Cross-device or cross-filesystem (hard) link or rename.
    CrossesDevices,
    /// A non-empty directory was specified where an empty directory was expected.
    DirectoryNotEmpty,
    /// Loop in the filesystem or IO subsystem; often, too many levels of
    /// symbolic links.
    FilesystemLoop,
    /// Illegal byte sequence.
    IllegalBytes,
    /// The operation was partially successful and needs to be checked later on
    /// due to not blocking.
    InProgress,
    /// This operation was interrupted.
    Interrupted,
    /// Data not valid for the operation were encountered.
    ///
    /// Unlike [`InvalidInput`], this typically means that the operation
    /// parameters were valid, however the error was caused by malformed
    /// input data.
    ///
    /// For example, a function that reads a file into a string will error with
    /// `InvalidData` if the file's contents are not valid UTF-8.
    ///
    /// [`InvalidInput`]: KErrorKind::InvalidInput
    InvalidData,
    /// Invalid executable format.
    InvalidExecutable,
    /// Invalid parameter/argument.
    InvalidInput,
    /// Input/output error.
    Io,
    /// The filesystem object is, unexpectedly, a directory.
    IsADirectory,
    /// Filename is too long.
    NameTooLong,
    /// Not enough space/cannot allocate memory.
    NoMemory,
    /// No such device.
    NoSuchDevice,
    /// No such process.
    NoSuchProcess,
    /// A filesystem object is, unexpectedly, not a directory.
    NotADirectory,
    /// The specified entity is not a socket.
    NotASocket,
    /// Not a typewriter.
    NotATty,
    /// The network operation failed because it was not connected yet.
    NotConnected,
    /// The requested entity is not found.
    NotFound,
    /// Operation not permitted.
    OperationNotPermitted,
    /// Operation not supported.
    OperationNotSupported,
    /// Result out of range.
    OutOfRange,
    /// The operation lacked the necessary privileges to complete.
    PermissionDenied,
    /// The filesystem or storage medium is read-only, but a write operation was attempted.
    ReadOnlyFilesystem,
    /// Device or resource is busy.
    ResourceBusy,
    /// The underlying storage (typically, a filesystem) is full.
    StorageFull,
    /// The I/O operation’s timeout expired, causing it to be canceled.
    TimedOut,
    /// The process has too many files open.
    TooManyOpenFiles,
    /// An error returned when an operation could not be completed because an
    /// "end of file" was reached prematurely.
    UnexpectedEof,
    /// This operation is unsupported or unimplemented.
    Unsupported,
    /// The operation needs to block to complete, but the blocking operation was
    /// requested to not occur.
    WouldBlock,
    /// An error returned when an operation could not be completed because a
    /// call to `write()` returned [`Ok(0)`](Ok).
    WriteZero,
    /// The connection was aborted.
    ConnectionAborted,
    /// File is too large for the filesystem or VFS limit.
    FileTooLarge,
    /// An opted-in file wait was interrupted by a signal.
    ///
    /// Defaults to EINTR. This does not establish that an enclosing transfer
    /// made no progress; its owner must preserve completed bytes first.
    IoWaitInterrupted,
}

impl KErrorKind {
    /// Returns the error description.
    pub fn as_str(&self) -> &'static str {
        use KErrorKind::*;
        match *self {
            AddrInUse => "Address in use",
            AlreadyConnected => "Already connected",
            AlreadyExists => "Entity already exists",
            ArgumentListTooLong => "Argument list too long",
            BadAddress => "Bad address",
            BadFileDescriptor => "Bad file descriptor",
            BadState => "Bad internal state",
            BrokenPipe => "Broken pipe",
            ConnectionAborted => "Connection aborted",
            ConnectionRefused => "Connection refused",
            ConnectionReset => "Connection reset",
            CrossesDevices => "Cross-device link or rename",
            DirectoryNotEmpty => "Directory not empty",
            FilesystemLoop => "Filesystem loop or indirection limit",
            IllegalBytes => "Illegal byte sequence",
            InProgress => "Operation in progress",
            Interrupted => "Operation interrupted",
            InvalidData => "Invalid data",
            InvalidExecutable => "Invalid executable format",
            InvalidInput => "Invalid input parameter",
            Io => "I/O error",
            IsADirectory => "Is a directory",
            NameTooLong => "Filename too long",
            NoMemory => "Out of memory",
            NoSuchDevice => "No such device",
            NoSuchProcess => "No such process",
            NotADirectory => "Not a directory",
            NotASocket => "Not a socket",
            NotATty => "Inappropriate ioctl for device",
            NotConnected => "Not connected",
            NotFound => "Entity not found",
            OperationNotPermitted => "Operation not permitted",
            OperationNotSupported => "Operation not supported",
            OutOfRange => "Result out of range",
            PermissionDenied => "Permission denied",
            ReadOnlyFilesystem => "Read-only filesystem",
            ResourceBusy => "Resource busy",
            StorageFull => "No storage space",
            TimedOut => "Timed out",
            TooManyOpenFiles => "Too many open files",
            UnexpectedEof => "Unexpected end of file",
            Unsupported => "Operation not supported",
            WouldBlock => "Operation would block",
            WriteZero => "Write zero",
            FileTooLarge => "File too large",
            IoWaitInterrupted => "File wait interrupted",
        }
    }

    /// Returns the internal code: positive kernel kind or negative Linux errno.
    ///
    /// Convert through `LinuxError` before producing a Linux syscall result.
    pub const fn code(self) -> i32 {
        self as i32
    }

    #[inline]
    const fn from_code(value: i32) -> Option<Self> {
        use KErrorKind::*;

        Some(match value {
            1 => AddrInUse,
            2 => AlreadyConnected,
            3 => AlreadyExists,
            4 => ArgumentListTooLong,
            5 => BadAddress,
            6 => BadFileDescriptor,
            7 => BadState,
            8 => BrokenPipe,
            9 => ConnectionRefused,
            10 => ConnectionReset,
            11 => CrossesDevices,
            12 => DirectoryNotEmpty,
            13 => FilesystemLoop,
            14 => IllegalBytes,
            15 => InProgress,
            16 => Interrupted,
            17 => InvalidData,
            18 => InvalidExecutable,
            19 => InvalidInput,
            20 => Io,
            21 => IsADirectory,
            22 => NameTooLong,
            23 => NoMemory,
            24 => NoSuchDevice,
            25 => NoSuchProcess,
            26 => NotADirectory,
            27 => NotASocket,
            28 => NotATty,
            29 => NotConnected,
            30 => NotFound,
            31 => OperationNotPermitted,
            32 => OperationNotSupported,
            33 => OutOfRange,
            34 => PermissionDenied,
            35 => ReadOnlyFilesystem,
            36 => ResourceBusy,
            37 => StorageFull,
            38 => TimedOut,
            39 => TooManyOpenFiles,
            40 => UnexpectedEof,
            41 => Unsupported,
            42 => WouldBlock,
            43 => WriteZero,
            44 => ConnectionAborted,
            45 => FileTooLarge,
            46 => IoWaitInterrupted,
            _ => return None,
        })
    }
}

impl TryFrom<i32> for KErrorKind {
    type Error = i32;

    #[inline]
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        KErrorKind::from_code(value).ok_or(value)
    }
}

impl fmt::Display for KErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl From<KErrorKind> for LinuxError {
    fn from(e: KErrorKind) -> Self {
        use KErrorKind::*;
        match e {
            AddrInUse => LinuxError::EADDRINUSE,
            AlreadyConnected => LinuxError::EISCONN,
            AlreadyExists => LinuxError::EEXIST,
            ArgumentListTooLong => LinuxError::E2BIG,
            BadAddress | BadState => LinuxError::EFAULT,
            BadFileDescriptor => LinuxError::EBADF,
            BrokenPipe => LinuxError::EPIPE,
            ConnectionAborted => LinuxError::ECONNABORTED,
            ConnectionRefused => LinuxError::ECONNREFUSED,
            ConnectionReset => LinuxError::ECONNRESET,
            CrossesDevices => LinuxError::EXDEV,
            DirectoryNotEmpty => LinuxError::ENOTEMPTY,
            FileTooLarge => LinuxError::EFBIG,
            FilesystemLoop => LinuxError::ELOOP,
            IllegalBytes => LinuxError::EILSEQ,
            InProgress => LinuxError::EINPROGRESS,
            Interrupted | IoWaitInterrupted => LinuxError::EINTR,
            InvalidExecutable => LinuxError::ENOEXEC,
            InvalidInput | InvalidData => LinuxError::EINVAL,
            Io => LinuxError::EIO,
            IsADirectory => LinuxError::EISDIR,
            NameTooLong => LinuxError::ENAMETOOLONG,
            NoMemory => LinuxError::ENOMEM,
            NoSuchDevice => LinuxError::ENODEV,
            NoSuchProcess => LinuxError::ESRCH,
            NotADirectory => LinuxError::ENOTDIR,
            NotASocket => LinuxError::ENOTSOCK,
            NotATty => LinuxError::ENOTTY,
            NotConnected => LinuxError::ENOTCONN,
            NotFound => LinuxError::ENOENT,
            OperationNotPermitted => LinuxError::EPERM,
            OperationNotSupported => LinuxError::EOPNOTSUPP,
            OutOfRange => LinuxError::ERANGE,
            PermissionDenied => LinuxError::EACCES,
            ReadOnlyFilesystem => LinuxError::EROFS,
            ResourceBusy => LinuxError::EBUSY,
            StorageFull => LinuxError::ENOSPC,
            TimedOut => LinuxError::ETIMEDOUT,
            TooManyOpenFiles => LinuxError::EMFILE,
            UnexpectedEof | WriteZero => LinuxError::EIO,
            Unsupported => LinuxError::ENOSYS,
            WouldBlock => LinuxError::EAGAIN,
        }
    }
}

impl TryFrom<LinuxError> for KErrorKind {
    type Error = LinuxError;

    fn try_from(e: LinuxError) -> Result<Self, Self::Error> {
        use KErrorKind::*;
        Ok(match e {
            LinuxError::EADDRINUSE => AddrInUse,
            LinuxError::EISCONN => AlreadyConnected,
            LinuxError::EEXIST => AlreadyExists,
            LinuxError::E2BIG => ArgumentListTooLong,
            LinuxError::EFAULT => BadAddress,
            LinuxError::EBADF => BadFileDescriptor,
            LinuxError::EPIPE => BrokenPipe,
            LinuxError::ECONNABORTED => ConnectionAborted,
            LinuxError::ECONNREFUSED => ConnectionRefused,
            LinuxError::ECONNRESET => ConnectionReset,
            LinuxError::EXDEV => CrossesDevices,
            LinuxError::ENOTEMPTY => DirectoryNotEmpty,
            LinuxError::EFBIG => FileTooLarge,
            LinuxError::ELOOP => FilesystemLoop,
            LinuxError::EILSEQ => IllegalBytes,
            LinuxError::EINPROGRESS => InProgress,
            LinuxError::EINTR => Interrupted,
            LinuxError::ENOEXEC => InvalidExecutable,
            LinuxError::EINVAL => InvalidInput,
            LinuxError::EIO => Io,
            LinuxError::EISDIR => IsADirectory,
            LinuxError::ENAMETOOLONG => NameTooLong,
            LinuxError::ENOMEM => NoMemory,
            LinuxError::ENODEV => NoSuchDevice,
            LinuxError::ESRCH => NoSuchProcess,
            LinuxError::ENOTDIR => NotADirectory,
            LinuxError::ENOTSOCK => NotASocket,
            LinuxError::ENOTTY => NotATty,
            LinuxError::ENOTCONN => NotConnected,
            LinuxError::ENOENT => NotFound,
            LinuxError::EPERM => OperationNotPermitted,
            LinuxError::EOPNOTSUPP => OperationNotSupported,
            LinuxError::ERANGE => OutOfRange,
            LinuxError::EACCES => PermissionDenied,
            LinuxError::EROFS => ReadOnlyFilesystem,
            LinuxError::EBUSY => ResourceBusy,
            LinuxError::ENOSPC => StorageFull,
            LinuxError::ETIMEDOUT => TimedOut,
            LinuxError::EMFILE => TooManyOpenFiles,
            LinuxError::ENOSYS => Unsupported,
            LinuxError::EAGAIN => WouldBlock,
            _ => {
                return Err(e);
            }
        })
    }
}

/// A signed carrier distinguishing kernel kinds from Linux errno values.
///
/// Positive values represent [`KErrorKind`]; negative values represent Linux
/// errno. Construct from a kernel kind, a positive Linux errno, or the checked
/// [`Self::try_from_i32`] decoder. Unknown positive Linux errno values can be
/// retained by `From<LinuxError>`, even though the checked decoder rejects them.
///
/// # Panics
///
/// `From<LinuxError>` may overflow when given `i32::MIN` in checked builds.
/// Zero or negative values created with `LinuxError::new` may create an invalid
/// carrier; formatting, canonicalization, and conversions can then panic or
/// reinterpret it as a kernel kind. Use positive errno values at that boundary.
#[repr(transparent)]
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct KError(i32);

enum KErrorData {
    Ky(KErrorKind),
    Linux(LinuxError),
}

impl KError {
    const fn new_ax(kind: KErrorKind) -> Self {
        KError(kind.code())
    }

    fn new_linux(kind: LinuxError) -> Self {
        KError(-kind.into_raw())
    }

    fn data(&self) -> KErrorData {
        if self.0 < 0 {
            KErrorData::Linux(LinuxError::new(-self.0))
        } else {
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: Positive codes are stored only \
                          from KErrorKind::code or after try_from_i32 finds a matching \
                          KErrorKind, so every positive value decodes to a kind"
            )]
            let kind = KErrorKind::from_code(self.0)
                .unwrap_or_else(|| panic!("invalid positive KError code: {}", self.0));
            KErrorData::Ky(kind)
        }
    }

    /// Returns the internal code: positive kernel kind or negative Linux errno.
    ///
    /// Convert through `LinuxError` before producing a Linux syscall result.
    pub const fn code(self) -> i32 {
        self.0
    }

    /// Returns a canonicalized version of this error.
    ///
    /// This method tries to convert [`LinuxError`] variants into their
    /// corresponding [`KErrorKind`] variants if possible. Unmapped Linux errors
    /// remain unchanged. This does not recover distinctions lost when several
    /// kernel kinds were converted to the same Linux errno.
    ///
    /// # Panics
    ///
    /// Panics if an unchecked `LinuxError` conversion created an invalid
    /// non-negative carrier; see the [`KError`] construction contract.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kerrno::{KError, KErrorKind, LinuxError};
    /// let linux_err = KError::from(LinuxError::EACCES);
    /// let canonical_err = linux_err.canonicalize();
    /// assert_eq!(canonical_err, KError::from(KErrorKind::PermissionDenied));
    /// ```
    pub fn canonicalize(self) -> Self {
        KErrorKind::try_from(self).map_or_else(Into::into, Into::into)
    }

    /// Recognizes interruption without discarding its source information.
    ///
    /// Includes Linux EINTR but not internal syscall restart codes.
    pub fn is_interrupted(self) -> bool {
        matches!(
            self.canonicalize(),
            Self::Interrupted | Self::IoWaitInterrupted
        )
    }

    /// Encodes this error as the raw return value a syscall dispatch layer
    /// stores in the user trap context: the negated Linux errno.
    ///
    /// The error is first converted through [`LinuxError`], so kernel
    /// [`KErrorKind`]s are canonicalized to a Linux-compatible errno before the
    /// sign is applied. This is the syscall ABI boundary: ordinary kernel
    /// callers should propagate [`KError`] and let the dispatch layer encode it
    /// exactly once.
    pub fn into_syscall_return(self) -> isize {
        -LinuxError::from(self).into_raw() as isize
    }
}

impl<E: Into<KErrorKind>> From<E> for KError {
    fn from(e: E) -> Self {
        KError::new_ax(e.into())
    }
}

impl From<LinuxError> for KError {
    fn from(e: LinuxError) -> Self {
        KError::new_linux(e)
    }
}

impl From<KError> for LinuxError {
    fn from(e: KError) -> Self {
        match e.data() {
            KErrorData::Ky(kind) => LinuxError::from(kind),
            KErrorData::Linux(kind) => kind,
        }
    }
}

impl TryFrom<KError> for KErrorKind {
    type Error = LinuxError;

    fn try_from(e: KError) -> Result<Self, Self::Error> {
        match e.data() {
            KErrorData::Ky(kind) => Ok(kind),
            KErrorData::Linux(e) => e.try_into(),
        }
    }
}

impl KError {
    /// Decodes an internal signed error representation without panicking.
    ///
    /// Positive values must identify a `KErrorKind`; negative values must be the
    /// negation of a named `LinuxError`. Zero is not an error representation.
    ///
    /// # Errors
    ///
    /// Returns the original `value` for unknown codes, zero, or `i32::MIN`.
    pub fn try_from_i32(value: i32) -> Result<Self, i32> {
        if KErrorKind::try_from(value).is_ok() {
            return Ok(KError(value));
        }
        if value < 0 {
            let linux = LinuxError::new(value.checked_neg().ok_or(value)?);
            if linux.name().is_some() {
                return Ok(KError(value));
            }
        }
        Err(value)
    }
}

impl fmt::Debug for KError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.data() {
            KErrorData::Ky(kind) => write!(f, "KErrorKind::{:?}", kind),
            KErrorData::Linux(kind) => write!(f, "LinuxError::{:?}", kind),
        }
    }
}

impl fmt::Display for KError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.data() {
            KErrorData::Ky(kind) => write!(f, "{}", kind),
            KErrorData::Linux(kind) => write!(f, "{}", kind),
        }
    }
}

macro_rules! kerror_consts {
    ($($name:ident),*) => {
        #[expect(non_upper_case_globals, reason = "CG-024-lint-exceptions: non_upper_case_globals: Macro-generated error constants retain the KErrorKind variant spelling")]
        impl KError {
            $(
                #[doc = concat!("An [`KError`] with kind [`KErrorKind::", stringify!($name), "`].")]
                pub const $name: Self = Self::new_ax(KErrorKind::$name);
            )*
        }
    };
}

kerror_consts!(
    AddrInUse,
    AlreadyConnected,
    AlreadyExists,
    ArgumentListTooLong,
    BadAddress,
    BadFileDescriptor,
    BadState,
    BrokenPipe,
    ConnectionRefused,
    ConnectionReset,
    CrossesDevices,
    DirectoryNotEmpty,
    FilesystemLoop,
    IllegalBytes,
    InProgress,
    Interrupted,
    InvalidData,
    InvalidExecutable,
    InvalidInput,
    Io,
    IsADirectory,
    NameTooLong,
    NoMemory,
    NoSuchDevice,
    NoSuchProcess,
    NotADirectory,
    NotASocket,
    NotATty,
    NotConnected,
    NotFound,
    OperationNotPermitted,
    OperationNotSupported,
    OutOfRange,
    PermissionDenied,
    ReadOnlyFilesystem,
    ResourceBusy,
    StorageFull,
    TimedOut,
    TooManyOpenFiles,
    UnexpectedEof,
    Unsupported,
    WouldBlock,
    WriteZero,
    ConnectionAborted,
    FileTooLarge,
    IoWaitInterrupted
);

/// A specialized [`Result`] type with [`KError`] as the error type.
pub type KResult<T = ()> = Result<T, KError>;

/// Convenience method to construct an [`KError`] type while printing a warning
/// message.
///
/// # Examples
///
/// ```
/// # use kerrno::{k_err_type, KError};
/// #
/// // Also print "[KError::AlreadyExists]" if the `log` crate is enabled.
/// assert_eq!(k_err_type!(AlreadyExists), KError::AlreadyExists,);
///
/// // Also print "[KError::BadAddress] the address is 0!" if the `log` crate
/// // is enabled.
/// assert_eq!(
///     k_err_type!(BadAddress, "the address is 0!"),
///     KError::BadAddress,
/// );
/// ```
#[macro_export]
macro_rules! k_err_type {
    ($err:ident) => {{
        use $crate::KErrorKind::*;
        let err = $crate::KError::from($err);
        $crate::__priv::warn!("[{:?}]", err);
        err
    }};
    ($err:ident, $msg:expr) => {{
        use $crate::KErrorKind::*;
        let err = $crate::KError::from($err);
        $crate::__priv::warn!("[{:?}] {}", err, $msg);
        err
    }};
}

/// Ensure a condition is true. If it is not, return from the function
/// with an error.
///
/// ## Examples
///
/// ```rust
/// # use kerrno::{ensure, k_err, KError, KResult};
///
/// fn example(user_id: i32) -> KResult {
///     ensure!(user_id > 0, k_err!(InvalidInput));
///     // After this point, we know that `user_id` is positive.
///     let user_id = user_id as u32;
///     Ok(())
/// }
/// ```
#[macro_export]
macro_rules! ensure {
    ($predicate:expr, $context_selector:expr $(,)?) => {
        if !$predicate {
            return $context_selector;
        }
    };
}

/// Convenience method to construct an [`Err(KError)`] type while printing a
/// warning message.
///
/// # Examples
///
/// ```
/// # use kerrno::{k_err, KResult, KError};
/// #
/// // Also print "[KError::AlreadyExists]" if the `log` crate is enabled.
/// assert_eq!(
///     k_err!(AlreadyExists),
///     KResult::<()>::Err(KError::AlreadyExists),
/// );
///
/// // Also print "[KError::BadAddress] the address is 0!" if the `log` crate is enabled.
/// assert_eq!(
///     k_err!(BadAddress, "the address is 0!"),
///     KResult::<()>::Err(KError::BadAddress),
/// );
/// ```
/// [`Err(KError)`]: Err
#[macro_export]
macro_rules! k_err {
    ($err:ident) => {
        Err($crate::k_err_type!($err))
    };
    ($err:ident, $msg:expr) => {
        Err($crate::k_err_type!($err, $msg))
    };
}

/// Throws an error of type [`KError`] with the given error code, optionally
/// with a message.
#[macro_export]
macro_rules! k_bail {
    ($($t:tt)*) => {
        return $crate::k_err!($($t)*);
    };
}

/// A specialized [`Result`] type with [`LinuxError`] as the error type.
pub type LinuxResult<T = ()> = Result<T, LinuxError>;

#[doc(hidden)]
pub mod __priv {
    pub use log::warn;
}

unittest::path_mod_test!(tests_unittest, "../ut-tests/kerrno_unittest.rs");
