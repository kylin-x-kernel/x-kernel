// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(all(feature = "dice_huk_key", feature = "virtcca_huk_key"))]
compile_error!(
    "features `dice_huk_key` and `virtcca_huk_key` are mutually exclusive; enable only one HUK \
     root."
);

#[cfg(feature = "dice_huk_key")]
pub mod dice;
#[cfg(feature = "virtcca_huk_key")]
pub mod virtcca;
