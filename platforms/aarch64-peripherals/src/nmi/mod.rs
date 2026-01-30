// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(feature = "nmi-pmu")]
pub mod nmi_pmu;
#[cfg(feature = "nmi-sdei")]
pub mod nmi_sdei;
