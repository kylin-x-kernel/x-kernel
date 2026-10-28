// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPC namespace drop-hook semantics.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use crate::ipc::{register_ipc_ns_drop_hook, IpcNamespace};

/// The drop observer runs exactly once when the namespace's last reference
/// is released, and registration conflicts surface as an error.
#[def_test]
fn ipc_namespace_drop_hook_runs_on_last_reference() {
    let fired = Arc::new(AtomicUsize::new(0));
    let namespace = Arc::new(IpcNamespace::new());
    let id = namespace.id();

    register_ipc_ns_drop_hook(id, {
        let fired = fired.clone();
        Arc::new(move || {
            fired.fetch_add(1, Ordering::Relaxed);
        })
    })
    .expect("a fresh namespace has no hook yet");
    assert!(
        register_ipc_ns_drop_hook(id, Arc::new(|| {})).is_err(),
        "a second registration for the same namespace is refused"
    );

    // Cloning the namespace (a process holding it) keeps the hook parked.
    let holder = namespace.clone();
    drop(namespace);
    assert_eq!(fired.load(Ordering::Relaxed), 0, "a live reference parks the hook");

    drop(holder);
    assert_eq!(
        fired.load(Ordering::Relaxed),
        1,
        "the hook runs once at the last reference"
    );

    // The slot is free again after the hook fired.
    assert!(
        register_ipc_ns_drop_hook(id, Arc::new(|| {})).is_ok(),
        "the fired hook is gone and the slot reusable"
    );
}
