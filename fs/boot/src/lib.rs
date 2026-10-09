// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem namespace preparation during kernel boot.
//!
//! This crate selects the root block device and builds the initial mount
//! namespace. Configured filesystems register their canonical KVFS type
//! descriptors through kernel initcalls before this crate probes registered
//! block-backed types or mounts the built-in virtual filesystems.

#![cfg_attr(any(not(test), doc), no_std)]

extern crate alloc;

#[macro_use]
extern crate log;

#[cfg(feature = "fs9p")]
use alloc::boxed::Box;
use alloc::{format, sync::Arc, vec::Vec};

#[cfg(feature = "fs9p")]
use kclass::{
    ClassDevice, Virtio9pDevice as _, Virtio9pDeviceImpl, prelude::DriverError, virtio_9p_devices,
};
#[cfg(feature = "fs9p")]
use kerrno::KError;
#[cfg(feature = "fs9p")]
use kvfs::{FileSystemType, FsContext, FsContextOperations};
use kvfs::{
    Filename, LookupFlags, LookupIntent, MntNamespace, MountFlags, NodePermission, Path,
    SuperBlock, SuperBlockFlags, path::PathBuf,
};

const PSEUDO_FS_MOUNT_FLAGS: MountFlags = MountFlags::NOSUID
    .union(MountFlags::NODEV)
    .union(MountFlags::NOEXEC)
    .union(MountFlags::RELATIME);
// Linux's safe devtmpfs policy is `nosuid,noexec`; `nodev` would prevent the
// device filesystem from serving its defining purpose.
const DEVFS_MOUNT_FLAGS: MountFlags = MountFlags::NOSUID
    .union(MountFlags::NOEXEC)
    .union(MountFlags::RELATIME);
const TMPFS_MOUNT_FLAGS: MountFlags = MountFlags::NOSUID
    .union(MountFlags::NODEV)
    .union(MountFlags::RELATIME);

/// Initializes device discovery projections and lifecycle publication.
///
/// This runs before driver discovery so sysfs paths are projected before add
/// events and removal ordering is owned by the device lifecycle coordinator,
/// independent of when userspace mounts sysfs.
pub fn init_device_discovery() {
    sysfs::init_device_projection();
    kdevice_uevent::init();
}

#[cfg(feature = "fs9p")]
struct Virtio9pTransport(ClassDevice<Virtio9pDeviceImpl>);

#[cfg(feature = "fs9p")]
impl p9::P9Transport for Virtio9pTransport {
    fn request(&mut self, request: &[u8], response: &mut [u8]) -> kerrno::KResult<usize> {
        self.0
            .request(request, response)
            .map_err(map_9p_driver_error)
    }
}

#[cfg(feature = "fs9p")]
fn map_9p_driver_error(error: DriverError) -> KError {
    match error {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::BadState => KError::BadState,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::Io => KError::Io,
        DriverError::NoMemory => KError::NoMemory,
        DriverError::ReadOnly => KError::ReadOnlyFilesystem,
        DriverError::ResourceBusy => KError::ResourceBusy,
        DriverError::Unsupported => KError::OperationNotSupported,
    }
}

#[cfg(feature = "fs9p")]
fn host_9p_get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &Path,
    _lookup_pwd: &Path,
) -> kvfs::VfsResult<Arc<SuperBlock>> {
    let mut devices = virtio_9p_devices();
    let handle = match context.source().filter(|mount_tag| !mount_tag.is_empty()) {
        Some(mount_tag) => devices
            .into_iter()
            .find(|device| device.mount_tag() == mount_tag),
        None => devices.pop(),
    }
    .ok_or(kvfs::VfsError::NoSuchDevice)?;
    info!("Mount 9P filesystem...");
    info!("  use virtio-9p device: {:?}", handle.name());
    info!("  mount tag: {:?}", handle.mount_tag());

    let transport = Box::new(Virtio9pTransport(handle));
    kvfs::get_tree_nodev(context, move |file_system_type, superblock_flags| {
        v9fs::mount(file_system_type, superblock_flags, transport)
    })
}

#[cfg(feature = "fs9p")]
static HOST_9P_CONTEXT_OPERATIONS: FsContextOperations = FsContextOperations::new(host_9p_get_tree);

#[cfg(feature = "fs9p")]
fn init_host_9p_context(context: &mut FsContext<'_>) -> kvfs::VfsResult<()> {
    context.set_operations(&HOST_9P_CONTEXT_OPERATIONS);
    Ok(())
}

#[cfg(feature = "fs9p")]
static HOST_9P_TYPE: FileSystemType = FileSystemType::nodev("9p", init_host_9p_context);

#[cfg(feature = "fs9p")]
#[macros::register_init]
fn init_host_9p_fs() {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Each statically named filesystem is \
                  registered once by its unique linker initialization entry"
    )]
    kvfs::register_filesystem(&HOST_9P_TYPE).expect("9P filesystem type must register once");
}

/// Prepares the initial mount namespace from registered filesystem types.
///
/// # Panics
///
/// Panics if filesystem initcalls have not registered the required canonical
/// types, or if bootstrap mounts, root device selection, root filesystem
/// construction, or init `fs_struct` setup fails.
pub fn prepare_namespace() {
    let bootstrap = memfs::ramfs::new_rootfs(SuperBlockFlags::empty());
    BootVfs::install_initial_root(bootstrap);

    let boot = BootVfs::initial();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create bootstrap \
                  /dev mountpoint; filesystem startup has no error return or recovery"
    )]
    boot.ensure_directory_path("/dev")
        .expect("Failed to create bootstrap /dev mountpoint");
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount bootstrap \
                  devfs; filesystem startup has no error return or recovery"
    )]
    boot.mount_at(
        "/dev",
        &devfs::FILE_SYSTEM_TYPE,
        None,
        SuperBlockFlags::empty(),
        DEVFS_MOUNT_FLAGS,
    )
    .expect("Failed to mount bootstrap devfs");
    mount_root_file_system(&boot);
    let root = boot.namespace.visible_root_path();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Boot installs a directory root from the \
                  newly constructed root filesystem before user tasks run"
    )]
    fs_context::init_fs()
        .lock()
        .replace_root_and_pwd(root.clone(), root)
        .expect("real root path must replace bootstrap root");
    anon_inodefs::init_anon_inodefs();
    pipefs::init_pipefs();
}

/// Mounts boot-time virtual filesystems into the initial namespace.
pub fn mount_virtual_filesystems() {
    info!("Initialize VFS...");
    devfs::capture_firmware_dtb_snapshot();
    BootVfs::initial().mount_virtual_filesystems();
}

struct BootVfs {
    namespace: Arc<MntNamespace>,
    root: Path,
}

impl BootVfs {
    fn install_initial_root(root_fs: Arc<SuperBlock>) {
        let namespace = MntNamespace::init_mount_tree(&root_fs);
        let root = namespace.visible_root_path();
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Boot installs a directory root from \
                      the newly constructed root filesystem before user tasks run"
        )]
        fs_context::init_fs()
            .lock()
            .attach_root(root.clone())
            .expect("root path must initialize init fs");
    }

    fn initial() -> Self {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: install_initial_root created the \
                      permanent initial mount namespace before BootVfs::initial"
        )]
        let namespace = MntNamespace::initial().expect("mount namespace must be initialized");
        let root = namespace.visible_root_path();
        Self { namespace, root }
    }

    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: /dev/log cannot be bound for \
                  a reason other than missing support; virtual filesystem setup has no error \
                  return or recovery"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: linear registration table: each block creates and \
                  mounts one independent virtual filesystem"
    )]
    fn mount_virtual_filesystems(&self) {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create /dev \
                      mountpoint; filesystem startup has no error return or recovery"
        )]
        self.ensure_directory_path("/dev")
            .expect("Failed to create /dev mountpoint");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount devfs; \
                      filesystem startup has no error return or recovery"
        )]
        self.mount_at(
            "/dev",
            &devfs::FILE_SYSTEM_TYPE,
            None,
            SuperBlockFlags::empty(),
            DEVFS_MOUNT_FLAGS,
        )
        .expect("Failed to mount devfs");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create /dev/shm \
                      mountpoint; filesystem startup has no error return or recovery"
        )]
        self.ensure_directory_path("/dev/shm")
            .expect("Failed to create /dev/shm mountpoint");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount /dev/shm; \
                      filesystem startup has no error return or recovery"
        )]
        self.mount_at(
            "/dev/shm",
            &memfs::TMPFS_TYPE,
            None,
            SuperBlockFlags::empty(),
            TMPFS_MOUNT_FLAGS,
        )
        .expect("Failed to mount /dev/shm");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create /tmp \
                      mountpoint; filesystem startup has no error return or recovery"
        )]
        self.ensure_directory_path("/tmp")
            .expect("Failed to create /tmp mountpoint");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount /tmp; \
                      filesystem startup has no error return or recovery"
        )]
        self.mount_at(
            "/tmp",
            &memfs::TMPFS_TYPE,
            None,
            SuperBlockFlags::empty(),
            TMPFS_MOUNT_FLAGS,
        )
        .expect("Failed to mount /tmp");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create /proc \
                      mountpoint; filesystem startup has no error return or recovery"
        )]
        self.ensure_directory_path("/proc")
            .expect("Failed to create /proc mountpoint");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount procfs; \
                      filesystem startup has no error return or recovery"
        )]
        self.mount_at(
            "/proc",
            &procfs::FILE_SYSTEM_TYPE,
            None,
            SuperBlockFlags::empty(),
            PSEUDO_FS_MOUNT_FLAGS,
        )
        .expect("Failed to mount procfs");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create /sys \
                      mountpoint; filesystem startup has no error return or recovery"
        )]
        self.ensure_directory_path("/sys")
            .expect("Failed to create /sys mountpoint");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount sysfs; \
                      filesystem startup has no error return or recovery"
        )]
        self.mount_at(
            "/sys",
            &sysfs::FILE_SYSTEM_TYPE,
            None,
            SuperBlockFlags::empty(),
            PSEUDO_FS_MOUNT_FLAGS,
        )
        .expect("Failed to mount sysfs");

        #[cfg(feature = "cgroup2")]
        {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create \
                          /sys/fs/cgroup mountpoint; filesystem startup has no error return or \
                          recovery"
            )]
            self.ensure_directory_path("/sys/fs/cgroup")
                .expect("Failed to create /sys/fs/cgroup mountpoint");
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: The immediately preceding \
                          ensure_directory_path succeeded during serialized boot filesystem setup"
            )]
            let mountpoint = self
                .lookup("/sys/fs/cgroup")
                .expect("Failed to resolve /sys/fs/cgroup mountpoint");
            let super_block = cgroup2fs::new_initial_cgroup2fs(SuperBlockFlags::empty());
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount \
                          cgroup2fs; filesystem startup has no error return or recovery"
            )]
            self.namespace
                .attach_with_flags_and_devname(
                    &mountpoint,
                    &super_block,
                    PSEUDO_FS_MOUNT_FLAGS,
                    None,
                )
                .expect("Failed to mount cgroup2fs");
        }

        #[cfg(feature = "ebpf")]
        {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create \
                          /sys/fs/bpf mountpoint; filesystem startup has no error return or \
                          recovery"
            )]
            self.ensure_directory_path("/sys/fs/bpf")
                .expect("Failed to create /sys/fs/bpf mountpoint");
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount \
                          bpffs; filesystem startup has no error return or recovery"
            )]
            self.mount_at(
                "/sys/fs/bpf",
                &bpffs::FILE_SYSTEM_TYPE,
                None,
                SuperBlockFlags::empty(),
                PSEUDO_FS_MOUNT_FLAGS,
            )
            .expect("Failed to mount bpffs");
        }
        if let Err(err) = devfs::bind_dev_log() {
            if err != kerrno::LinuxError::ENOSYS && err != kerrno::LinuxError::EOPNOTSUPP {
                panic!("Failed to bind dev-log: {err}");
            }
            warn!("/dev/log not available: {err}");
        }
    }

    fn lookup(&self, path: impl AsRef<str>) -> kvfs::VfsResult<Path> {
        let cred = kcred::initial_cred();
        Filename::new(path.as_ref()).lookup_at(
            &self.root,
            &self.root,
            LookupIntent::Open,
            LookupFlags::follow(),
            &cred,
        )
    }

    fn mkdir_path(&self, path: impl AsRef<str>) -> kvfs::VfsResult<()> {
        let cred = kcred::initial_cred();
        Filename::new(path.as_ref())
            .mkdir_at(
                &self.root,
                &self.root,
                NodePermission::from_bits_truncate(0o755),
                NodePermission::empty(),
                &cred,
            )
            .map(|_| ())
    }

    fn ensure_directory_path(&self, path: &str) -> kvfs::VfsResult<()> {
        let mut current = PathBuf::new();
        for_each_lexical_component(path, |component| {
            current.push(component);
            if self.lookup(&current).is_err() {
                self.mkdir_path(&current)?;
            }
            Ok(())
        })
    }

    fn mount_at(
        &self,
        path: &str,
        file_system_type: &'static kvfs::FileSystemType,
        source: Option<&str>,
        superblock_flags: SuperBlockFlags,
        mount_flags: MountFlags,
    ) -> kvfs::VfsResult<()> {
        let mountpoint = self.lookup(path)?;
        if !mountpoint.is_dir() {
            return Err(kvfs::VfsError::NotADirectory);
        }
        let cred = kcred::initial_cred();
        let mut context =
            kvfs::FsContext::new(file_system_type, source, None, superblock_flags, &cred)?;
        self.namespace.mount_new(
            &mountpoint,
            mount_flags,
            &mut context,
            &self.root,
            &self.root,
        )?;
        Ok(())
    }
}

#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the root block device cannot be \
              mounted (a non-probe mount error, or every registered filesystem rejects it); root \
              mounting has no error return or recovery"
)]
fn mount_root_file_system(boot: &BootVfs) {
    info!("Initialize filesystem subsystem...");

    let mut block_devs = block::block_devices()
        .into_iter()
        .filter(|device| device.num_sectors() != 0)
        .collect();
    let handle = select_root_block(&mut block_devs);
    let source = format!("/dev/{}", handle.name());

    info!(
        "  use block device 0: {:?} ({:?})",
        handle.name(),
        handle.device_number(),
    );

    let file_system_types: Vec<_> = kvfs::registered_filesystems()
        .into_iter()
        .filter(|file_system_type| file_system_type.requires_device())
        .collect();
    let cred = kcred::initial_cred();
    for superblock_flags in [SuperBlockFlags::empty(), SuperBlockFlags::RDONLY] {
        let mut mount_flags = MountFlags::RELATIME;
        if superblock_flags.contains(SuperBlockFlags::RDONLY) {
            mount_flags.insert(MountFlags::RDONLY);
        }

        for file_system_type in &file_system_types {
            let result = kvfs::FsContext::new(
                file_system_type,
                Some(&source),
                None,
                superblock_flags,
                &cred,
            )
            .and_then(|mut context| {
                boot.namespace.mount_new(
                    &boot.root,
                    mount_flags,
                    &mut context,
                    &boot.root,
                    &boot.root,
                )
            });
            match result {
                Ok(mount) => {
                    info!("  filesystem type: {:?}", mount.filesystem_name());
                    return;
                }
                Err(err)
                    if matches!(
                        kerrno::LinuxError::from(err),
                        kerrno::LinuxError::EACCES | kerrno::LinuxError::EINVAL
                    ) =>
                {
                    debug!(
                        "root filesystem probe as {} failed: {err:?}",
                        file_system_type.name()
                    );
                }
                Err(err) => {
                    error!(
                        "Failed to mount root filesystem as {}: {err:?}",
                        file_system_type.name()
                    );
                    panic!("VFS: Unable to mount root fs");
                }
            }
        }
    }
    error!("Failed to mount {source} using any registered block filesystem type");
    panic!("VFS: Unable to mount root fs");
}

/// Chooses the block device used as the root filesystem.
fn select_root_block(devs: &mut Vec<Arc<block::BlockDevice>>) -> Arc<block::BlockDevice> {
    let preferred = kbuild_config::KFEAT_ROOT_BLOCK.trim();
    if !preferred.is_empty() {
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the configured root block \
                      device is absent; root mounting has no error return or recovery"
        )]
        let index = devs
            .iter()
            .position(|device| device.name() == preferred)
            .unwrap_or_else(|| {
                panic!(
                    "root block device '{preferred}' not found among {:?}",
                    devs.iter().map(|device| device.name()).collect::<Vec<_>>()
                )
            });
        return devs.remove(index);
    }

    #[cfg(feature = "rootfs-secondary-block")]
    {
        assert!(devs.len() >= 2, "Less than two block devices found!");
        devs.remove(1)
    }
    #[cfg(not(feature = "rootfs-secondary-block"))]
    {
        assert!(!devs.is_empty(), "No block device found!");
        devs.remove(0)
    }
}

/// Mounts the host-share 9P filesystem into the initial namespace.
#[cfg(feature = "fs9p")]
pub fn mount_host_share(mount_path: &str) {
    let boot = BootVfs::initial();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to create the 9P \
                  host-share mountpoint; filesystem startup has no error return or recovery"
    )]
    boot.ensure_directory_path(mount_path)
        .expect("Failed to create 9P mountpoint path");
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: failed to mount the 9P host \
                  share; filesystem startup has no error return or recovery"
    )]
    boot.mount_at(
        mount_path,
        &HOST_9P_TYPE,
        None,
        SuperBlockFlags::empty(),
        MountFlags::RELATIME,
    )
    .expect("Failed to mount 9P filesystem");
    info!("  mounted at: {:?}", mount_path);
}

fn for_each_lexical_component<E>(
    path: &str,
    mut f: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let mut rest = path;
    let mut at_start = true;
    while !rest.is_empty() {
        let (component, next) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index + 1..]),
            None => (rest, ""),
        };
        rest = next;
        let component = match component {
            "" if at_start => Some("/"),
            "" => None,
            "." if at_start => Some("."),
            "." => None,
            name => Some(name),
        };
        at_start = false;
        if let Some(component) = component {
            f(component)?;
        }
    }
    Ok(())
}
