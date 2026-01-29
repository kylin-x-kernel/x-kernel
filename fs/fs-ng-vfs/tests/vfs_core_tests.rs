extern crate alloc;

#[test]
fn test_mountpoint_thread_safety() {
    use fs_ng_vfs::Mountpoint;

    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mountpoint>();
}

#[test]
fn test_reference_creation() {
    use fs_ng_vfs::Reference;

    let root_ref = Reference::root();
    let root_key = root_ref.key();
    assert_eq!(root_key.1, "");

    let child_ref = Reference::new(None, "test.txt".into());
    let child_key = child_ref.key();
    assert_eq!(child_key.1, "test.txt");
}

#[test]
fn test_reference_key_uniqueness() {
    use fs_ng_vfs::Reference;

    let ref1 = Reference::new(None, "file1.txt".into());
    let ref2 = Reference::new(None, "file2.txt".into());
    let ref3 = Reference::new(None, "file1.txt".into());

    assert_ne!(ref1.key(), ref2.key());

    assert_eq!(ref1.key(), ref3.key());
}

#[test]
fn test_node_type_conversion() {
    use fs_ng_vfs::NodeType;

    assert_eq!(NodeType::from(0o4), NodeType::Directory);
    assert_eq!(NodeType::from(0o10), NodeType::RegularFile);
    assert_eq!(NodeType::from(0o12), NodeType::Symlink);
    assert_eq!(NodeType::from(0o14), NodeType::Socket);
    assert_eq!(NodeType::from(0o1), NodeType::Fifo);
    assert_eq!(NodeType::from(0o2), NodeType::CharacterDevice);
    assert_eq!(NodeType::from(0o6), NodeType::BlockDevice);

    assert_eq!(NodeType::from(0xFF), NodeType::Unknown);
}

#[test]
fn test_node_permission_flags() {
    use fs_ng_vfs::NodePermission;

    let rwx_owner =
        NodePermission::OWNER_READ | NodePermission::OWNER_WRITE | NodePermission::OWNER_EXEC;
    assert_eq!(rwx_owner.bits(), 0o700);

    let default_perm = NodePermission::default();
    assert!(default_perm.contains(NodePermission::OWNER_READ));
    assert!(default_perm.contains(NodePermission::OWNER_WRITE));
    assert!(default_perm.contains(NodePermission::GROUP_READ));
    assert!(default_perm.contains(NodePermission::GROUP_WRITE));
    assert!(default_perm.contains(NodePermission::OTHER_READ));
    assert!(default_perm.contains(NodePermission::OTHER_WRITE));

    let setuid = NodePermission::SET_UID;
    assert_eq!(setuid.bits(), 0o4000);

    let setgid = NodePermission::SET_GID;
    assert_eq!(setgid.bits(), 0o2000);

    let sticky = NodePermission::STICKY;
    assert_eq!(sticky.bits(), 0o1000);
}

#[test]
fn test_vfs_error_types() {
    use fs_ng_vfs::VfsError;

    let _not_found = VfsError::NotFound;
    let _already_exists = VfsError::AlreadyExists;
    let _not_a_directory = VfsError::NotADirectory;
    let _is_a_directory = VfsError::IsADirectory;
    let _directory_not_empty = VfsError::DirectoryNotEmpty;
    let _permission_denied = VfsError::PermissionDenied;
    let _invalid_input = VfsError::InvalidInput;
}

#[test]
fn test_filesystem_type_constraints() {
    use fs_ng_vfs::Filesystem;

    fn assert_traits<T: Send + Sync + Clone>() {}
    assert_traits::<Filesystem>();
}

#[test]
fn test_path_component_parsing() {
    use fs_ng_vfs::path::{Component, Path};

    let path = Path::new("/path/to/file.txt");
    let components: alloc::vec::Vec<_> = path.components().collect();

    assert_eq!(components.len(), 4);

    assert!(matches!(components[0], Component::RootDir));

    match &components[1] {
        Component::Normal(name) => assert_eq!(*name, "path"),
        _ => panic!("Expected Normal component"),
    }

    match &components[2] {
        Component::Normal(name) => assert_eq!(*name, "to"),
        _ => panic!("Expected Normal component"),
    }

    match &components[3] {
        Component::Normal(name) => assert_eq!(*name, "file.txt"),
        _ => panic!("Expected Normal component"),
    }
}

#[test]
fn test_path_normalization() {
    use fs_ng_vfs::path::PathBuf;

    let path = PathBuf::from("/path/to/file");
    assert_eq!(path.as_str(), "/path/to/file");

    let root = PathBuf::from("/");
    assert_eq!(root.as_str(), "/");

    let empty = PathBuf::new();
    assert_eq!(empty.as_str(), "");
}

#[test]
fn test_direntry_weak_relationship() {
    use fs_ng_vfs::{DirEntry, WeakDirEntry};

    fn assert_downgrade(_entry: &DirEntry) -> WeakDirEntry {
        _entry.downgrade()
    }

    fn assert_upgrade(_weak: &WeakDirEntry) -> Option<DirEntry> {
        _weak.upgrade()
    }
}
