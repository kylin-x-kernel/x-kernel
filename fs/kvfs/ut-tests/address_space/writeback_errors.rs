// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error observation across opens, independent channels, and counter wrap.

use unittest::def_test;

use super::WritebackErrors;
use crate::VfsError;

#[def_test]
fn unseen_failures_survive_sampling_and_non_consuming_checks() {
    let errors = WritebackErrors::new();
    let mut existing = errors.sample();
    errors.record(VfsError::Io);
    let mut reopened = errors.sample();
    assert_eq!(errors.check(reopened), Err(VfsError::Io));
    assert_eq!(errors.check(errors.sample()), Err(VfsError::Io));
    assert_eq!(errors.check_and_advance(&mut reopened), Err(VfsError::Io));
    assert_eq!(errors.check_and_advance(&mut reopened), Ok(()));
    assert_eq!(errors.check(errors.sample()), Ok(()));
    assert_eq!(errors.check_and_advance(&mut existing), Err(VfsError::Io));
    errors.record(VfsError::StorageFull);
    errors.record(VfsError::Io);
    assert_eq!(errors.check_and_advance(&mut reopened), Err(VfsError::Io));
}

#[def_test]
fn operation_snapshot_preserves_unseen_history_without_reporting_it_again() {
    let errors = WritebackErrors::new();
    let mut existing = errors.sample();
    errors.record(VfsError::Io);
    let generation = errors.current_generation();
    assert_eq!(errors.check(generation), Ok(()));
    let mut reopened = errors.sample();
    assert_eq!(errors.check(reopened), Err(VfsError::Io));
    assert_eq!(errors.check_and_advance(&mut existing), Err(VfsError::Io));
    // Observation changes do not count as new failures during an operation.
    assert_eq!(errors.check(generation), Ok(()));
    assert_eq!(errors.check_and_advance(&mut reopened), Err(VfsError::Io));
    errors.record(VfsError::StorageFull);
    assert_eq!(errors.check(generation), Err(VfsError::StorageFull));
    assert_eq!(errors.check(generation), Err(VfsError::StorageFull));
    let mut later = errors.sample();
    assert_eq!(
        errors.check_and_advance(&mut later),
        Err(VfsError::StorageFull)
    );
}

#[def_test]
fn operation_snapshot_detects_new_failures_across_generation_wrap() {
    let errors = WritebackErrors::new();
    errors.state.lock().generation = u64::MAX;
    let generation = errors.current_generation();
    errors.record(VfsError::Io);
    assert_eq!(errors.check(generation), Err(VfsError::Io));
    assert_eq!(errors.check(errors.current_generation()), Ok(()));
    assert_eq!(errors.check(errors.sample()), Err(VfsError::Io));
}

#[def_test]
fn mapping_observation_does_not_consume_the_filesystem_channel() {
    let mapping = WritebackErrors::new();
    let filesystem = WritebackErrors::new();
    mapping.record(VfsError::Io);
    filesystem.record(VfsError::Io);
    let mut file_cursor = mapping.sample();
    assert_eq!(
        mapping.check_and_advance(&mut file_cursor),
        Err(VfsError::Io)
    );
    assert_eq!(mapping.check(mapping.sample()), Ok(()));
    let mut reopened_filesystem_cursor = filesystem.sample();
    assert_eq!(
        filesystem.check_and_advance(&mut reopened_filesystem_cursor),
        Err(VfsError::Io)
    );
}

#[def_test]
fn unseen_failure_remains_observable_when_generation_wraps() {
    let errors = WritebackErrors::new();
    errors.state.lock().generation = u64::MAX;
    let mut existing = errors.sample();
    errors.record(VfsError::Io);
    let mut reopened = errors.sample();
    assert_eq!(errors.check_and_advance(&mut reopened), Err(VfsError::Io));
    assert_eq!(errors.check(errors.sample()), Ok(()));
    assert_eq!(errors.check_and_advance(&mut existing), Err(VfsError::Io));
}
