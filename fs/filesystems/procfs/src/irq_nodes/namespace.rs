// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-superblock stable IRQ namespace.
//!
//! Namespace records own the stable directory/file inodes and a weak KIRQ
//! retirement subscription. The KIRQ handle remains the only Line identity;
//! inodes never own the record strongly, avoiding a record -> inode -> record
//! cycle.

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
};
use core::fmt::Write;

use kerrno::LinuxError;
use kirq::{IrqLineHandle, IrqLineRetirementObserver, lookup_irq_line, next_irq_line};
use ksync::Mutex;
use kvfs::{
    DirMapping, NodePermission, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFs,
    SparseDirSource, VfsError, VfsInode, VfsResult, new_sparse_dir_inode,
};

use super::{
    error,
    file::{AFFINITY_FILE_COUNT, AffinityFileKind, new_affinity_file_inode},
};

const DIRECTORY_MODE: NodePermission = NodePermission::from_bits_truncate(0o555);

/// Per-superblock record owner. Strong records keep inode metadata alive after
/// all open files/dentries are closed.
pub(crate) struct IrqNamespace {
    fs: Arc<SimpleFs>,
    records: Mutex<BTreeMap<usize, Arc<LineRecord>>>,
}

struct LineRecord {
    handle: Arc<IrqLineHandle>,
    dir: Arc<VfsInode>,
    _files: [Arc<VfsInode>; AFFINITY_FILE_COUNT],
    _observer: Arc<dyn IrqLineRetirementObserver>,
    _subscription: kirq::IrqLineRetirementSubscription,
}

struct LineRetireObserver {
    namespace: Weak<IrqNamespace>,
    virq: usize,
    handle: Weak<IrqLineHandle>,
}

impl IrqLineRetirementObserver for LineRetireObserver {
    fn on_line_retired(&self, _virq: usize) {
        let Some(namespace) = self.namespace.upgrade() else {
            return;
        };
        let Some(handle) = self.handle.upgrade() else {
            return;
        };
        let removed = {
            let mut records = namespace.records.lock();
            match records.get(&self.virq) {
                Some(record) if record.handle.is_same_line(&handle) => records.remove(&self.virq),
                _ => None,
            }
        };
        drop(removed);
    }
}

impl IrqNamespace {
    fn new(fs: Arc<SimpleFs>) -> Self {
        Self {
            fs,
            records: Mutex::new(BTreeMap::new()),
        }
    }

    fn ensure_record(self: &Arc<Self>, virq: usize) -> VfsResult<Arc<LineRecord>> {
        // A bounded retry handles a retirement/replacement that wins between
        // candidate construction and the authoritative commit. The commit
        // section never destroys an unadopted record while holding the map lock.
        for _ in 0..3 {
            let (existing, handle) = {
                let records = self.records.lock();
                let Some(handle) = lookup_irq_line(virq).map_err(error::map_management)? else {
                    return Err(VfsError::NotFound);
                };
                if !handle.has_action_history() || !handle.is_current() {
                    return Err(VfsError::NotFound);
                }
                let existing = records
                    .get(&virq)
                    .filter(|record| record.handle.is_same_line(&handle))
                    .filter(|_| handle.is_current())
                    .cloned();
                (existing, handle)
            };
            if let Some(existing) = existing {
                return Ok(existing);
            }

            let candidate = self.build_record(virq, Arc::new(handle))?;
            match self.commit_candidate(virq, candidate) {
                Ok(record) => return Ok(record),
                Err(VfsError::NotFound) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(VfsError::NotFound)
    }

    /// Commits one candidate while holding the namespace lock across the
    /// authoritative KIRQ identity check and the map mutation.
    fn commit_candidate(
        self: &Arc<Self>,
        virq: usize,
        candidate: Arc<LineRecord>,
    ) -> VfsResult<Arc<LineRecord>> {
        let (selected, stale, removed) = {
            let mut records = self.records.lock();
            let Some(current) = lookup_irq_line(virq).map_err(error::map_management)? else {
                return Err(VfsError::NotFound);
            };
            if !current.has_action_history() || !current.is_current() {
                return Err(VfsError::NotFound);
            }

            if let Some(existing) = records.get(&virq)
                && existing.handle.is_same_line(&current)
                && current.is_current()
            {
                return Ok(existing.clone());
            }

            if !candidate.handle.is_same_line(&current) || !current.is_current() {
                return Err(VfsError::NotFound);
            }

            let stale = records.insert(virq, candidate.clone());
            // Recheck after publication as well: retirement can race the
            // authoritative lookup while this namespace lock is held.
            let still_current = lookup_irq_line(virq).ok().flatten().is_some_and(|now| {
                candidate.handle.is_same_line(&now) && candidate.handle.is_current()
            });
            let removed = if still_current {
                None
            } else {
                records.remove(&virq)
            };
            let selected = still_current.then(|| candidate.clone());
            (selected, stale, removed)
        };
        drop(stale);
        drop(removed);
        drop(candidate);
        selected.ok_or(VfsError::NotFound)
    }

    #[cfg(unittest)]
    pub(super) fn record_count(&self) -> usize {
        self.records.lock().len()
    }

    fn build_record(
        self: &Arc<Self>,
        virq: usize,
        handle: Arc<IrqLineHandle>,
    ) -> VfsResult<Arc<LineRecord>> {
        let domain = kcpu_id_map::text::CpuTextDomain::runtime()
            .map_err(crate::cpu_text_error::map_cpu_text)?;
        let source_is_user_settable = handle
            .is_user_affinity_source_supported()
            .map_err(error::map_affinity_read)?;
        let files = [
            new_affinity_file_inode(
                self.fs.clone(),
                handle.clone(),
                AffinityFileKind::RequestedBitmap,
                source_is_user_settable,
                domain.clone(),
            )?,
            new_affinity_file_inode(
                self.fs.clone(),
                handle.clone(),
                AffinityFileKind::RequestedList,
                source_is_user_settable,
                domain.clone(),
            )?,
            new_affinity_file_inode(
                self.fs.clone(),
                handle.clone(),
                AffinityFileKind::EffectiveBitmap,
                source_is_user_settable,
                domain.clone(),
            )?,
            new_affinity_file_inode(
                self.fs.clone(),
                handle.clone(),
                AffinityFileKind::EffectiveList,
                source_is_user_settable,
                domain.clone(),
            )?,
        ];
        let mut mapping = DirMapping::new();
        for (kind, inode) in AffinityFileKind::ALL.iter().zip(files.iter()) {
            mapping.add_file(kind.name(), inode.clone());
        }
        let dir = SimpleDir::new_inode_with_owner(
            self.fs.clone(),
            Arc::new(LineDirOps {
                mapping,
                handle: handle.clone(),
            }),
            DIRECTORY_MODE,
            0,
            0,
        );
        let observer: Arc<dyn IrqLineRetirementObserver> = Arc::new(LineRetireObserver {
            namespace: Arc::downgrade(self),
            virq,
            handle: Arc::downgrade(&handle),
        });
        let subscription = handle
            .subscribe_retirement(Arc::downgrade(&observer))
            .map_err(error::map_management)?;
        Ok(Arc::new(LineRecord {
            handle,
            dir,
            _files: files,
            _observer: observer,
            _subscription: subscription,
        }))
    }
}

struct LineDirOps {
    mapping: DirMapping,
    handle: Arc<IrqLineHandle>,
}

impl SimpleDirOps for LineDirOps {
    fn validate_open(&self) -> VfsResult<()> {
        error::ensure_live(&self.handle)
    }

    fn child_names<'a>(
        &'a self,
    ) -> VfsResult<kalloc_types::KBox<dyn Iterator<Item = alloc::borrow::Cow<'a, str>> + 'a>> {
        error::ensure_live(&self.handle)?;
        <DirMapping as SimpleDirOps>::child_names(&self.mapping)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<kvfs::Dentry> {
        error::ensure_live(&self.handle)?;
        <DirMapping as SimpleDirOps>::lookup_child(&self.mapping, lookup, name)
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

struct RootSource {
    namespace: Arc<IrqNamespace>,
}

impl SparseDirSource for RootSource {
    fn first_key(&self) -> VfsResult<Option<u64>> {
        self.next_key_from(0)
    }

    fn next_key(&self, after: u64) -> VfsResult<Option<u64>> {
        let Some(start) = after
            .checked_add(1)
            .and_then(|value| usize::try_from(value).ok())
        else {
            return Ok(None);
        };
        self.next_key_from(start)
    }

    fn name_for_key(&self, key: u64) -> VfsResult<String> {
        let virq = usize::try_from(key).map_err(|_| VfsError::from(LinuxError::EOVERFLOW))?;
        virq_name(virq)
    }

    fn lookup_key(
        &self,
        lookup: SimpleDirLookup<'_>,
        key: u64,
        name: &str,
    ) -> VfsResult<kvfs::Dentry> {
        let virq = usize::try_from(key).map_err(|_| VfsError::from(LinuxError::EOVERFLOW))?;
        let record = self.namespace.ensure_record(virq)?;
        Ok(lookup.dir_from_inode(name, record.dir.clone()))
    }

    fn lookup_name(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<kvfs::Dentry> {
        let virq = parse_virq(name)?;
        let record = self.namespace.ensure_record(virq)?;
        Ok(lookup.dir_from_inode(name, record.dir.clone()))
    }
}

impl RootSource {
    fn next_key_from(&self, mut cursor: usize) -> VfsResult<Option<u64>> {
        loop {
            let Some(handle) = next_irq_line(cursor).map_err(error::map_management)? else {
                return Ok(None);
            };
            let virq = handle.virq();
            if handle.has_action_history() && handle.is_current() {
                return u64::try_from(virq)
                    .map(Some)
                    .map_err(|_| VfsError::from(LinuxError::EOVERFLOW));
            }
            let Some(next) = virq.checked_add(1) else {
                return Ok(None);
            };
            cursor = next;
        }
    }
}

pub(crate) fn new_root_inode(fs: Arc<SimpleFs>) -> Arc<VfsInode> {
    let namespace = Arc::new(IrqNamespace::new(fs.clone()));
    let namespace_for_source = namespace.clone();
    let root = Arc::new(RootSource {
        namespace: namespace_for_source,
    });
    new_sparse_dir_inode(fs, root, DIRECTORY_MODE, 0, 0)
}

fn virq_name(virq: usize) -> VfsResult<String> {
    let mut name = String::new();
    name.try_reserve(20)
        .map_err(|_| VfsError::from(LinuxError::ENOMEM))?;
    write!(&mut name, "{virq}").map_err(|_| VfsError::from(LinuxError::EIO))?;
    Ok(name)
}

fn parse_virq(name: &str) -> VfsResult<usize> {
    if name.is_empty() || (name.len() > 1 && name.starts_with('0')) {
        return Err(VfsError::NotFound);
    }
    let mut value = 0usize;
    for byte in name.bytes() {
        if !byte.is_ascii_digit() {
            return Err(VfsError::NotFound);
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add((byte - b'0') as usize))
            .ok_or(VfsError::NotFound)?;
    }
    Ok(value)
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../../ut-tests/irq_namespace.rs");
