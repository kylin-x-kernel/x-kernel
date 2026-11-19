// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::def_test;

use super::{AllocError, DmaMappingCompletion, release_after_restore};

#[def_test]
fn page_release_requires_confirmed_normal_mapping() {
    let releases = Arc::new(AtomicUsize::new(0));
    let release = || {
        let releases = Arc::clone(&releases);
        move || {
            releases.fetch_add(1, Ordering::Relaxed);
        }
    };
    assert!(release_after_restore(Err(AllocError::NoMemory), release()).is_none());
    for confirmed in [false, true] {
        let flush = DmaMappingCompletion::new(move || confirmed);
        let completion = release_after_restore(Ok(Some(flush)), release());
        assert_eq!(releases.load(Ordering::Relaxed), 0);
        assert_eq!(
            completion.map(DmaMappingCompletion::complete),
            Some(confirmed)
        );
    }
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    drop(release_after_restore(
        Ok(Some(DmaMappingCompletion::new(|| false))),
        release(),
    ));
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    assert!(release_after_restore(Ok(None), release()).is_none());
    assert_eq!(releases.load(Ordering::Relaxed), 2);
}

#[def_test]
fn coherent_allocation_requires_positive_confirmation() {
    use super::confirm_mapping;

    assert!(confirm_mapping(Ok(None)).is_ok());
    assert!(confirm_mapping(Ok(Some(DmaMappingCompletion::new(|| true)))).is_ok());
    assert!(confirm_mapping(Ok(Some(DmaMappingCompletion::new(|| false)))).is_err());
    assert!(confirm_mapping(Err(AllocError::NoMemory)).is_err());
}
