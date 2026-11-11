// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::{FeatureSet, IncompatFeatures};
use crate::{Ext4Error, UnsupportedKind};

#[def_test]
fn mount_negotiation_rejects_filesystem_without_extents() {
    let features = FeatureSet::new(0, IncompatFeatures::FILETYPE.bits(), 0);

    assert_eq!(
        features.validate_mount(),
        Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentFilesystem))
    );
}

#[def_test]
fn quota_mount_support_distinguishes_user_group_and_project() {
    use super::ReadOnlyCompatFeatures;
    for flags in [
        ReadOnlyCompatFeatures::QUOTA,
        ReadOnlyCompatFeatures::PROJECT,
        ReadOnlyCompatFeatures::QUOTA | ReadOnlyCompatFeatures::PROJECT,
    ] {
        let features = FeatureSet::new(0, IncompatFeatures::EXTENTS.bits(), flags.bits());
        assert_eq!(features.validate_read_only(), Ok(()));
        assert_eq!(features.validate_mount(), Ok(()));
        assert_eq!(features.validate_writable(), Ok(()));
    }
}
