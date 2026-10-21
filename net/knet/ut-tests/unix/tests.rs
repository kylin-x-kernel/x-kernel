// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcred::Cred;
use kerrno::KError;
use unittest::{assert_eq, def_test};

use super::{ABSTRACT_BINDINGS, BindEntry, UnixAddr, lookup_bind_entry};

#[def_test]
fn unix_missing_abstract_address_returns_connection_refused() {
    let addr = UnixAddr::Abstract(Arc::from(&b"xkernel-missing-abstract-socket"[..]));

    assert_eq!(
        lookup_bind_entry(&addr, &Cred::root(), |_| Ok(())),
        Err(KError::ConnectionRefused)
    );
}

#[def_test(serial)]
fn unix_abstract_lookup_releases_registry_before_callback() {
    let name: Arc<[u8]> = Arc::from(&b"xkernel-abstract-lock-snapshot"[..]);
    let addr = UnixAddr::Abstract(name.clone());
    ABSTRACT_BINDINGS
        .lock()
        .insert(name.clone(), Arc::new(BindEntry::default()));

    let result = lookup_bind_entry(&addr, &Cred::root(), |_| {
        Ok(ABSTRACT_BINDINGS.try_lock().is_some())
    });

    ABSTRACT_BINDINGS.lock().remove(&name);
    assert_eq!(result, Ok(true));
}
