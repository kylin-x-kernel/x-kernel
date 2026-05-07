// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Credential transition errors.

/// Error returned when a credential transition is not permitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    /// The caller lacks the privilege required for the requested ID change.
    PermissionDenied,
}
