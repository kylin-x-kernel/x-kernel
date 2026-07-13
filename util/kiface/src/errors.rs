// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared diagnostics.

use syn::{Error, Generics};

pub fn generic_not_allowed_error(generics: &Generics) -> Error {
    Error::new_spanned(
        generics,
        "generic parameters are not allowed in kiface interfaces",
    )
}
