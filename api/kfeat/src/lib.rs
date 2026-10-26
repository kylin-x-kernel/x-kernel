// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Top-level feature selection for [X-Kernel].
//!
//! System capabilities are assembled here: `ipi` selects the runtime, task,
//! RCU and IPI-provider features together; `smp` includes that assembly and
//! selects the participating subsystems' SMP support. Runtime's local feature
//! switches are not substitutes for these system-level selections.
//! PMU, NMI and ARM timer recovery likewise select their runtime and provider
//! capabilities here. Each kernel crate's `smp` feature controls only its own
//! code; it must not forward SMP to dependencies or force it in dependency
//! declarations. This crate selects platform, scheduler, lock, page-table and
//! IPI-provider SMP support together. Enabling a leaf crate's `smp` feature
//! alone is not a complete multicore kernel configuration.
//! Host multithreaded tests may select real locks through dev-dependencies;
//! those test-only selections do not configure production kernels.
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
//!     - File system support is gated by Kconfig `KFEAT_FS`; Cargo features
//!       expose concrete filesystem capabilities such as `fs_ext4`.
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
//!     - `driver_virtio_rng`: virtio-rng entropy source driver (enables the Hwrng class).
//!     - `entropy_virtio_rng`: trust VirtIO Hwrng entropy for pool seeding (requires the driver).
//!     - `entropy_arch_cpu`: CPU instruction entropy (AArch64 RNDR / x86 RDSEED·RDRAND).
//!     - `entropy_smccc_trng`: ARM SMCCC TRNG 1.0 firmware entropy for the kernel pool.
//!     - `entropy_jitter`: software timing jitter entropy for the kernel pool.
//!     - `driver_ramdisk`: Use the RAM disk to emulate the block device.
//!     - `driver-ixgbe`: Enable the Intel 82599 10Gbit NIC driver.
//!     - `driver-bcm2835-sdhci`: Enable the BCM2835 SDHCI driver (Raspberry Pi SD card).
//! - Diagnostics:
//!     - `kwork_stall_check`: Link prepared workqueue stall inspection and enable
//!       watchdog boot initialization; periodic inspection requires system integration.
#![no_std]

// Link container-owned tests without a runtime dependency on the container.
#[cfg(all(unittest, feature = "kernel-tests"))]
extern crate krcu_list as _;
#[cfg(all(unittest, feature = "kernel-tests"))]
extern crate krcu_list_kernel_tests as _;
#[cfg(all(unittest, feature = "kernel-tests"))]
extern crate kxarray as _;
#[cfg(all(unittest, feature = "kernel-tests"))]
extern crate kxarray_kernel_tests as _;

#[cfg(feature = "kwork_stall_check")]
extern crate kworkng as _;

#[cfg(feature = "fs_fat")]
extern crate fat as _;
#[cfg(feature = "fs_ext4")]
extern crate kext4 as _;
#[cfg(feature = "platform_kplat_aarch64")]
extern crate kplat_aarch64 as _;
#[cfg(feature = "platform_kplat_loongarch64")]
extern crate kplat_loongarch64 as _;
#[cfg(feature = "platform_kplat_riscv64")]
extern crate kplat_riscv64 as _;
#[cfg(feature = "platform_kplat_x86_64")]
extern crate kplat_x86_64 as _;
