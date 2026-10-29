// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup-v2 device controller: programs attached to a cgroup decide device
//! access.
//!
//! A `BPF_PROG_TYPE_CGROUP_DEVICE` program receives a
//! `bpf_cgroup_dev_ctx { access_type, major, minor }` and returns non-zero to
//! allow the access. Programs attach to a cgroup object — not to an open file
//! description — so every holder of the same cgroup sees one policy, the
//! policy outlives the attaching descriptor, and programs on ancestors apply
//! to descendants.
//!
//! Attachment and enforcement live with the [`BpfProgram`] object layer; the
//! device-open/mknod paths reach it through the
//! [`kvfs::DeviceAccessPolicy`](kvfs::DeviceAccessPolicy) hook that
//! [`install_policy`] registers during bpffs initialization.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};

use kbpf::{
    ReadOnlyMemory, Vm,
    uapi::{bpf_devcg_acc, bpf_devcg_dev, defines},
    vm::InitialRegs,
};
use kcgroup::Cgroup;
use kerrno::{KError, KResult};
use ksync::{Mutex, static_lock};
use kvfs::{DeviceAccess, DeviceAccessPolicy, VfsError, VfsFile, VfsInode, VfsResult};

use crate::BpfProgram;

// Every uapi value below comes from `kbpf::uapi` — the single generated
// table (`core/kbpf/src/uapi.rs`, generated from Linux's bpf.h and pinned by
// `cargo test -p kbpf --test uapi_conformance`). The shifts that compose
// `bpf_cgroup_dev_ctx::access_type` stay here because they are cgroup.c's
// encoding rule, not a uapi constant.

/// `BPF_DEVCG_DEV_BLOCK`: the device-type half of `access_type`.
pub const DEVICE_TYPE_BLOCK: u32 = bpf_devcg_dev::BPF_DEVCG_DEV_BLOCK;
/// `BPF_DEVCG_DEV_CHAR`.
pub const DEVICE_TYPE_CHAR: u32 = bpf_devcg_dev::BPF_DEVCG_DEV_CHAR;
/// `BPF_DEVCG_ACC_MKNOD`, shifted into the access half: `kernel/bpf/cgroup.c`
/// encodes `access_type = (access << 16) | dev_type`.
pub const DEVICE_ACC_MKNOD: u32 = bpf_devcg_acc::BPF_DEVCG_ACC_MKNOD << 16;
/// `BPF_DEVCG_ACC_READ`, shifted into the access half.
pub const DEVICE_ACC_READ: u32 = bpf_devcg_acc::BPF_DEVCG_ACC_READ << 16;
/// `BPF_DEVCG_ACC_WRITE`, shifted into the access half.
pub const DEVICE_ACC_WRITE: u32 = bpf_devcg_acc::BPF_DEVCG_ACC_WRITE << 16;

/// Linux `BPF_CGROUP_MAX_PROGS`: how many programs one cgroup may carry
/// (direct attachments and links together).
const MAX_CGROUP_PROGS: usize = defines::BPF_CGROUP_MAX_PROGS as usize;

/// The attachment modes of a cgroup's device programs — Linux
/// `cgrp->bpf.flags[atype]`: `0` (NONE, children may not attach),
/// `BPF_F_ALLOW_OVERRIDE` (children may attach and shadow), and
/// `BPF_F_ALLOW_MULTI` (children attach in addition).
pub const ATTACH_NONE: u32 = 0;
/// `BPF_F_ALLOW_OVERRIDE`.
pub const ATTACH_ALLOW_OVERRIDE: u32 = defines::BPF_F_ALLOW_OVERRIDE;
/// `BPF_F_ALLOW_MULTI`.
pub const ATTACH_ALLOW_MULTI: u32 = defines::BPF_F_ALLOW_MULTI;

/// Monotonically increasing identity for link attachments. Two links of the
/// same program onto the same cgroup are two independent attachments (Linux
/// keys `bpf_prog_list` entries by the link object), and each link's release
/// must remove exactly its own entry.
static LINK_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// An entry of a cgroup's device program list.
struct AttachedDeviceProgram {
    /// The program object. Owning it keeps the id registry entry alive while
    /// the attachment exists: Linux refcounts `bpf_prog` from the attachment,
    /// so `BPF_PROG_GET_FD_BY_ID`/`GET_NEXT_ID` still see a program whose
    /// descriptors user space has all closed.
    program: Arc<BpfProgram>,
    /// `Some(id)` for a link attachment: the entry dies with the link file
    /// that owns this id, never through another link's release or a
    /// `prog_id`-matching detach.
    link: Option<u64>,
}

impl AttachedDeviceProgram {
    fn prog_id(&self) -> u32 {
        self.program.id()
    }

    fn insns(&self) -> Arc<[u8]> {
        self.program.insns_shared()
    }
}

/// One cgroup's attached device programs.
struct CgroupDeviceState {
    cgroup: Weak<Cgroup>,
    /// The recorded attachment mode (`cgrp->bpf.flags[atype]`, one of
    /// `ATTACH_NONE`/`ATTACH_ALLOW_OVERRIDE`/`ATTACH_ALLOW_MULTI`): further
    /// attaches must request the same mode (`EPERM` otherwise), and the mode
    /// resets when the last attachment goes away.
    flags: u32,
    progs: Vec<AttachedDeviceProgram>,
}

static_lock! {
/// Device programs by cgroup object, keyed by that object's address with a
/// weak back-reference. A removed cgroup never yields a new object at the
/// same identity (`kcgroup` allocates namespaces and children from monotonic
/// state and frees removed objects), and entries whose cgroup is gone are
/// pruned on access, so the state follows the cgroup's own lifetime rather
/// than any descriptor's.
static DEVICE_PROGRAMS: Mutex<BTreeMap<usize, CgroupDeviceState>> =
    Mutex::new(BTreeMap::new());
}

/// A `bpf(2)` link that keeps a device program attached to its cgroup.
///
/// The link is an anonymous-inode fd whose [`kvfs::FileOperations::release`]
/// runs when the last descriptor referencing the link is closed — the
/// lifetime Linux models with `bpf_cgroup_link` and
/// `bpf_cgroup_link_release`. The id identifies exactly one attachment entry,
/// so two links of the same program detach independently.
struct BpfCgroupLink {
    cgroup: Weak<Cgroup>,
    link_id: u64,
}

impl BpfCgroupLink {
    fn into_file(self: Arc<Self>, cred: Arc<kcred::Cred>) -> VfsResult<Arc<VfsFile>> {
        let fops: Arc<dyn kvfs::FileOperations> = self.clone();
        anon_inodefs::AnonInodeFs::global().get_file(
            "bpf-link",
            fops,
            self,
            kvfs::FMode::READ | kvfs::FMode::WRITE,
            kvfs::OpenFlags::empty(),
            cred,
        )
    }

    fn detach(&self) {
        detach_link(&self.cgroup, self.link_id);
    }
}

impl kvfs::FileOperations for BpfCgroupLink {
    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> VfsResult<()> {
        self.detach();
        Ok(())
    }
}

/// The kvfs policy hook: consults the current task's cgroup and its
/// ancestors, and answers `EPERM` when an effective device program denies the
/// access.
struct CgroupDevicePolicy;

impl DeviceAccessPolicy for CgroupDevicePolicy {
    fn check_device_access(&self, device: kvfs::DeviceId, access: DeviceAccess) -> VfsResult<()> {
        // Every set kind is reported to the programs: Linux reports an
        // O_RDWR open with READ and WRITE set together, not one or the other.
        let mut access_bits = 0;
        if access.contains(DeviceAccess::MKNOD) {
            access_bits |= DEVICE_ACC_MKNOD;
        }
        if access.contains(DeviceAccess::READ) {
            access_bits |= DEVICE_ACC_READ;
        }
        if access.contains(DeviceAccess::WRITE) {
            access_bits |= DEVICE_ACC_WRITE;
        }
        // Kernel tasks (and any task that never joined a cgroup) have no
        // device policy: `devices` only gates accesses from member tasks.
        match kprocess::current_cgroup() {
            Some(cgroup)
                if !device_allowed(
                    &cgroup,
                    access_bits,
                    device.major(),
                    device.minor(),
                    type_bits_of(access),
                ) =>
            {
                Err(VfsError::OperationNotPermitted)
            }
            _ => Ok(()),
        }
    }
}

fn type_bits_of(access: DeviceAccess) -> u32 {
    if access.contains(DeviceAccess::TYPE_CHAR) {
        DEVICE_TYPE_CHAR
    } else {
        DEVICE_TYPE_BLOCK
    }
}

/// Installs the device-policy hook consulted by every device open and
/// device-node creation. Called once from bpffs initialization; before that,
/// device access is unrestricted.
pub fn install_policy() {
    let _ = kvfs::register_device_access_policy(Some(Arc::new(CgroupDevicePolicy)));
}

/// Returns whether `access` to `(major, minor)` of `dev_type` is allowed for
/// `cgroup` and, through cgroup inheritance, for every ancestor of it.
///
/// With no attached program at any level the access is allowed — the
/// `devices` controller's "no policy" state.
pub fn device_allowed(
    cgroup: &Arc<Cgroup>,
    access: u32,
    major: u32,
    minor: u32,
    dev_type: u32,
) -> bool {
    let programs = effective_programs(cgroup);
    if programs.is_empty() {
        return true;
    }
    let access = access | dev_type;
    programs
        .iter()
        .all(|insns| run_device_program(insns, access, major, minor, dev_type))
}

/// Collects the effective device programs of `cgroup`: its own attachments
/// plus every ancestor's, nearest first.
fn effective_programs(cgroup: &Arc<Cgroup>) -> Vec<Arc<[u8]>> {
    // `compute_effective_progs()`: every level contributes until programs
    // have been contributed, and after that only MULTI levels keep
    // contributing — a single-attached level shadows its ancestors.
    let attachments = DEVICE_PROGRAMS.lock();
    let mut programs = Vec::new();
    let mut contributed = false;
    let mut current = Some(cgroup.clone());
    while let Some(node) = current {
        if let Some(state) = attachments
            .get(&(Arc::as_ptr(&node) as usize))
            .filter(|state| state.cgroup.strong_count() > 0)
            && (!contributed || state.flags == ATTACH_ALLOW_MULTI)
        {
            programs.extend(state.progs.iter().map(|p| p.insns()));
            contributed |= !state.progs.is_empty();
        }
        current = node.parent();
    }
    programs
}

/// Runs one device program against the access context.
///
/// A program returns non-zero to allow; interpreter errors (malformed
/// programs) deny, mirroring the verifier's refusal to run programs it cannot
/// prove safe.
fn run_device_program(
    insns: &Arc<[u8]>,
    access: u32,
    major: u32,
    minor: u32,
    dev_type: u32,
) -> bool {
    device_program_verdict(insns, access, major, minor, dev_type)
        .map(|retval| retval != 0)
        .unwrap_or(false)
}

/// Executes one device program; `Err` means the interpreter rejected it.
pub(crate) fn device_program_verdict(
    insns: &Arc<[u8]>,
    access: u32,
    major: u32,
    minor: u32,
    dev_type: u32,
) -> Result<u64, kbpf::Error> {
    let access = access | dev_type;
    let mut ctx = [0u8; 12];
    ctx[0..4].copy_from_slice(&access.to_le_bytes());
    ctx[4..8].copy_from_slice(&major.to_le_bytes());
    ctx[8..12].copy_from_slice(&minor.to_le_bytes());
    let (ctx_memory, ctx_base) = ReadOnlyMemory::context(Arc::new(ctx));
    let mut vm = Vm::new();
    vm.add_read_only_memory(ctx_memory);
    vm.run_with_initial_regs(
        insns,
        InitialRegs {
            r1: ctx_base,
            ..InitialRegs::default()
        },
    )
}

/// Linux's `hierarchy_allows_attach()`: a cgroup may attach only when no
/// ancestor forbids it — a MULTI ancestor always allows, an ancestor carrying
/// programs allows only under OVERRIDE, program-free ancestors are skipped.
fn hierarchy_allows_attach(
    attachments: &BTreeMap<usize, CgroupDeviceState>,
    target: &Arc<Cgroup>,
) -> bool {
    let mut current = target.parent();
    while let Some(node) = current {
        if let Some(state) = attachments
            .get(&(Arc::as_ptr(&node) as usize))
            .filter(|state| state.cgroup.strong_count() > 0)
        {
            if state.flags == ATTACH_ALLOW_MULTI {
                return true;
            }
            if !state.progs.is_empty() {
                return state.flags == ATTACH_ALLOW_OVERRIDE;
            }
        }
        current = node.parent();
    }
    true
}

/// Attaches a loaded device program to `target` with `flags`
/// (`ATTACH_NONE`/`ATTACH_ALLOW_OVERRIDE`/`ATTACH_ALLOW_MULTI`).
///
/// The recorded mode is sticky while the cgroup carries programs: a request
/// whose mode differs is `EPERM` (`__cgroup_bpf_attach`'s per-entry flags
/// check), and the mode resets when the last attachment goes away. A parent
/// attachment can forbid the attach outright (`hierarchy_allows_attach`).
/// `ATTACH_NONE`/`ATTACH_ALLOW_OVERRIDE` replace the target's programs (the
/// single-attach case); `ATTACH_ALLOW_MULTI` appends, and re-attaching an
/// already-attached program is `EINVAL` like Linux's `find_attach_entry`.
///
/// # Errors
///
/// [`KError::ArgumentListTooLong`] at the cap; [`KError::OperationNotPermitted`]
/// on a mode mismatch or a forbidding ancestor; [`KError::InvalidInput`] for a
/// multi re-attach of a directly attached program.
pub fn attach(target: &Arc<Cgroup>, program: &Arc<BpfProgram>, flags: u32) -> KResult<()> {
    let mut devices = DEVICE_PROGRAMS.lock();
    prune_dead_cgroups(&mut devices);
    if !hierarchy_allows_attach(&devices, target) {
        return Err(KError::OperationNotPermitted);
    }
    let key = target_key(target);
    if let Some(state) = devices.get(&key) {
        // Linux's `__cgroup_bpf_attach` validates the flags before the
        // count, so a mode-mismatched attach onto a full cgroup answers
        // `EPERM`, not `E2BIG`.
        if state.flags != flags {
            return Err(KError::OperationNotPermitted);
        }
        if state.progs.len() >= MAX_CGROUP_PROGS {
            return Err(KError::ArgumentListTooLong);
        }
        // Linux's multi duplicate check matches direct attachments only
        // (`pl->prog == prog && pl->link == NULL`): the same program may
        // additionally arrive through links.
        if flags == ATTACH_ALLOW_MULTI
            && state
                .progs
                .iter()
                .any(|recorded| recorded.link.is_none() && recorded.prog_id() == program.id())
        {
            return Err(KError::InvalidInput);
        }
    }
    let state = devices.entry(key).or_insert_with(|| CgroupDeviceState {
        cgroup: Arc::downgrade(target),
        flags: ATTACH_NONE,
        progs: Vec::new(),
    });
    if flags != ATTACH_ALLOW_MULTI {
        state.progs.clear();
    }
    state.flags = flags;
    state.progs.push(AttachedDeviceProgram {
        program: program.clone(),
        link: None,
    });
    Ok(())
}

/// Attaches `program` to `target` through a link entry whose lifetime the
/// returned link file owns, and returns the link identity that release uses
/// to remove exactly this entry.
///
/// `BPF_LINK_CREATE` is multi-attachment on Linux: `cgroup_bpf_link_attach()`
/// passes `BPF_F_ALLOW_MULTI | flags`, so the program is appended, never
/// replaces, and a cgroup carrying single-mode programs answers `EPERM`.
///
/// # Errors
///
/// [`KError::ArgumentListTooLong`] at the cap; [`KError::OperationNotPermitted`]
/// on a mode mismatch or a forbidding ancestor.
pub fn attach_link(target: &Arc<Cgroup>, program: &Arc<BpfProgram>) -> KResult<u64> {
    let mut devices = DEVICE_PROGRAMS.lock();
    prune_dead_cgroups(&mut devices);
    if !hierarchy_allows_attach(&devices, target) {
        return Err(KError::OperationNotPermitted);
    }
    let state = devices
        .entry(target_key(target))
        .or_insert_with(|| CgroupDeviceState {
            cgroup: Arc::downgrade(target),
            flags: ATTACH_NONE,
            progs: Vec::new(),
        });
    // Mode first, like `__cgroup_bpf_attach`: a full single-mode cgroup
    // answers `EPERM` for a link attach, not `E2BIG`.
    if state.flags != ATTACH_ALLOW_MULTI && !state.progs.is_empty() {
        return Err(KError::OperationNotPermitted);
    }
    if state.progs.len() >= MAX_CGROUP_PROGS {
        return Err(KError::ArgumentListTooLong);
    }
    state.flags = ATTACH_ALLOW_MULTI;
    let link_id = LINK_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    state.progs.push(AttachedDeviceProgram {
        program: program.clone(),
        link: Some(link_id),
    });
    Ok(link_id)
}

/// Detaches `prog_id` from `target` — `BPF_PROG_DETACH` with a program fd.
///
/// On a multi-attached cgroup only a direct attachment of that program
/// matches (`find_detach_entry` matches `pl->link == NULL`); link-owned
/// entries are reachable only through their link. On a single-attached
/// cgroup the one attachment is removed whatever kind it is (Linux's
/// legacy NONE/OVERRIDE detach takes the first entry, even a link's).
///
/// # Errors
///
/// [`KError::NotFound`] when nothing matches.
pub fn detach(target: &Arc<Cgroup>, prog_id: u32) -> KResult<()> {
    let mut devices = DEVICE_PROGRAMS.lock();
    prune_dead_cgroups(&mut devices);
    let key = target_key(target);
    let Some(state) = devices.get_mut(&key) else {
        return Err(KError::NotFound);
    };
    let recorded = state.progs.len();
    if state.flags == ATTACH_ALLOW_MULTI {
        state
            .progs
            .retain(|attached| attached.link.is_some() || attached.prog_id() != prog_id);
    } else if !state.progs.is_empty() {
        let _ = state.progs.remove(0);
    }
    if state.progs.len() == recorded {
        return Err(KError::NotFound);
    }
    if state.progs.is_empty() {
        devices.remove(&key);
    }
    Ok(())
}

/// Detaches on `BPF_PROG_DETACH` whose program fd does not resolve —
/// Linux's NULL-program detach. On a multi-attached cgroup that request is
/// `EINVAL` (`find_detach_entry`: "the user has to specify valid FD"); a
/// single-attached cgroup loses its one attachment whatever kind it is.
///
/// # Errors
///
/// [`KError::InvalidInput`] in multi mode; [`KError::NotFound`] when the
/// target carries no attachment.
pub fn detach_legacy(target: &Arc<Cgroup>) -> KResult<()> {
    let mut devices = DEVICE_PROGRAMS.lock();
    prune_dead_cgroups(&mut devices);
    let key = target_key(target);
    let Some(state) = devices.get_mut(&key) else {
        return Err(KError::NotFound);
    };
    if state.flags == ATTACH_ALLOW_MULTI {
        return Err(KError::InvalidInput);
    }
    devices.remove(&key);
    Ok(())
}

/// Removes the attachment owned by the link with `link_id` — the release
/// path of a link file. Other links of the same program, and direct
/// attachments, are untouched.
fn detach_link(cgroup: &Weak<Cgroup>, link_id: u64) {
    let mut devices = DEVICE_PROGRAMS.lock();
    prune_dead_cgroups(&mut devices);
    let Some(cgroup) = cgroup.upgrade() else {
        return;
    };
    let key = Arc::as_ptr(&cgroup) as usize;
    if let Some(state) = devices.get_mut(&key) {
        state
            .progs
            .retain(|attached| attached.link != Some(link_id));
        if state.progs.is_empty() {
            devices.remove(&key);
        }
    }
}

/// Returns the ids of the effective device programs of `target`: its own
/// attachments plus every ancestor's, nearest first
/// (`BPF_PROG_QUERY` with `BPF_F_QUERY_EFFECTIVE`).
pub fn effective_program_ids(target: &Arc<Cgroup>) -> Vec<u32> {
    // The query must answer the post-shadowing set enforcement runs
    // (`compute_effective_progs()`): after a level has contributed
    // programs, only MULTI levels keep contributing.
    let attachments = DEVICE_PROGRAMS.lock();
    let mut ids = Vec::new();
    let mut contributed = false;
    let mut current = Some(target.clone());
    while let Some(node) = current {
        if let Some(state) = attachments
            .get(&(Arc::as_ptr(&node) as usize))
            .filter(|state| state.cgroup.strong_count() > 0)
            && (!contributed || state.flags == ATTACH_ALLOW_MULTI)
        {
            ids.extend(state.progs.iter().map(|p| p.prog_id()));
            contributed |= !state.progs.is_empty();
        }
        current = node.parent();
    }
    ids
}

/// Mints the link fd for a freshly attached link; the returned file's release
/// detaches the program when the last reference dies.
///
/// The attachment is created here; if the link file cannot be minted, the
/// dropped link releases its own attachment by identity, so a failed
/// `BPF_LINK_CREATE` never leaves a policy behind.
pub fn link_file(
    target: &Arc<Cgroup>,
    program: &Arc<BpfProgram>,
    cred: Arc<kcred::Cred>,
) -> VfsResult<Arc<VfsFile>> {
    let link_id = attach_link(target, program)?;
    let link = BpfCgroupLink {
        cgroup: Arc::downgrade(target),
        link_id,
    };
    Arc::new(link).into_file(cred)
}

/// Returns the ids of the programs directly attached to `target`
/// (`BPF_PROG_QUERY` without the effective flag).
pub fn attached_program_ids(target: &Arc<Cgroup>) -> Vec<u32> {
    let mut devices = DEVICE_PROGRAMS.lock();
    prune_dead_cgroups(&mut devices);
    devices
        .get(&target_key(target))
        .map(|state| state.progs.iter().map(|p| p.prog_id()).collect())
        .unwrap_or_default()
}

/// Answers `BPF_PROG_QUERY` in one controller-lock acquisition: the
/// effective (post-shadowing) or direct program ids plus the recorded
/// attachment mode (0 for an effective query, mirroring
/// `__cgroup_bpf_query`).
pub fn query(target: &Arc<Cgroup>, effective: bool) -> (Vec<u32>, u32) {
    let attachments = DEVICE_PROGRAMS.lock();
    if effective {
        let mut ids = Vec::new();
        let mut contributed = false;
        let mut current = Some(target.clone());
        while let Some(node) = current {
            if let Some(state) = attachments
                .get(&(Arc::as_ptr(&node) as usize))
                .filter(|state| state.cgroup.strong_count() > 0)
                && (!contributed || state.flags == ATTACH_ALLOW_MULTI)
            {
                ids.extend(state.progs.iter().map(|p| p.prog_id()));
                contributed |= !state.progs.is_empty();
            }
            current = node.parent();
        }
        (ids, 0)
    } else {
        let Some(state) = attachments.get(&target_key(target)) else {
            return (Vec::new(), ATTACH_NONE);
        };
        (
            state.progs.iter().map(|p| p.prog_id()).collect(),
            state.flags,
        )
    }
}

/// Returns the recorded attachment mode of `target`
/// (`ATTACH_NONE`/`ATTACH_ALLOW_OVERRIDE`/`ATTACH_ALLOW_MULTI`) — what
/// `BPF_PROG_QUERY` without the effective flag reports in `attach_flags`.
pub fn attachment_flags(target: &Arc<Cgroup>) -> u32 {
    let mut devices = DEVICE_PROGRAMS.lock();
    prune_dead_cgroups(&mut devices);
    devices
        .get(&target_key(target))
        .map(|state| state.flags)
        .unwrap_or(ATTACH_NONE)
}

fn target_key(target: &Arc<Cgroup>) -> usize {
    Arc::as_ptr(target) as usize
}

/// Drops entries whose cgroup object is gone — the controller state follows
/// the cgroup's own lifetime, not any descriptor's.
fn prune_dead_cgroups(devices: &mut BTreeMap<usize, CgroupDeviceState>) {
    devices.retain(|_, state| state.cgroup.strong_count() > 0);
}
