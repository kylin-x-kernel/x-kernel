// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn raw_transfer_retains_one_strong_reference() {
    cases::raw_transfer_retains_one_strong_reference();
}

#[def_test]
fn independent_raw_transfers_preserve_extraction() {
    cases::independent_raw_transfers_preserve_extraction();
}

#[def_test]
fn raw_transfer_preserves_aligned_zst() {
    cases::raw_transfer_preserves_aligned_zst();
}

#[path = "../tests/fixtures/karc.rs"]
mod cases;

#[def_test]
fn shared_identity_and_counts() {
    cases::shared_identity_and_counts();
}

#[def_test]
fn weak_retains_storage_not_payload() {
    cases::weak_retains_storage_not_payload();
}

#[def_test]
fn empty_weak_cannot_upgrade() {
    cases::empty_weak_cannot_upgrade();
}

#[def_test]
fn extraction_invalidates_existing_weak() {
    cases::extraction_invalidates_existing_weak();
}

#[def_test]
fn failed_extraction_preserves_owner() {
    cases::failed_extraction_preserves_owner();
}

#[def_test]
fn reservation_failure_drops_input() {
    cases::reservation_failure_drops_input();
}

#[def_test]
fn invalid_flags_reject_input() {
    cases::invalid_flags_reject_input();
}

#[def_test]
fn zero_sized_values_still_need_shared_storage() {
    cases::zero_sized_values_still_need_shared_storage();
}

#[def_test]
fn aligned_value_preserves_alignment() {
    cases::aligned_value_preserves_alignment();
}

#[def_test]
fn shared_handles_are_send_and_sync_for_atomic_payloads() {
    cases::shared_handles_are_send_and_sync_for_atomic_payloads();
}
