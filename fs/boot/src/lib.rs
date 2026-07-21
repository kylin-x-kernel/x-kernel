// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem namespace preparation during kernel boot.
//!
//! This crate selects the root block device and builds the initial mount
//! namespace. The concrete root filesystem is supplied through
//! [`fs_block::FileSystemType`], so boot code does not branch on filesystem or
//! backend names. Direct tmpfs, procfs, devfs, and bpffs construction here is
//! initial namespace layout policy rather than filesystem-type dispatch.

#![cfg_attr(all(not(test), not(doc)), no_std)]

extern crate alloc;

#[macro_use]
extern crate log;

#[cfg(feature = "fs9p")]
use alloc::{boxed::Box, format, string::String};
use alloc::{sync::Arc, vec::Vec};

use kclass::{BlockDeviceImpl, ClassDevice, block_devices};
#[cfg(feature = "fs9p")]
use kclass::{Virtio9pDevice as _, Virtio9pDeviceImpl, virtio_9p_devices};
use kdevice::{DeviceId, subscribe_device_removed};
use ksync::{Mutex, static_lock};
use kvfs::{
    Filename, LookupFlags, LookupIntent, MntNamespace, NodePermission, Path, StatFsFlags,
    SuperBlock, path::PathBuf,
};

static_lock! {
    static FS_BACKING_DEVICES: Mutex<Vec<DeviceId>> = Mutex::new(Vec::new());
}

#[cfg(feature = "fs9p")]
struct Virtio9pTransport(Mutex<ClassDevice<Virtio9pDeviceImpl>>);

#[cfg(feature = "fs9p")]
impl v9fs::Transport for Virtio9pTransport {
    fn request(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, String> {
        let dev = self.0.lock();
        dev.request(req, resp)
            .map_err(|err| format!("virtio-9p error: {err:?}"))
    }
}

/// Prepares the initial mount namespace and init fs_struct.
pub fn prepare_namespace() {
    let root_fs = mount_root_super_block();
    BootVfs::install_initial_root(root_fs);
    kvfs::init_anon_inodefs();
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
        fs_context::init_fs()
            .lock()
            .attach_root(root.clone())
            .expect("root path must initialize init fs");
    }

    fn initial() -> Self {
        let namespace = MntNamespace::initial().expect("mount namespace must be initialized");
        let root = namespace.visible_root_path();
        Self { namespace, root }
    }

    fn mount_virtual_filesystems(&self) {
        self.mount_at("/dev", devfs::new_devfs())
            .expect("Failed to mount devfs");
        self.mount_at(
            "/dev/shm",
            memfs::shmem::new_tmpfs(
                StatFsFlags::NOSUID | StatFsFlags::NODEV | StatFsFlags::RELATIME,
            ),
        )
        .expect("Failed to mount /dev/shm");
        self.mount_at(
            "/tmp",
            memfs::shmem::new_tmpfs(
                StatFsFlags::NOSUID | StatFsFlags::NODEV | StatFsFlags::RELATIME,
            ),
        )
        .expect("Failed to mount /tmp");
        self.mount_at("/proc", procfs::new_procfs())
            .expect("Failed to mount procfs");
        self.mount_at(
            "/sys",
            memfs::ramfs::new_ramfs_with_name_and_flags(
                "sysfs",
                StatFsFlags::NOSUID
                    | StatFsFlags::NODEV
                    | StatFsFlags::NOEXEC
                    | StatFsFlags::RELATIME,
            ),
        )
        .expect("Failed to mount sysfs");

        #[cfg(feature = "ebpf")]
        {
            if self.lookup("/sys/fs").is_err() {
                self.mkdir_path("/sys/fs")
                    .expect("Failed to create /sys/fs");
            }
            self.mount_at("/sys/fs/bpf", bpffs::new_bpffs())
                .expect("Failed to mount bpffs");
        }
        self.create_sys_graphics_links()
            .expect("Failed to create sys graphics links");

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
        let (dir, name) = Filename::new(path.as_ref()).create_at(
            &self.root,
            &self.root,
            LookupIntent::Open,
            LookupFlags::DIRECTORY,
            &cred,
        )?;
        dir.mkdir(&name, NodePermission::from_bits_truncate(0o755), &cred)?;
        Ok(())
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

    fn mount_at(&self, path: &str, fs: Arc<SuperBlock>) -> kvfs::VfsResult<()> {
        let mountpoint = match self.lookup(path) {
            Ok(loc) if loc.is_dir() => loc,
            Ok(loc) => {
                let name = loc.name();
                loc.parent()
                    .ok_or(kvfs::VfsError::InvalidInput)?
                    .unlink(&name, &kcred::initial_cred())?;
                self.mkdir_path(path)?;
                self.lookup(path)?
            }
            Err(_) => {
                self.mkdir_path(path)?;
                self.lookup(path)?
            }
        };
        self.namespace.attach(&mountpoint, &fs)?;
        Ok(())
    }

    fn create_sys_graphics_links(&self) -> kvfs::VfsResult<()> {
        self.ensure_directory_path("/sys/class/graphics/fb0/device")?;
        let cred = kcred::initial_cred();
        let symlink_result = Filename::new("/sys/class/graphics/fb0/device/subsystem")
            .create_at(
                &self.root,
                &self.root,
                LookupIntent::Open,
                LookupFlags::empty(),
                &cred,
            )
            .and_then(|(dir, name)| dir.symlink(&name, "whatever", &cred));
        if let Err(err) = symlink_result
            && err != kvfs::VfsError::AlreadyExists
        {
            return Err(err);
        }
        Ok(())
    }

    #[cfg(feature = "fs9p")]
    fn mount_host_share(&self, mount_path: &str, fs: Arc<SuperBlock>) -> kvfs::VfsResult<()> {
        self.ensure_directory_path(mount_path)?;
        self.namespace.attach(&self.lookup(mount_path)?, &fs)?;
        Ok(())
    }
}

fn mount_root_super_block() -> Arc<kvfs::SuperBlock> {
    info!("Initialize filesystem subsystem...");

    let mut block_devs = block_devices();
    let handle = select_root_block(&mut block_devs);

    let backing_id = handle.id();
    subscribe_fs_backing_unregister(backing_id, "block");
    info!(
        "  use block device 0: {:?} (driver={}, {:?})",
        handle.name(),
        handle.driver_name(),
        handle.location(),
    );

    let fs = match fs_block::FileSystemType::mount_bdev(handle, StatFsFlags::empty()) {
        Ok(fs) => fs,
        Err(e) => {
            error!("Failed to mount root filesystem: {e:?}");
            panic!("VFS: Unable to mount root fs");
        }
    };
    info!("  filesystem type: {:?}", fs.name());
    fs
}

/// Chooses the block device used as the root filesystem.
fn select_root_block(devs: &mut Vec<ClassDevice<BlockDeviceImpl>>) -> ClassDevice<BlockDeviceImpl> {
    let preferred = kbuild_config::KFEAT_ROOT_BLOCK.trim();
    if !preferred.is_empty() {
        if let Some(index) = devs.iter().position(|device| device.name() == preferred) {
            return devs.remove(index);
        }
        warn!(
            "preferred root block device '{preferred}' not found among {:?}; falling back",
            devs.iter().map(|device| device.name()).collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "rootfs-secondary-block")]
    {
        assert!(devs.len() >= 2, "Less than two block devices found!");
        devs.remove(1)
    }
    #[cfg(not(feature = "rootfs-secondary-block"))]
    {
        devs.pop().expect("No block device found!")
    }
}

/// Mounts the host-share 9P filesystem into the initial namespace.
#[cfg(feature = "fs9p")]
pub fn mount_host_share(mount_path: &str) {
    let mut virtio_9p_devs = virtio_9p_devices();
    let handle = virtio_9p_devs.pop().expect("No virtio-9p device found!");
    let backing_id = handle.id();
    subscribe_fs_backing_unregister(backing_id, "virtio-9p");
    let mount_tag = handle.mount_tag();
    info!("Mount 9P filesystem...");
    info!("  use virtio-9p device: {:?}", handle.name());
    info!("  mount tag: {:?}", mount_tag);

    let transport = Box::new(Virtio9pTransport(Mutex::new(handle)));
    let fs =
        v9fs::Fs9pFilesystem::mount(transport, mount_tag).expect("Failed to initialize filesystem");
    info!("  filesystem type: {:?}", fs.name());
    BootVfs::initial()
        .mount_host_share(mount_path, fs)
        .expect("Failed to mount 9P filesystem");
    info!("  mounted at: {:?}", mount_path);
}

fn subscribe_fs_backing_unregister(id: DeviceId, label: &'static str) {
    FS_BACKING_DEVICES.lock().push(id);
    subscribe_device_removed(Arc::new(move |removed_id| {
        if removed_id != id {
            return;
        }
        let mut devices = FS_BACKING_DEVICES.lock();
        if let Some(pos) = devices.iter().position(|device_id| *device_id == id) {
            devices.swap_remove(pos);
            warn!(
                "filesystem: mounted {} backing device {:?} was removed; mounted filesystem is \
                 now stale",
                label, id
            );
        }
    }));
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
