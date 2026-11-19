// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network procfs nodes under `/proc/net` and `/proc/sys/net`.

use alloc::{borrow::Cow, format, sync::Arc};

use kalloc_types::KBox;
use kprocess::AsThread;
use kvfs::{
    Dentry, DirMapping, NodePermission, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFile,
    SimpleFileOps, SimpleFs, VfsError, VfsFile, VfsResult,
};

/// Reads require sleepable task context without stack or namespace-registry
/// locks. Writes additionally require a current process thread.
struct IpForwardFile {
    namespace: Arc<kns::NetNamespace>,
}

impl SimpleFileOps for IpForwardFile {
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let value = knet::netns::ipv4_forwarding(&self.namespace)?;
        Ok(Cow::Owned(format!("{value}\n").into_bytes()))
    }

    fn write(&self, _file: &VfsFile, data: &[u8], offset: u64) -> VfsResult<usize> {
        let task = ktask::current();
        task.try_as_thread().ok_or(VfsError::NotFound)?;
        if !kprocess::current_cred().has_cap(kcred::CAP_NET_ADMIN) {
            return Err(VfsError::OperationNotPermitted);
        }
        if offset != 0 || data.is_empty() {
            return Ok(data.len());
        }
        let value = core::str::from_utf8(data)
            .map_err(|_| VfsError::InvalidInput)?
            .trim_ascii()
            .parse::<i32>()
            .map_err(|_| VfsError::InvalidInput)?;
        knet::netns::set_ipv4_forwarding(&self.namespace, value)?;
        Ok(data.len())
    }
}

/// Requires a current process thread in task context, never IRQ context.
/// May sleep while reading process namespace state; callers must hold no
/// spinlock, IRQ/preemption-disable guard, or process namespace write lock.
fn current_namespace() -> VfsResult<Arc<kns::NetNamespace>> {
    let task = ktask::current();
    let thread = task.try_as_thread().ok_or(VfsError::NotFound)?;
    thread.process().net_ns()
}

struct Ipv4SysctlDir {
    fs: Arc<SimpleFs>,
}

impl SimpleDirOps for Ipv4SysctlDir {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(KBox::new(core::iter::once(Cow::Borrowed("ip_forward")))?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        if name != "ip_forward" {
            return Err(VfsError::NotFound);
        }
        lookup.file(
            name,
            SimpleFile::new_regular_with_permission(
                self.fs.clone(),
                NodePermission::OWNER_READ
                    | NodePermission::OWNER_WRITE
                    | NodePermission::GROUP_READ
                    | NodePermission::OTHER_READ,
                IpForwardFile {
                    namespace: current_namespace()?,
                },
            ),
        )
    }

    fn supports_dentry_cache(&self) -> bool {
        // Each pathname lookup must resolve the caller's namespace.
        false
    }
}

/// Registers `/proc/sys/net` controls for the caller's network namespace.
pub(crate) fn add_sys_entries(sys: &mut DirMapping, fs: Arc<SimpleFs>) {
    let mut net = DirMapping::new();
    net.add(
        "ipv4",
        SimpleDir::new_maker(fs.clone(), Arc::new(Ipv4SysctlDir { fs: fs.clone() })),
    );
    sys.add("net", SimpleDir::new_maker(fs, Arc::new(net)));
}

/// Registers `/proc/net`, including the built-in table enumeration used by unqualified iptables-save.
pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    let mut net = DirMapping::new();
    net.add(
        "ip_tables_names",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::OWNER_READ | NodePermission::GROUP_READ,
            || {
                Ok(format!(
                    "{}\n",
                    knet::LegacyTableHandle::registered_names().join("\n")
                ))
            },
        ),
    );
    root.add("net", SimpleDir::new_maker(fs, Arc::new(net)));
}

unittest::path_mod_test!(tests, "../../ut-tests/net_nodes_root.rs");
