// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual device crates.

#![no_std]

pub use vdev_core as core;
pub use vdev_test_mmio as test_mmio;
pub use vdev_uart16550 as uart16550;
pub use vdev_virtio_net as virtio_net;
pub use vdev_vpl011 as vpl011;
