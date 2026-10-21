// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{CompatFeatures, FeatureSet, IncompatFeatures};
use crate::{Ext4Error, FeatureClass};

#[def_test]
fn overlapping_feature_bits_remain_in_their_own_classes() {
    let features = FeatureSet::new(
        CompatFeatures::HAS_JOURNAL.bits(),
        IncompatFeatures::RECOVER.bits(),
        0,
    );

    assert!(features.has_journal());
    assert!(features.needs_recovery());
    assert_eq!(features.compat(), CompatFeatures::HAS_JOURNAL);
    assert_eq!(features.incompat(), IncompatFeatures::RECOVER);
}

#[def_test]
fn unknown_incompatible_bits_are_retained_and_rejected() {
    const UNKNOWN: u32 = 1 << 31;
    let features = FeatureSet::new(0, UNKNOWN, 0);

    assert_eq!(features.incompat().bits(), UNKNOWN);
    assert_eq!(
        features.validate_read_only(),
        Err(Ext4Error::UnsupportedFeature {
            class: FeatureClass::Incompatible,
            bits: UNKNOWN,
        })
    );
}
