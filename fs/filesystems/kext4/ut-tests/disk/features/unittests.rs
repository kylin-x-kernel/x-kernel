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
