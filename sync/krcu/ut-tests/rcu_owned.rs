// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owning-container contracts independent of the runtime provider.

use core::sync::atomic::AtomicUsize;

use unittest::def_test;

use crate::Rcu;

#[def_test]
fn owned_container_accepts_send_only_and_shared_payloads() {
    fn require_send<T: Send>() {}
    fn require_sync<T: Sync>() {}

    // Moving exclusive ownership needs Send, not Sync. Sharing additionally
    // permits a different task to recover and destroy the replaced allocation.
    require_send::<Rcu<core::cell::Cell<u64>>>();
    require_send::<Rcu<u64>>();
    require_sync::<Rcu<u64>>();
    require_send::<Rcu<AtomicUsize>>();
    require_sync::<Rcu<AtomicUsize>>();
}
