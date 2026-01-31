#![cfg(unittest)]

use unittest::{assert_eq, def_test};

use crate::types::{DeviceId, NodePermission, NodeType};

#[def_test]
fn test_node_type_conversion() {
    // Test NodeType from u8 conversion
    assert_eq!(NodeType::from(0o1), NodeType::Fifo);
    assert_eq!(NodeType::from(0o2), NodeType::CharacterDevice);
    assert_eq!(NodeType::from(0o4), NodeType::Directory);
    assert_eq!(NodeType::from(0o6), NodeType::BlockDevice);
    assert_eq!(NodeType::from(0o10), NodeType::RegularFile);
    assert_eq!(NodeType::from(0o12), NodeType::Symlink);
    assert_eq!(NodeType::from(0o14), NodeType::Socket);

    // Test unknown/invalid values
    assert_eq!(NodeType::from(0o0), NodeType::Unknown);
    assert_eq!(NodeType::from(0o77), NodeType::Unknown);
}

#[def_test]
fn test_node_permission_bitflags() {
    // Test permission combinations
    let rwx = NodePermission::OWNER_READ | NodePermission::OWNER_WRITE | NodePermission::OWNER_EXEC;
    assert!(rwx.contains(NodePermission::OWNER_READ));
    assert!(rwx.contains(NodePermission::OWNER_WRITE));
    assert!(rwx.contains(NodePermission::OWNER_EXEC));
    assert!(!rwx.contains(NodePermission::GROUP_READ));

    // Test default permissions (0o666)
    let default = NodePermission::default();
    assert!(default.contains(NodePermission::OWNER_READ));
    assert!(default.contains(NodePermission::OWNER_WRITE));
    assert!(!default.contains(NodePermission::OWNER_EXEC));
    assert!(default.contains(NodePermission::GROUP_READ));
    assert!(default.contains(NodePermission::GROUP_WRITE));
    assert!(default.contains(NodePermission::OTHER_READ));
    assert!(default.contains(NodePermission::OTHER_WRITE));

    // Test special bits
    let special = NodePermission::SET_UID | NodePermission::SET_GID | NodePermission::STICKY;
    assert!(special.contains(NodePermission::SET_UID));
    assert!(special.contains(NodePermission::SET_GID));
    assert!(special.contains(NodePermission::STICKY));
}

#[def_test]
fn test_device_id_major_minor() {
    // Test DeviceId major/minor extraction
    let dev1 = DeviceId::new(1, 2);
    assert_eq!(dev1.major(), 1);
    assert_eq!(dev1.minor(), 2);

    // Test with larger numbers
    let dev2 = DeviceId::new(0x1234, 0x5678);
    assert_eq!(dev2.major(), 0x1234);
    assert_eq!(dev2.minor(), 0x5678);

    // Test edge cases
    let dev3 = DeviceId::new(0, 0);
    assert_eq!(dev3.major(), 0);
    assert_eq!(dev3.minor(), 0);

    let dev4 = DeviceId::new(0xFFFFFFFF, 0xFFFFFFFF);
    assert_eq!(dev4.major(), 0xFFFFFFFF);
    assert_eq!(dev4.minor(), 0xFFFFFFFF);
}
