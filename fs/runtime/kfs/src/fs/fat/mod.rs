// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FAT filesystem adapter and utilities.
mod dir;
mod ff;
mod file;
mod fs;
mod util;

use core::{cell::UnsafeCell, ptr::NonNull};

use fatfs::SeekFrom;
pub use fs::FatFilesystem;
use fs::{FatFilesystem, FatFilesystemGuard};

#[cfg(unittest)]
mod fat_test;

use crate::disk::SeekableDisk;

impl fatfs::IoBase for SeekableDisk {
    type Error = ();
}

impl fatfs::Read for SeekableDisk {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        SeekableDisk::read(self, buf).map_err(|_| ())
    }
}

impl fatfs::Write for SeekableDisk {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        SeekableDisk::write(self, buf).map_err(|_| ())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        SeekableDisk::flush(self).map_err(|_| ())
    }
}

impl fatfs::Seek for SeekableDisk {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        let size = self.size();
        let new_pos = match pos {
            SeekFrom::Start(pos) => Some(pos),
            SeekFrom::Current(off) => self.position().checked_add_signed(off),
            SeekFrom::End(off) => size.checked_add_signed(off),
        }
        .ok_or(())?;
        self.set_position(new_pos).map_err(|_| ())?;
        Ok(new_pos)
    }
}

/// A reference to an object within a filesystem.
pub(crate) struct FsRef<T> {
    owner: NonNull<FatFilesystem>,
    inner: UnsafeCell<T>,
}

impl<T> FsRef<T> {
    /// Create a new filesystem reference wrapper.
    pub fn new(owner: &FatFilesystem, inner: T) -> Self {
        Self {
            owner: NonNull::from(owner),
            inner: UnsafeCell::new(inner),
        }
    }

    /// Extend a FAT handle's filesystem-tied lifetime to the surrounding node.
    ///
    /// # Safety
    ///
    /// The caller must ensure the handle is only accessed while holding the
    /// owning [`FatFilesystemInner`] lock for the same filesystem instance.
    pub unsafe fn from_filesystem_handle(owner: &FatFilesystem, inner: T) -> Self {
        Self::new(owner, inner)
    }

    /// Borrow an immutable reference tied to the filesystem lifetime.
    pub fn borrow<'a>(&self, fs: &'a FatFilesystemGuard<'a>) -> &'a T {
        self.assert_owner(fs);
        // SAFETY: `assert_owner` checked that `fs` is the filesystem instance
        // that created this handle, and callers hold that filesystem mutex while
        // borrowing. The underlying FAT object therefore stays alive and access
        // remains serialized by the owner lock.
        unsafe { &*self.inner.get() }
    }

    /// Borrow a mutable reference tied to the filesystem lifetime.
    #[allow(clippy::mut_from_ref)]
    pub fn borrow_mut<'a>(&self, fs: &'a mut FatFilesystemGuard<'a>) -> &'a mut T {
        self.assert_owner(fs);
        // SAFETY: `assert_owner` checked that `fs` is the filesystem instance
        // that created this handle, and the owner mutex guard provides the
        // exclusive access required to hand out `&mut T`.
        unsafe { &mut *self.inner.get() }
    }

    fn assert_owner(&self, fs: &FatFilesystemGuard<'_>) {
        assert_eq!(
            self.owner,
            fs.owner_ptr(),
            "FAT handle borrowed under a different filesystem instance"
        );
    }
}

impl FsRef<ff::File<'static>> {
    /// Capture a FAT file handle whose borrow is externally guarded by the
    /// owning filesystem lock.
    ///
    /// # Safety
    ///
    /// The caller must ensure the returned handle is only accessed while
    /// holding the matching [`FatFilesystemInner`] lock for the filesystem
    /// that created `file`.
    pub unsafe fn from_file_handle(owner: &FatFilesystem, file: ff::File<'_>) -> Self {
        // SAFETY: the caller guarantees `file` stays tied to `owner` and is only
        // accessed while holding that filesystem's mutex, so extending the
        // handle lifetime to the node wrapper remains within the owner lock's
        // audit surface.
        Self::new(owner, unsafe {
            core::mem::transmute::<ff::File<'_>, ff::File<'static>>(file)
        })
    }
}

impl FsRef<ff::Dir<'static>> {
    /// Capture a FAT directory handle whose borrow is externally guarded by
    /// the owning filesystem lock.
    ///
    /// # Safety
    ///
    /// The caller must ensure the returned handle is only accessed while
    /// holding the matching [`FatFilesystemInner`] lock for the filesystem
    /// that created `dir`.
    pub unsafe fn from_dir_handle(owner: &FatFilesystem, dir: ff::Dir<'_>) -> Self {
        // SAFETY: the caller guarantees `dir` stays tied to `owner` and is only
        // accessed while holding that filesystem's mutex, so extending the
        // handle lifetime to the node wrapper remains within the owner lock's
        // audit surface.
        Self::new(owner, unsafe {
            core::mem::transmute::<ff::Dir<'_>, ff::Dir<'static>>(dir)
        })
    }
}

// SAFETY: `FsRef<T>` stores the exact owning `FatFilesystemInner` pointer for the
// captured FAT handle. Every access path checks that the caller holds that same
// filesystem instance and then relies on its mutex to serialize handle access.
unsafe impl<T> Send for FsRef<T> {}

// SAFETY: shared references do not expose `T` directly. Borrowing requires the
// matching owner lock, and `assert_owner` rejects reborrowing under a different
// filesystem instance.
unsafe impl<T> Sync for FsRef<T> {}
