// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Discoverable identities for character devices without hardware topology.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use super::{CHRDEV_MAP, ChrdevEntry, ChrdevRegistry, DeviceFileOps};
use crate::{DeviceId, VfsError, VfsResult};

pub(super) struct CharacterDeviceIdentity {
    name: String,
    class: String,
}

impl CharacterDeviceIdentity {
    pub(super) fn matches(&self, other: &Self) -> bool {
        self.name == other.name && self.class == other.class
    }
}

/// Snapshot of one named cdev registration, including its retirement state.
///
/// A snapshot retains the backend but does not own or unregister its range.
/// Consumers must check availability before exposing attributes or opening it.
#[derive(Clone)]
pub struct CharacterDeviceRegistration(Arc<ChrdevEntry>);

impl CharacterDeviceRegistration {
    /// Device number assigned by the cdev registry.
    pub fn device_id(&self) -> DeviceId {
        self.0.first
    }

    /// Device-node basename, owned by the registration rather than a filesystem.
    pub fn name(&self) -> &str {
        &self.identity().name
    }

    /// Class directory name used by virtual-device filesystem projections.
    pub fn class(&self) -> &str {
        &self.identity().class
    }

    /// Whether this exact registration still owns its device number.
    pub fn is_registered(&self) -> bool {
        self.0.is_registered.load(Ordering::Acquire)
    }

    /// Whether two snapshots refer to the same registration lifetime.
    pub fn same_registration(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Returns the backend only while this registration is available.
    pub fn operations(&self) -> Option<Arc<dyn DeviceFileOps>> {
        self.0.live_operations()
    }

    fn identity(&self) -> &CharacterDeviceIdentity {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: named registration snapshots are \
                      constructed only for entries containing immutable identity metadata"
        )]
        self.0
            .identity
            .as_ref()
            .expect("named registration identity")
    }
}

impl ChrdevRegistry {
    fn named_devices(&self) -> Vec<CharacterDeviceRegistration> {
        self.entries
            .values()
            .filter(|entry| entry.identity.is_some())
            .cloned()
            .map(CharacterDeviceRegistration)
            .collect()
    }
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Registers one discoverable character device in sleepable task context.
///
/// Identity and dispatch enter the same registry transaction. The caller owns
/// the number until `cdev_del(device, 1)`. Notification callbacks run after the
/// registry lock is released; they must not re-enter the same publication.
///
/// # Errors
/// Returns `InvalidInput` for invalid name/class components or `ResourceBusy`
/// for an overlapping number or duplicate class/name. A failed registration
/// publishes no identity.
pub fn cdev_add_named(
    device: DeviceId,
    name: String,
    class: String,
    operations: Arc<dyn DeviceFileOps>,
) -> VfsResult<CharacterDeviceRegistration> {
    if !valid_component(&name) || !valid_component(&class) {
        return Err(VfsError::InvalidInput);
    }
    let mut entry = ChrdevEntry::new(device, 1, operations)?;
    entry.identity = Some(CharacterDeviceIdentity { name, class });
    let entry = Arc::new(entry);
    let subscribers = {
        let mut registry = CHRDEV_MAP.lock();
        registry.add(&entry)?;
        registry.subscribers.clone()
    };
    for subscriber in subscribers {
        subscriber();
    }
    Ok(CharacterDeviceRegistration(entry))
}

/// Snapshots all named cdevs currently registered, in sleepable task context.
pub fn registered_character_devices() -> Vec<CharacterDeviceRegistration> {
    CHRDEV_MAP.lock().named_devices()
}

/// Subscribes to named-cdev changes and atomically captures their current list.
///
/// Requires sleepable task context. Callbacks run without registry locks and
/// should refresh the list; consumers serialize snapshot installation with
/// callbacks to prevent an older snapshot from overtaking a removal.
pub fn subscribe_character_devices(
    callback: Arc<dyn Fn() + Send + Sync>,
) -> Vec<CharacterDeviceRegistration> {
    let mut registry = CHRDEV_MAP.lock();
    registry.subscribers.push(callback);
    registry.named_devices()
}

unittest::path_mod_test!(tests, "../../../ut-tests/node_device_registration.rs");
