// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel error types and errno conversions.
#![cfg_attr(not(test), no_std)]

use core::fmt;

pub use linux_sysno::Errno as LinuxError;
use strum::EnumCount;

/// The error kind type used by ArceOS.
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
        }
    }

    /// Returns the error code value in `i32`.
    pub const fn code(self) -> i32 {
        self as i32
    }
}

impl TryFrom<i32> for KErrorKind {
    type Error = i32;

    #[inline]
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        if value > 0 && value <= KErrorKind::COUNT as i32 {
            Ok(unsafe { core::mem::transmute::<i32, KErrorKind>(value) })
        } else {
            Err(value)
        }
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
            ConnectionRefused => LinuxError::ECONNREFUSED,
            ConnectionReset => LinuxError::ECONNRESET,
            CrossesDevices => LinuxError::EXDEV,
            DirectoryNotEmpty => LinuxError::ENOTEMPTY,
            FilesystemLoop => LinuxError::ELOOP,
            IllegalBytes => LinuxError::EILSEQ,
            InProgress => LinuxError::EINPROGRESS,
            Interrupted => LinuxError::EINTR,
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
            LinuxError::ECONNREFUSED => ConnectionRefused,
            LinuxError::ECONNRESET => ConnectionReset,
            LinuxError::EXDEV => CrossesDevices,
            LinuxError::ENOTEMPTY => DirectoryNotEmpty,
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

/// The error type used by ArceOS.
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
            KErrorData::Ky(unsafe { core::mem::transmute::<i32, KErrorKind>(self.0) })
        }
    }

    /// Returns the error code value in `i32`.
    pub const fn code(self) -> i32 {
        self.0
    }

    /// Returns a canonicalized version of this error.
    ///
    /// This method tries to convert [`LinuxError`] variants into their
    /// corresponding [`KErrorKind`] variants if possible.
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
    pub fn try_from_i32(value: i32) -> Result<Self, i32> {
        if KErrorKind::try_from(value).is_ok() {
            return Ok(KError(value));
        }
        if value < 0 {
            let linux = LinuxError::new(-value);
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
        #[allow(non_upper_case_globals)]
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
    WriteZero
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

#[cfg(test)]
mod tests {
    use strum::EnumCount;

    use crate::{KError, KErrorKind, LinuxError};

    #[test]
    fn test_try_from() {
        let max_code = KErrorKind::COUNT as i32;
        assert_eq!(max_code, 43);
        assert_eq!(max_code, KError::WriteZero.code());

        assert_eq!(KError::AddrInUse.code(), 1);
        assert_eq!(Ok(KError::AddrInUse), KError::try_from_i32(1));
        assert_eq!(Ok(KError::AlreadyConnected), KError::try_from_i32(2));
        assert_eq!(Ok(KError::WriteZero), KError::try_from_i32(max_code));
        assert_eq!(Err(max_code + 1), KError::try_from_i32(max_code + 1));
        assert_eq!(Err(0), KError::try_from_i32(0));
        assert_eq!(Err(i32::MAX), KError::try_from_i32(i32::MAX));
    }

    #[test]
    fn test_conversion() {
        for i in 1.. {
            let err = LinuxError::new(i);
            if err.name().is_none() {
                break;
            }
            assert_eq!(err.into_raw(), i);
            let e = KError::from(err);
            assert_eq!(e.code(), -i);
            assert_eq!(LinuxError::from(e), err);
        }
    }
}
