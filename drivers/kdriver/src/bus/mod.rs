// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus probing helpers.
#[cfg(bus = "mmio")]
mod mmio;
#[cfg(bus = "pci")]
mod pci;
