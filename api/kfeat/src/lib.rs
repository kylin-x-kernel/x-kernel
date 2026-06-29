// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Top-level feature selection for [X-Kernel].
//!
//! # Cargo Features
//!
//! - CPU
//!     - `smp`: Enable SMP (symmetric multiprocessing) support.
//!     - `fp-simd`: Enable floating point and SIMD support.
//! - Interrupts:
//!     - `irq`: Enable interrupt handling support.
//!     - `ipi`: Enable Inter-Processor Interrupts (IPIs).
//! - Memory
//!     - `alloc`: Enable dynamic memory allocation.
//!     - `alloc_tlsf`: Use the TLSF allocator.
//!     - `alloc_slab`: Use the slab allocator.
//!     - `alloc_buddy`: Use the buddy system allocator.
//!     - `paging`: Enable page table manipulation.
//!     - `tls`: Enable thread-local storage.
//! - Upperlayer stacks (fs, net, display)
//!     - `fs`: Enable file system support.
//!     - `myfs`: Allow users to define their custom filesystems to override the default.
//!     - `net`: Enable networking support.
//!     - `display`: Enable graphics support.
//! - Device drivers (selected independently from the upperlayer stacks; a
//!   filesystem feature no longer implies a specific block driver, so
//!   non-virtio platforms can plug in their own drivers).
//!     - `driver_virtio_blk`: virtio-blk block device driver.
//!     - `driver_virtio_net`: virtio-net NIC driver (implies `net`).
//!     - `driver_virtio_gpu`: virtio-gpu display driver (implies `display`).
//!     - `driver_virtio_input`: virtio-input device driver (implies `input`).
//!     - `driver_virtio_socket`: virtio-vsock transport driver (implies `vsock`).
//!     - `driver_virtio_9p`: virtio-9p transport driver.
//!     - `driver_ramdisk`: Use the RAM disk to emulate the block device.
//!     - `driver-ixgbe`: Enable the Intel 82599 10Gbit NIC driver.
//!     - `driver-bcm2835-sdhci`: Enable the BCM2835 SDHCI driver (Raspberry Pi SD card).
#![no_std]

#[cfg(feature = "platform_aarch64_qemu_virt")]
extern crate aarch64_qemu_virt;
#[cfg(feature = "platform_loongarch64_qemu_virt")]
extern crate loongarch64_qemu_virt;
#[cfg(feature = "platform_riscv64_qemu_virt")]
extern crate riscv64_qemu_virt;
#[cfg(feature = "platform_x86_64_qemu_virt")]
extern crate x86_64_qemu_virt;
