// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-namespace projection files: `uid_map`, `gid_map`, and `setgroups`.

use alloc::{borrow::Cow, format, string::String, sync::Arc, vec::Vec};

use kcred::{CAP_SYS_ADMIN, IdMapExtent, UserNamespace};
use kprocess::AsThread;
use ksync::Mutex;
use ktask::WeakKtaskRef;
use kvfs::{SimpleFileOps, VfsError, VfsFile, VfsResult};
use memaddr::PAGE_SIZE_4K;

use super::root::upgrade_task;

/// Which ID map a [`IdMapFile`] projects.
#[derive(Clone, Copy)]
pub(super) enum IdMapKind {
    Uid,
    Gid,
}

impl IdMapKind {
    fn map(self, ns: &UserNamespace) -> Vec<IdMapExtent> {
        match self {
            Self::Uid => ns.uid_map(),
            Self::Gid => ns.gid_map(),
        }
    }

    fn is_written(self, ns: &UserNamespace) -> bool {
        match self {
            Self::Uid => ns.has_uid_map(),
            Self::Gid => ns.has_gid_map(),
        }
    }

    fn write(
        self,
        ns: &UserNamespace,
        writer: &kcred::Cred,
        current: &kcred::Cred,
        extents: Vec<IdMapExtent>,
    ) -> VfsResult<()> {
        match self {
            Self::Uid => ns.write_uid_map(writer, current, extents),
            Self::Gid => ns.write_gid_map(writer, current, extents),
        }
    }
}

/// `/proc/<pid>/{uid_map,gid_map}`.
#[derive(Default)]
struct PinnedIdMap {
    target: Option<Arc<UserNamespace>>,
    reader: Option<Arc<UserNamespace>>,
}

pub(super) struct IdMapFile {
    task: WeakKtaskRef,
    kind: IdMapKind,
    pinned: Mutex<PinnedIdMap>,
}

impl IdMapFile {
    pub(super) fn new(task: WeakKtaskRef, kind: IdMapKind) -> Self {
        Self {
            task,
            kind,
            pinned: Mutex::new(PinnedIdMap::default()),
        }
    }

    fn resolve_target(&self) -> VfsResult<Arc<UserNamespace>> {
        let task = upgrade_task(&self.task)?;
        Ok(task.as_thread().real_cred().user_ns_arc().clone())
    }

    fn pinned_target(&self) -> Option<Arc<UserNamespace>> {
        self.pinned.lock().target.clone()
    }

    fn target_ns(&self) -> VfsResult<Arc<UserNamespace>> {
        match self.pinned_target() {
            Some(ns) => Ok(ns),
            None => self.resolve_target(),
        }
    }
}

/// Renders one `uid_map`/`gid_map` view.
///
/// `outside` extents are stored in the parent namespace's coordinates. Linux
/// `uid_m_show`/`gid_m_show` normally render them in the reader's namespace,
/// but a reader inside the target namespace substitutes the parent namespace
/// (`if lower_ns == ns && lower_ns->parent`), so it sees parent coordinates
/// rather than the target's own inside IDs.
fn render_id_map(
    kind: IdMapKind,
    ns: &Arc<UserNamespace>,
    reader_ns: &Arc<UserNamespace>,
) -> String {
    let mut text = String::new();
    if ns.parent().is_none() {
        // The initial namespace has an implicit identity map. The inside
        // coordinate is 0 in the initial namespace; render the outside value
        // in the reader's namespace, matching `uid_m_show`'s projection.
        let outside = match kind {
            IdMapKind::Uid => reader_ns.map_uid_from_initial(0),
            IdMapKind::Gid => reader_ns.map_gid_from_initial(0),
        }
        .unwrap_or(u32::MAX);
        text.push_str(&format!("{:10} {:10} {:10}\n", 0, outside, u32::MAX));
        return text;
    }
    let Some(parent) = ns.parent() else {
        return text;
    };
    let lower_ns = if Arc::ptr_eq(reader_ns, ns) {
        parent
    } else {
        reader_ns
    };
    for extent in kind.map(ns) {
        let global = match kind {
            IdMapKind::Uid => parent.map_uid_to_initial(extent.outside()),
            IdMapKind::Gid => parent.map_gid_to_initial(extent.outside()),
        };
        let outside = global
            .and_then(|id| match kind {
                IdMapKind::Uid => lower_ns.map_uid_from_initial(id),
                IdMapKind::Gid => lower_ns.map_gid_from_initial(id),
            })
            .unwrap_or(u32::MAX);
        text.push_str(&format!(
            "{:10} {:10} {:10}\n",
            extent.inside(),
            outside,
            extent.count()
        ));
    }
    text
}

impl SimpleFileOps for IdMapFile {
    fn validate_open(&self) -> VfsResult<()> {
        // Linux pins the target and reader user namespaces in the open file
        // description; later switches must not retarget either side.
        let target = self.resolve_target()?;
        let reader = kprocess::current_cred().user_ns_arc().clone();
        let mut pinned = self.pinned.lock();
        pinned.target = Some(target);
        pinned.reader = Some(reader);
        Ok(())
    }

    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let (target, reader) = {
            let pinned = self.pinned.lock();
            (pinned.target.clone(), pinned.reader.clone())
        };
        let target = match target {
            Some(target) => target,
            None => self.resolve_target()?,
        };
        let reader = reader.unwrap_or_else(|| kprocess::current_cred().user_ns_arc().clone());
        Ok(Cow::Owned(
            render_id_map(self.kind, &target, &reader).into_bytes(),
        ))
    }

    fn write(&self, file: &VfsFile, data: &[u8], offset: u64) -> VfsResult<usize> {
        let target = self.target_ns()?;
        let writer = file.cred().clone();
        let current = kprocess::current_cred();

        let Some(parent) = target.parent() else {
            return Err(VfsError::OperationNotPermitted);
        };
        if !core::ptr::eq(writer.user_ns(), &*target) && !core::ptr::eq(writer.user_ns(), &**parent)
        {
            return Err(VfsError::OperationNotPermitted);
        }
        if offset != 0 || data.len() >= PAGE_SIZE_4K {
            return Err(VfsError::InvalidInput);
        }
        if self.kind.is_written(&target) {
            return Err(VfsError::OperationNotPermitted);
        }
        if !writer.has_cap_in(&target, CAP_SYS_ADMIN) {
            return Err(VfsError::OperationNotPermitted);
        }

        let extents = parse_id_map(data)?;
        self.kind.write(&target, &writer, &current, extents)?;
        Ok(data.len())
    }
}

/// `/proc/<pid>/setgroups`.
pub(super) struct SetgroupsFile {
    task: WeakKtaskRef,
    pinned_ns: Mutex<Option<Arc<UserNamespace>>>,
}

impl SetgroupsFile {
    pub(super) fn new(task: WeakKtaskRef) -> Self {
        Self {
            task,
            pinned_ns: Mutex::new(None),
        }
    }

    fn resolve_target(&self) -> VfsResult<Arc<UserNamespace>> {
        let task = upgrade_task(&self.task)?;
        Ok(task.as_thread().real_cred().user_ns_arc().clone())
    }

    fn target_ns(&self) -> VfsResult<Arc<UserNamespace>> {
        if let Some(ns) = self.pinned_ns.lock().clone() {
            return Ok(ns);
        }
        self.resolve_target()
    }
}

impl SimpleFileOps for SetgroupsFile {
    fn validate_open(&self) -> VfsResult<()> {
        let ns = self.resolve_target()?;
        *self.pinned_ns.lock() = Some(ns);
        Ok(())
    }

    fn validate_open_for_write(&self) -> VfsResult<()> {
        // Linux `proc_setgroups_open()` requires CAP_SYS_ADMIN in the target
        // namespace at open-for-write; the write callback performs no
        // capability check. CAP_SETGID is not sufficient.
        let ns = self.resolve_target()?;
        *self.pinned_ns.lock() = Some(ns.clone());
        if kprocess::current_cred().has_cap_in(&ns, CAP_SYS_ADMIN) {
            Ok(())
        } else {
            Err(VfsError::PermissionDenied)
        }
    }

    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let ns = self.target_ns()?;
        let text = if ns.setgroups_allowed() {
            "allow\n"
        } else {
            "deny\n"
        };
        Ok(Cow::Owned(text.as_bytes().to_vec()))
    }

    fn write(&self, _file: &VfsFile, data: &[u8], offset: u64) -> VfsResult<usize> {
        const MAX_SETGROUPS_WRITE: usize = 8;

        // The pid file resolves its task before the write callback in Linux; a
        // task that vanished after open therefore reports ESRCH even for
        // malformed input. Authority was checked at open.
        let ns = self.target_ns()?;
        if offset != 0 || data.len() >= MAX_SETGROUPS_WRITE {
            return Err(VfsError::InvalidInput);
        }
        let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
        let (allowed, rest) = if let Some(rest) = text.strip_prefix("allow") {
            (true, rest)
        } else if let Some(rest) = text.strip_prefix("deny") {
            (false, rest)
        } else {
            return Err(VfsError::InvalidInput);
        };
        if !rest.trim_ascii().is_empty() {
            return Err(VfsError::InvalidInput);
        }

        ns.set_setgroups_allowed(allowed)?;
        Ok(data.len())
    }
}

fn parse_id_map(data: &[u8]) -> VfsResult<Vec<IdMapExtent>> {
    let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
    let mut extents = Vec::new();
    for line in text.split_terminator('\n') {
        let mut fields = line.split_ascii_whitespace();
        let inside = parse_id(fields.next())?;
        let outside = parse_id(fields.next())?;
        let count = parse_id(fields.next())?;
        if fields.next().is_some() {
            return Err(VfsError::InvalidInput);
        }
        // Linux rejects `(u32)-1` as a starting ID; it is the invalid-ID
        // sentinel rather than a mappable value. The count may still be
        // `u32::MAX`, which maps the whole 0..-2 range.
        if inside == u32::MAX || outside == u32::MAX {
            return Err(VfsError::InvalidInput);
        }
        extents.push(IdMapExtent::new(inside, outside, count)?);
    }
    Ok(extents)
}

fn parse_id(field: Option<&str>) -> VfsResult<u32> {
    let field = field.ok_or(VfsError::InvalidInput)?;
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(VfsError::InvalidInput);
    }
    field.parse::<u32>().map_err(|_| VfsError::InvalidInput)
}

unittest::path_mod_test!(tests_userns, "../../ut-tests/task_nodes_userns.rs");
