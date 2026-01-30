// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel synchronization primitives.
//!
//! This crate provides blocking synchronization primitives for kernel tasks:
//!
//! - [`Mutex`]: Mutual exclusion lock with configurable spinning
//! - [`RwLock`]: Reader-writer lock (allows multiple readers or one writer)
//! - [`Semaphore`]: Counting semaphore for resource management
//! - [`spin`]: Re-export of `kspin` for spinlocks
//!
//! # Examples
//!
//! ## Mutex
//! ```no_run
//! use ksync::Mutex;
//!
//! static DATA: Mutex<Vec<u8>> = Mutex::new(Vec::new());
//!
//! fn task() {
//!     let mut data = DATA.lock();
//!     data.push(42);
//! }
//! ```
//!
//! ## Mutex with Custom Spin Configuration
//! ```no_run
//! use ksync::{Mutex, SpinConfig};
//!
//! static DATA: Mutex<u32> = Mutex::const_new(
//!     ksync::RawMutex::with_config(SpinConfig {
//!         max_spins: 20,
//!         spin_before_yield: 5,
//!     }),
//!     0,
//! );
//! ```
//!
//! ## RwLock
//! ```no_run
//! use ksync::RwLock;
//!
//! static CONFIG: RwLock<u32> = RwLock::new(0);
//!
//! fn reader() {
//!     let config = CONFIG.read();
//!     // multiple readers allowed
//! }
//!
//! fn writer() {
//!     let mut config = CONFIG.write();
//!     // exclusive writer
//! }
//! ```
//!
//! ## Semaphore
//! ```no_run
//! use ksync::Semaphore;
//!
//! static SEM: Semaphore = Semaphore::new(3);
//!
//! fn task() {
//!     let _guard = SEM.acquire_guard();
//!     // do work with permit
//!     // permit automatically released when guard is dropped
//! }
//! ```
//!
//! # Features
//!
//! - `stats`: Enable mutex statistics tracking (total locks, spins, blocks)
//! - `watchdog`: Enable watchdog support for deadlock detection

#![cfg_attr(not(test), no_std)]
#![feature(doc_cfg)]

pub use kspin as spin;

mod mutex;
mod rwlock;
mod semaphore;
mod util;

#[cfg(feature = "stats")]
pub use self::mutex::MutexStats;
pub use self::{
    mutex::{Mutex, MutexGuard, RawMutex},
    rwlock::{RawRwLock, RwLock, RwLockReadGuard, RwLockWriteGuard},
    semaphore::{Semaphore, SemaphoreGuard},
    util::SpinConfig,
};
