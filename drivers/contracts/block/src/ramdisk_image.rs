// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Embedded filesystem image backing a static RAM disk.
//!
//! Active only under the `ramdisk-static` feature. The image path is supplied
//! at build time through the `XKERNEL_RAMDISK_IMG` environment variable, which
//! the crate's `build.rs` turns into a `cargo:rustc-env`. The Makefile sets it
//! from the `RAMDISK_IMG` variable (defaulting to an empty EXT4 image built
//! by `make ramdisk_img`, but any FAT/ext4 image may be substituted).
//!
//! The image bytes live in a 512-byte-aligned, writable static so that
//! [`crate::ramdisk_static::RamDisk`] can use them directly as a zero-copy
//! block backend. Because the root filesystem is mounted read-write, the
//! backing storage must be mutable, so the static is placed in `.data`; the
//! embedded image therefore grows the kernel binary by roughly the image size.

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};

/// Length of the embedded image, supplied by the build script to avoid a
/// second `include_bytes!` call (see [`IMAGE`]).
const IMG_LEN: usize = {
    let bytes = env!("XKERNEL_RAMDISK_IMG_LEN").as_bytes();
    // const-context decimal parser (no `str::parse` in const on all targets).
    let mut val: usize = 0;
    let mut i = 0;
    while i < bytes.len() {
        val = val * 10 + (bytes[i] - b'0') as usize;
        i += 1;
    }
    val
};

/// 512-byte-aligned, writable container for the embedded image bytes.
///
/// `UnsafeCell` provides the interior mutability the block device needs to
/// write through the storage. `unsafe impl Sync` is sound because every access
/// is mediated by the `SpinNoPreempt` lock held inside
/// [`crate::ramdisk_static::RamDisk`].
#[repr(C, align(512))]
struct Image(UnsafeCell<[u8; IMG_LEN]>);

// SAFETY: The only access to the backing bytes goes through
// `ramdisk_static::RamDisk`, which serializes all reads and writes with its
// own `SpinNoPreempt` lock. There is no unsynchronized shared access.
unsafe impl Sync for Image {}

static IMAGE: Image = Image(UnsafeCell::new(*include_bytes!(env!(
    "XKERNEL_RAMDISK_IMG"
))));

/// Guard to ensure [`ramdisk`] is called at most once.
static RAMDISK_CALLED: AtomicBool = AtomicBool::new(false);

/// Construct a static RAM disk backed by the embedded filesystem image.
///
/// Returns a [`crate::ramdisk_static::RamDisk`] whose storage is the embedded
/// image, used directly (zero-copy) rather than copied into the heap. This
/// hands out a one-time `&'static mut [u8]` view over the image; it must be
/// called at most once, and the ramdisk driver's `probe_device` path is the
/// only intended caller.
pub fn ramdisk() -> crate::ramdisk_static::RamDisk {
    assert!(
        !RAMDISK_CALLED.swap(true, Ordering::AcqRel),
        "ramdisk_image::ramdisk() must be called at most once"
    );
    // SAFETY: `IMAGE` is a unique static. The `RAMDISK_CALLED` guard above
    // ensures this function is entered at most once. The returned mutable slice
    // is consumed immediately by `RamDisk::new`, which stores only a raw
    // `base_addr`/`len` and never retains the borrow. Subsequent reads and
    // writes reconstruct temporary slices under the device's `SpinNoPreempt`
    // lock, so no two mutable borrows of the backing storage can coexist.
    let buf: &'static mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(IMAGE.0.get() as *mut u8, IMG_LEN) };
    crate::ramdisk_static::RamDisk::new(buf)
}
