// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

struct Backend;
impl DeviceFileOps for Backend {}

fn named_entry(device: DeviceId) -> Arc<ChrdevEntry> {
    let mut entry = ChrdevEntry::new(device, 1, Arc::new(Backend)).unwrap();
    entry.identity = Some(CharacterDeviceIdentity {
        name: String::from("registration_test"),
        class: String::from("test"),
    });
    Arc::new(entry)
}

#[def_test]
fn named_identity_exists_only_for_successful_live_registration() {
    let mut registry = ChrdevRegistry::new();
    let device = DeviceId::new(244, 12);
    let first = named_entry(device);
    registry.add(&first).unwrap();
    let snapshot = registry.named_devices();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].device_id(), device);
    assert_eq!(snapshot[0].name(), "registration_test");
    assert_eq!(snapshot[0].class(), "test");
    assert_eq!(
        registry.add(&named_entry(device)),
        Err(VfsError::ResourceBusy)
    );
    assert_eq!(registry.named_devices().len(), 1);
    assert_eq!(
        registry.add(&named_entry(DeviceId::new(244, 13))),
        Err(VfsError::ResourceBusy)
    );
    assert_eq!(registry.named_devices().len(), 1);
    registry.del(device, 1).unwrap();
    assert!(registry.named_devices().is_empty());
    assert!(!snapshot[0].is_registered());
    assert!(snapshot[0].operations().is_none());
    registry.add(&named_entry(device)).unwrap();
    let replacement = registry.named_devices();
    assert!(replacement[0].is_registered());
    assert!(!snapshot[0].same_registration(&replacement[0]));
    assert!(valid_component("tty0"));
    assert!(!valid_component("../tty0"));
    assert!(!valid_component(""));
}
