// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::format;

use unittest::def_test;

use crate::{KError, KErrorKind, LinuxError};

const KIND_LINUX_PAIRS: &[(KErrorKind, LinuxError)] = &[
    (KErrorKind::IoWaitInterrupted, LinuxError::EINTR),
    (KErrorKind::AddrInUse, LinuxError::EADDRINUSE),
    (KErrorKind::AlreadyConnected, LinuxError::EISCONN),
    (KErrorKind::AlreadyExists, LinuxError::EEXIST),
    (KErrorKind::ArgumentListTooLong, LinuxError::E2BIG),
    (KErrorKind::BadAddress, LinuxError::EFAULT),
    (KErrorKind::BadFileDescriptor, LinuxError::EBADF),
    (KErrorKind::BadState, LinuxError::EFAULT),
    (KErrorKind::BrokenPipe, LinuxError::EPIPE),
    (KErrorKind::ConnectionAborted, LinuxError::ECONNABORTED),
    (KErrorKind::ConnectionRefused, LinuxError::ECONNREFUSED),
    (KErrorKind::ConnectionReset, LinuxError::ECONNRESET),
    (KErrorKind::CrossesDevices, LinuxError::EXDEV),
    (KErrorKind::DirectoryNotEmpty, LinuxError::ENOTEMPTY),
    (KErrorKind::FileTooLarge, LinuxError::EFBIG),
    (KErrorKind::FilesystemLoop, LinuxError::ELOOP),
    (KErrorKind::IllegalBytes, LinuxError::EILSEQ),
    (KErrorKind::InProgress, LinuxError::EINPROGRESS),
    (KErrorKind::Interrupted, LinuxError::EINTR),
    (KErrorKind::InvalidData, LinuxError::EINVAL),
    (KErrorKind::InvalidExecutable, LinuxError::ENOEXEC),
    (KErrorKind::InvalidInput, LinuxError::EINVAL),
    (KErrorKind::Io, LinuxError::EIO),
    (KErrorKind::IsADirectory, LinuxError::EISDIR),
    (KErrorKind::NameTooLong, LinuxError::ENAMETOOLONG),
    (KErrorKind::NoMemory, LinuxError::ENOMEM),
    (KErrorKind::NoSuchDevice, LinuxError::ENODEV),
    (KErrorKind::NoSuchProcess, LinuxError::ESRCH),
    (KErrorKind::NotADirectory, LinuxError::ENOTDIR),
    (KErrorKind::NotASocket, LinuxError::ENOTSOCK),
    (KErrorKind::NotATty, LinuxError::ENOTTY),
    (KErrorKind::NotConnected, LinuxError::ENOTCONN),
    (KErrorKind::NotFound, LinuxError::ENOENT),
    (KErrorKind::OperationNotPermitted, LinuxError::EPERM),
    (KErrorKind::OperationNotSupported, LinuxError::EOPNOTSUPP),
    (KErrorKind::OutOfRange, LinuxError::ERANGE),
    (KErrorKind::PermissionDenied, LinuxError::EACCES),
    (KErrorKind::ReadOnlyFilesystem, LinuxError::EROFS),
    (KErrorKind::ResourceBusy, LinuxError::EBUSY),
    (KErrorKind::StorageFull, LinuxError::ENOSPC),
    (KErrorKind::TimedOut, LinuxError::ETIMEDOUT),
    (KErrorKind::TooManyOpenFiles, LinuxError::EMFILE),
    (KErrorKind::UnexpectedEof, LinuxError::EIO),
    (KErrorKind::Unsupported, LinuxError::ENOSYS),
    (KErrorKind::WouldBlock, LinuxError::EAGAIN),
    (KErrorKind::WriteZero, LinuxError::EIO),
];

#[def_test]
fn test_kerrorkind_as_str_and_code() {
    assert_eq!(KErrorKind::AddrInUse.as_str(), "Address in use");
    assert_eq!(
        KErrorKind::InvalidExecutable.as_str(),
        "Invalid executable format"
    );
    assert_eq!(KErrorKind::WouldBlock.as_str(), "Operation would block");
    assert_eq!(KErrorKind::AddrInUse.code(), 1);
}

#[def_test]
fn test_kerrorkind_try_from_rejects_invalid_values() {
    assert_eq!(KErrorKind::try_from(0), Err(0));
    assert_eq!(KErrorKind::try_from(-1), Err(-1));
    assert!(KErrorKind::try_from(1).is_ok());
    assert!(KErrorKind::try_from(i32::MAX).is_err());
}

#[def_test]
fn test_kerror_linux_and_kernel_conversions() {
    let linux = KError::from(LinuxError::EACCES);
    assert_eq!(linux.code(), -LinuxError::EACCES.into_raw());
    assert_eq!(
        linux.canonicalize(),
        KError::from(KErrorKind::PermissionDenied)
    );

    let kernel = KError::from(KErrorKind::BadFileDescriptor);
    assert_eq!(LinuxError::from(kernel), LinuxError::EBADF);
    assert_eq!(
        KErrorKind::try_from(kernel),
        Ok(KErrorKind::BadFileDescriptor)
    );
}

#[def_test]
fn test_kerror_try_from_i32_and_formatting() {
    let linux = KError::try_from_i32(-LinuxError::ENOENT.into_raw()).unwrap();
    assert_eq!(linux.code(), -LinuxError::ENOENT.into_raw());
    assert_eq!(LinuxError::from(linux), LinuxError::ENOENT);

    let kernel = KError::try_from_i32(KErrorKind::TimedOut.code()).unwrap();
    assert_eq!(kernel.code(), KErrorKind::TimedOut.code());
    assert_eq!(KErrorKind::try_from(kernel), Ok(KErrorKind::TimedOut));

    assert_eq!(KError::try_from_i32(0), Err(0));
}

#[def_test]
fn test_kerrorkind_to_linuxerror_mapping_table() {
    for &(kind, linux) in KIND_LINUX_PAIRS {
        assert_eq!(LinuxError::from(kind), linux);
        assert!(!kind.as_str().is_empty());
        assert_eq!(format!("{kind}"), kind.as_str());
    }
}

#[def_test]
fn test_linuxerror_to_kerrorkind_mapping_and_error_cases() {
    for &(kind, linux) in KIND_LINUX_PAIRS {
        let mapped = KErrorKind::try_from(linux);
        if matches!(
            kind,
            KErrorKind::IoWaitInterrupted
                | KErrorKind::BadState
                | KErrorKind::InvalidData
                | KErrorKind::UnexpectedEof
                | KErrorKind::WriteZero
        ) {
            continue;
        }
        assert_eq!(mapped, Ok(kind));
    }

    assert_eq!(
        KErrorKind::try_from(LinuxError::EDEADLK),
        Err(LinuxError::EDEADLK)
    );
}

#[def_test]
fn test_kerror_debug_display_and_canonicalize() {
    let kernel = KError::from(KErrorKind::ReadOnlyFilesystem);
    assert_eq!(format!("{kernel}"), "Read-only filesystem");
    assert!(format!("{kernel:?}").contains("ReadOnlyFilesystem"));

    let linux = KError::from(LinuxError::EPIPE);
    assert!(format!("{linux}").contains("Broken pipe"));
    assert!(format!("{linux:?}").contains("LinuxError"));
    assert_eq!(linux.canonicalize(), KError::from(KErrorKind::BrokenPipe));

    let unmapped = KError::from(LinuxError::EDEADLK);
    assert_eq!(unmapped.canonicalize(), unmapped);
    assert_eq!(KErrorKind::try_from(unmapped), Err(LinuxError::EDEADLK));
}

#[def_test]
fn test_kerror_constants_and_try_from_i32_invalid_linux() {
    assert_eq!(
        KError::PermissionDenied.code(),
        KErrorKind::PermissionDenied.code()
    );
    assert_eq!(KError::WouldBlock.code(), KErrorKind::WouldBlock.code());
    assert_eq!(KError::try_from_i32(-123456), Err(-123456));
}

#[def_test]
fn file_wait_interruption_preserves_source_and_defaults_to_eintr() {
    let marker = KError::IoWaitInterrupted;
    assert_eq!(KError::FileTooLarge.code(), 45);
    assert_eq!(marker.code(), 46);
    assert_eq!(KError::try_from_i32(marker.code()), Ok(marker));
    assert_eq!(marker.canonicalize(), marker);
    assert_ne!(marker, KError::Interrupted);
    assert_eq!(LinuxError::from(marker), LinuxError::EINTR);
    assert!(marker.is_interrupted());
    assert!(KError::Interrupted.is_interrupted());
    assert!(KError::from(LinuxError::EINTR).is_interrupted());
    assert!(!KError::from(LinuxError::ERESTARTSYS).is_interrupted());
    assert!(!KError::WouldBlock.is_interrupted());
}
