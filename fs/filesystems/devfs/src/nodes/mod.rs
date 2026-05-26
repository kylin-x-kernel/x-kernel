// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! devfs device node definitions.

pub(crate) mod cpu_dma_latency;
pub(crate) mod dtb;
pub(crate) mod fb;
pub(crate) mod r#loop;
pub(crate) mod null_zero_full;
pub(crate) mod pts;
pub(crate) mod random;
pub(crate) mod rtc;
pub(crate) mod shm;
pub(crate) mod tty_nodes;

#[cfg(all(feature = "dice", target_os = "none"))]
pub(crate) mod dice;
#[cfg(feature = "input")]
pub(crate) mod event;
#[cfg(feature = "dev-log")]
pub(crate) mod log;
#[cfg(feature = "memtrack")]
pub(crate) mod memtrack;
