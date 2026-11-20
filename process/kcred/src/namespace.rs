// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-namespace identity, parentage, and ID mappings.
//!
//! A user namespace owns a pair of ID maps. IDs are local to the namespace;
//! writing a map translates a contiguous local range to a contiguous range in
//! the parent namespace. The initial namespace implicitly maps every 32-bit ID
//! to itself and has no parent. Mappings are immutable once written, matching
//! Linux: the kernel never rewrites an existing map because credentials and
//! files may already carry IDs derived from it.

use alloc::vec::Vec;
use core::ptr;

use kerrno::{KError, KResult, LinuxError};
use klazy::Once;
use kspin::SpinNoPreempt;

use crate::{
    Cred, Gid, Uid,
    credentials::caps::{CAP_SETFCAP, CAP_SETGID, CAP_SETUID, CAP_SYS_ADMIN},
};

static INIT_USER_NS: Once<alloc::sync::Arc<UserNamespace>> = Once::new();

/// Maximum number of extents in one ID map, matching Linux's `UID_GID_MAP_MAX_EXTENTS`.
pub const ID_MAP_MAX_EXTENTS: usize = 340;

/// Namespace identifier allocated from a kernel-wide atomic counter.
///
/// This is used for namespace identities that are externally rendered as
/// `/proc/[pid]/ns/*` inode-style identifiers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NamespaceId(u64);

impl Default for NamespaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl NamespaceId {
    /// Allocates the next namespace ID without allocating memory.
    ///
    /// The counter starts at one and uses relaxed atomic increment. IDs are
    /// distinct until the `u64` counter wraps; exhaustion is not checked and
    /// allocation does not synchronize unrelated namespace state.
    /// An ID is not an authorization token.
    pub fn new() -> Self {
        use core::sync::atomic::{AtomicU64, Ordering};

        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw ID value.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One contiguous range in a user-namespace ID map.
///
/// `inside` is the first ID in the owning namespace, `outside` is the first ID
/// in the parent namespace, and `count` is the number of translated IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdMapExtent {
    inside: u32,
    outside: u32,
    count: u32,
}

impl IdMapExtent {
    /// Creates an extent, rejecting an empty or overflowing range.
    ///
    /// The range end is exclusive and must not exceed `2^32`; IDs themselves
    /// remain 32-bit.
    pub fn new(inside: u32, outside: u32, count: u32) -> KResult<Self> {
        if count == 0
            || u64::from(inside) + u64::from(count) >= (1u64 << 32)
            || u64::from(outside) + u64::from(count) >= (1u64 << 32)
        {
            return Err(KError::InvalidInput);
        }
        Ok(Self {
            inside,
            outside,
            count,
        })
    }

    /// Returns the first local ID.
    pub const fn inside(self) -> u32 {
        self.inside
    }

    /// Returns the first parent-namespace ID.
    pub const fn outside(self) -> u32 {
        self.outside
    }

    /// Returns the number of translated IDs.
    pub const fn count(self) -> u32 {
        self.count
    }

    fn inside_end(self) -> u64 {
        u64::from(self.inside) + u64::from(self.count)
    }

    fn outside_end(self) -> u64 {
        u64::from(self.outside) + u64::from(self.count)
    }

    fn contains_inside(self, id: u32) -> bool {
        u64::from(id) >= u64::from(self.inside) && u64::from(id) < self.inside_end()
    }

    fn contains_outside(self, id: u32) -> bool {
        u64::from(id) >= u64::from(self.outside) && u64::from(id) < self.outside_end()
    }

    fn contains_inside_range(self, first: u32, count: u32) -> bool {
        let first = u64::from(first);
        let end = first + u64::from(count);
        first >= u64::from(self.inside) && end <= self.inside_end()
    }
}

#[derive(Clone, Debug, Default)]
struct IdMap {
    extents: Vec<IdMapExtent>,
    /// Set once a writer has supplied at least one extent. An empty root map is
    /// implicit rather than "not yet ready".
    is_written: bool,
}

impl IdMap {
    fn map_inside(&self, id: u32) -> Option<u32> {
        let extent = self
            .extents
            .iter()
            .find(|extent| extent.contains_inside(id))?;
        Some((u64::from(id) - u64::from(extent.inside) + u64::from(extent.outside)) as u32)
    }

    fn map_outside(&self, id: u32) -> Option<u32> {
        let extent = self
            .extents
            .iter()
            .find(|extent| extent.contains_outside(id))?;
        Some((u64::from(id) - u64::from(extent.outside) + u64::from(extent.inside)) as u32)
    }

    /// Translates an `inside` range to its parent-namespace equivalent.
    ///
    /// Returns `None` unless one extent covers the whole range: adjacent
    /// extents are independent authorizations and may not be combined.
    fn map_inside_range(&self, first: u32, count: u32) -> Option<u32> {
        let extent = self
            .extents
            .iter()
            .find(|extent| extent.contains_inside_range(first, count))?;
        Some((u64::from(first) - u64::from(extent.inside) + u64::from(extent.outside)) as u32)
    }
}

#[derive(Debug, Default)]
struct UserNamespaceMapState {
    uid_map: IdMap,
    gid_map: IdMap,
    /// Whether `setgroups(2)` is permitted for tasks in this namespace.
    ///
    /// The initial namespace allows it, and a child inherits the flag from its
    /// parent. Writing `deny` clears it permanently; writing `allow` cannot
    /// restore it.
    setgroups_allowed: bool,
}

/// A user namespace: identity, parent link, and immutable ID mappings.
///
/// The kernel allocates a child namespace with empty maps. A writer authorized
/// against the namespace and its parent fills the maps through
/// [`Self::write_uid_map`] and [`Self::write_gid_map`]. Until a map is written,
/// IDs are not translatable across the namespace boundary; that is valid and
/// does not stop tasks from running, matching Linux `CLONE_NEWUSER`.
pub struct UserNamespace {
    id: NamespaceId,
    parent: Option<alloc::sync::Arc<UserNamespace>>,
    /// Nesting depth in the user-namespace tree (initial namespace is 0).
    level: u32,
    owner_euid: Uid,
    /// Whether the creator held `CAP_SETFCAP` in the parent when this namespace
    /// was created. Linux consults this when the creator later maps parent UID
    /// 0 into the namespace (`verify_root_map()`).
    parent_could_setfcap: bool,
    map_state: SpinNoPreempt<UserNamespaceMapState>,
}

impl core::fmt::Debug for UserNamespace {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("UserNamespace")
            .field("id", &self.id)
            .field("parent", &self.parent.as_ref().map(|parent| parent.id))
            .field("level", &self.level)
            .field("owner_euid", &self.owner_euid)
            .field("parent_could_setfcap", &self.parent_could_setfcap)
            .finish_non_exhaustive()
    }
}

impl UserNamespace {
    fn new_root() -> Self {
        Self {
            id: NamespaceId::new(),
            parent: None,
            level: 0,
            owner_euid: 0,
            parent_could_setfcap: false,
            map_state: SpinNoPreempt::new(UserNamespaceMapState {
                setgroups_allowed: true,
                ..UserNamespaceMapState::default()
            }),
        }
    }

    /// Creates a child namespace owned by `owner_euid` in `parent`.
    ///
    /// The child starts with empty UID and GID maps and inherits the parent's
    /// `setgroups` policy, as Linux does in `create_user_ns()`. This operation
    /// only allocates namespace identity; it does not grant the caller
    /// capabilities or publish the namespace.
    ///
    /// `parent_could_setfcap` records whether the creator held `CAP_SETFCAP` in
    /// the parent. It is consulted only when the creator maps parent UID 0 into
    /// the new namespace without `CAP_SETFCAP` in the parent.
    pub fn new_child(
        parent: &alloc::sync::Arc<Self>,
        owner_euid: Uid,
        owner_egid: Gid,
        parent_could_setfcap: bool,
    ) -> KResult<alloc::sync::Arc<Self>> {
        // Linux `create_user_ns`: a parent at level > 32 cannot create another
        // child; exhaustion reports ENOSPC.
        if parent.level > 32 {
            return Err(KError::from(LinuxError::ENOSPC));
        }
        // Linux also rejects a creator whose euid/egid has no mapping in the
        // parent namespace; otherwise the new owner could never be translated
        // back to the initial namespace.
        if parent.map_uid_from_initial(owner_euid).is_none()
            || parent.map_gid_from_initial(owner_egid).is_none()
        {
            return Err(KError::OperationNotPermitted);
        }
        let setgroups_allowed = parent.setgroups_allowed();
        Ok(alloc::sync::Arc::new(Self {
            id: NamespaceId::new(),
            parent: Some(parent.clone()),
            level: parent.level + 1,
            owner_euid,
            parent_could_setfcap,
            map_state: SpinNoPreempt::new(UserNamespaceMapState {
                setgroups_allowed,
                ..UserNamespaceMapState::default()
            }),
        }))
    }

    /// Returns the namespace ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }

    /// Returns the parent user namespace, if this is not the root user namespace.
    pub fn parent(&self) -> Option<&alloc::sync::Arc<UserNamespace>> {
        self.parent.as_ref()
    }

    /// Returns the nesting depth (initial namespace is level 0).
    pub const fn level(&self) -> u32 {
        self.level
    }

    /// Returns the effective UID that owned the creating process.
    pub fn owner_euid(&self) -> Uid {
        self.owner_euid
    }

    /// Returns whether `root_id`, expressed in the initial user namespace,
    /// owns this namespace or one of its ancestors.
    ///
    /// Revision-3 file capabilities store the namespace owner that created the
    /// attribute. Linux applies such capabilities only while executing in that
    /// owner's namespace or a descendant; zero always refers to the initial
    /// namespace root.
    pub fn owns_root_id(&self, root_id: Uid) -> bool {
        if root_id == 0 {
            return true;
        }
        let mut namespace = Some(self);
        while let Some(current) = namespace {
            // `owner_euid` is already an initial-namespace ID; do not project
            // it through the parent map again.
            if current.owner_euid == root_id {
                return true;
            }
            namespace = current.parent().map(|parent| &**parent);
        }
        false
    }

    /// Returns whether `self` is `other` or an ancestor of `other`.
    ///
    /// Capability checks use this relation: a process whose credentials belong
    /// to `self` is capable only in `self` and its descendants.
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        let mut current = Some(other);
        while let Some(namespace) = current {
            if ptr::eq(self, namespace) {
                return true;
            }
            current = namespace.parent.as_deref();
        }
        false
    }

    /// Returns whether the UID map has been written.
    pub fn has_uid_map(&self) -> bool {
        let state = self.map_state.lock();
        self.parent.is_none() || state.uid_map.is_written
    }

    /// Returns whether the GID map has been written.
    pub fn has_gid_map(&self) -> bool {
        let state = self.map_state.lock();
        self.parent.is_none() || state.gid_map.is_written
    }

    /// Returns whether both ID maps have been installed.
    ///
    /// This is the explicit readiness signal for a child namespace; emptiness of
    /// the extent vector is never used as a proxy. The initial namespace is
    /// implicitly ready because it maps every ID to itself.
    pub fn is_ready(&self) -> bool {
        self.has_uid_map() && self.has_gid_map()
    }

    /// Returns whether `setgroups(2)` is permitted in this namespace.
    pub fn setgroups_allowed(&self) -> bool {
        self.map_state.lock().setgroups_allowed
    }

    /// Returns the UID map extents.
    pub fn uid_map(&self) -> Vec<IdMapExtent> {
        self.map_state.lock().uid_map.extents.clone()
    }

    /// Returns the GID map extents.
    pub fn gid_map(&self) -> Vec<IdMapExtent> {
        self.map_state.lock().gid_map.extents.clone()
    }

    /// Installs the UID map on behalf of a map-file writer.
    ///
    /// `writer` is the credential captured when the map file was opened
    /// (`file->f_cred`); `current` is the credential of the task issuing
    /// `write(2)`. Linux `new_idmap_permitted()` checks the parent-namespace
    /// capability on both the opener (`file_ns_capable`) and the current task
    /// (`ns_capable`), so dropping privilege after open cannot authorize a map
    /// write.
    ///
    /// The map may be written successfully once. Shape, overlap, capability,
    /// ownership, parent coverage, and single-write rules are enforced here;
    /// the caller only has to verify that the target namespace is the opener's
    /// namespace or its immediate parent before passing `file->f_cred`.
    pub fn write_uid_map(
        &self,
        writer: &Cred,
        current: &Cred,
        extents: Vec<IdMapExtent>,
    ) -> KResult<()> {
        self.write_map(MapKind::Uid, writer, current, extents)
    }

    /// Installs the GID map on behalf of a map-file writer.
    ///
    /// The same authorization rules as [`Self::write_uid_map`] apply, with
    /// `CAP_SETGID` in place of `CAP_SETUID`. A GID map is writable regardless
    /// of the namespace's `setgroups` policy; an unprivileged writer using the
    /// single-extent self-map shortcut must have disabled `setgroups` first.
    pub fn write_gid_map(
        &self,
        writer: &Cred,
        current: &Cred,
        extents: Vec<IdMapExtent>,
    ) -> KResult<()> {
        self.write_map(MapKind::Gid, writer, current, extents)
    }

    fn write_map(
        &self,
        kind: MapKind,
        writer: &Cred,
        current: &Cred,
        mut extents: Vec<IdMapExtent>,
    ) -> KResult<()> {
        let Some(parent) = self.parent.as_ref() else {
            return Err(KError::OperationNotPermitted);
        };
        if self.map_is_written(kind) {
            return Err(KError::OperationNotPermitted);
        }
        if !writer.has_cap_in(self, CAP_SYS_ADMIN) {
            return Err(KError::OperationNotPermitted);
        }
        // An empty write is rejected as malformed and leaves the map writable,
        // so the writer can retry with real extents.
        validate_id_map(&mut extents)?;
        if !self.id_map_write_permitted(kind, writer, current, &extents) {
            return Err(KError::OperationNotPermitted);
        }
        for extent in &extents {
            if !parent.maps_range_to_parent(kind, extent.outside(), extent.count()) {
                return Err(KError::OperationNotPermitted);
            }
        }

        let mut state = self.map_state.lock();
        let map = match kind {
            MapKind::Uid => &mut state.uid_map,
            MapKind::Gid => &mut state.gid_map,
        };
        if map.is_written {
            return Err(KError::OperationNotPermitted);
        }
        map.extents = extents;
        map.is_written = true;
        Ok(())
    }

    fn map_is_written(&self, kind: MapKind) -> bool {
        let state = self.map_state.lock();
        match kind {
            MapKind::Uid => state.uid_map.is_written,
            MapKind::Gid => state.gid_map.is_written,
        }
    }

    /// Linux `new_idmap_permitted()`.
    fn id_map_write_permitted(
        &self,
        kind: MapKind,
        writer: &Cred,
        current: &Cred,
        extents: &[IdMapExtent],
    ) -> bool {
        let Some(parent) = self.parent.as_ref() else {
            return false;
        };
        if kind == MapKind::Uid && !self.root_map_is_permitted(writer, extents) {
            return false;
        }

        // A namespace owner may map exactly its own ID without capabilities in
        // the parent. For GIDs this shortcut requires that `setgroups(2)` has
        // already been disabled, since otherwise the process could drop groups.
        if extents.len() == 1 && extents[0].count() == 1 {
            // Owner IDs are stored in initial-namespace coordinates (the
            // contract `new_child` checks); comparing a re-projected value
            // would treat the global ID as parent-local and fail for nested
            // namespaces.
            let owner = Some(self.owner_euid);
            let mapped = match kind {
                MapKind::Uid => parent.map_uid_to_initial(extents[0].outside()),
                MapKind::Gid => parent.map_gid_to_initial(extents[0].outside()),
            };
            let writer_id = self.writer_id_in_initial(kind, writer);
            let is_owner_self_map = match kind {
                MapKind::Uid => matches!(
                    (owner, mapped, writer_id),
                    (Some(owner), Some(mapped), Some(writer_id))
                        if owner == mapped && owner == writer_id
                ),
                // A GID map is authorized by the writer's own EGID, not by
                // the namespace creator's UID; comparing UID to GID would
                // require uid == gid.
                MapKind::Gid => {
                    mapped.is_some() && mapped == writer_id && self.owner_euid == writer.euid()
                }
            };
            if kind == MapKind::Uid && is_owner_self_map {
                return true;
            }
            if kind == MapKind::Gid && !self.setgroups_allowed() && is_owner_self_map {
                return true;
            }
        }

        let capability = match kind {
            MapKind::Uid => CAP_SETUID,
            MapKind::Gid => CAP_SETGID,
        };
        current.has_cap_in(parent, capability) && writer.has_cap_in(parent, capability)
    }

    /// Linux `verify_root_map()`: mapping parent UID 0 into a namespace can
    /// create file capabilities valid in ancestors, so it needs `CAP_SETFCAP`
    /// in the parent (or the creator had it when writing its own map).
    fn root_map_is_permitted(&self, writer: &Cred, extents: &[IdMapExtent]) -> bool {
        if !extents.iter().any(|extent| extent.outside() == 0) {
            return true;
        }
        if core::ptr::eq(writer.user_ns(), self) {
            return self.parent_could_setfcap;
        }
        self.parent
            .as_ref()
            .is_some_and(|parent| writer.has_cap_in(parent, CAP_SETFCAP))
    }

    /// Translates the writer's effective ID into initial-namespace coordinates.
    ///
    /// Credentials keep their numeric IDs when they enter a new user namespace,
    /// before that namespace has a map. While the relevant map is still empty,
    /// those numbers are therefore still expressed in the parent namespace. Once
    /// the map is installed, the writer's own namespace is authoritative.
    fn writer_id_in_initial(&self, kind: MapKind, writer: &Cred) -> Option<Uid> {
        // Credentials store IDs in initial-namespace coordinates, including in
        // the window between `clone(CLONE_NEWUSER)` and the first map write.
        // Do not translate them through this namespace's (possibly empty) map
        // or the parent again.
        Some(match kind {
            MapKind::Uid => writer.euid(),
            MapKind::Gid => writer.egid(),
        })
    }

    /// Applies a `/proc/<pid>/setgroups` write.
    ///
    /// `allow` succeeds only while the namespace still allows supplemental
    /// groups, so it is idempotent but cannot undo `deny`. `deny` succeeds
    /// until a GID map is installed and is likewise idempotent. The caller is
    /// responsible for parsing the text and for the capability check on the
    /// target namespace.
    pub fn set_setgroups_allowed(&self, allowed: bool) -> KResult<()> {
        let mut state = self.map_state.lock();
        if allowed {
            if !state.setgroups_allowed {
                return Err(KError::OperationNotPermitted);
            }
            return Ok(());
        }
        if self.parent.is_none() || state.gid_map.is_written {
            return Err(KError::OperationNotPermitted);
        }
        state.setgroups_allowed = false;
        Ok(())
    }

    /// Maps a local UID to the parent namespace.
    ///
    /// Returns `None` when the UID is outside every written extent. The root
    /// namespace maps every UID to itself.
    pub fn map_uid_to_parent(&self, uid: Uid) -> Option<Uid> {
        self.map_to_parent(MapKind::Uid, uid)
    }

    /// Maps a parent-namespace UID to this namespace.
    pub fn map_uid_from_parent(&self, uid: Uid) -> Option<Uid> {
        self.map_from_parent(MapKind::Uid, uid)
    }

    /// Maps a local GID to the parent namespace.
    pub fn map_gid_to_parent(&self, gid: Gid) -> Option<Gid> {
        self.map_to_parent(MapKind::Gid, gid)
    }

    /// Maps a parent-namespace GID to this namespace.
    pub fn map_gid_from_parent(&self, gid: Gid) -> Option<Gid> {
        self.map_from_parent(MapKind::Gid, gid)
    }

    /// Maps a local UID all the way to the initial namespace.
    pub fn map_uid_to_initial(&self, uid: Uid) -> Option<Uid> {
        self.map_id_to_initial(MapKind::Uid, uid)
    }

    /// Maps an initial-namespace UID into this namespace.
    pub fn map_uid_from_initial(&self, uid: Uid) -> Option<Uid> {
        self.map_id_from_initial(MapKind::Uid, uid)
    }

    /// Maps a local GID all the way to the initial namespace.
    pub fn map_gid_to_initial(&self, gid: Gid) -> Option<Gid> {
        self.map_id_to_initial(MapKind::Gid, gid)
    }

    /// Maps an initial-namespace GID into this namespace.
    pub fn map_gid_from_initial(&self, gid: Gid) -> Option<Gid> {
        self.map_id_from_initial(MapKind::Gid, gid)
    }

    fn map_id_to_initial(&self, kind: MapKind, id: u32) -> Option<u32> {
        let mut current = self;
        let mut id = id;
        loop {
            if current.parent.is_none() {
                return Some(id);
            }
            id = current.map_to_parent(kind, id)?;
            current = current.parent.as_deref()?;
        }
    }

    fn map_id_from_initial(&self, kind: MapKind, id: u32) -> Option<u32> {
        let mut chain = Vec::new();
        let mut current = self;
        while let Some(parent) = current.parent.as_deref() {
            chain.push(current);
            current = parent;
        }
        let mut id = id;
        for namespace in chain.into_iter().rev() {
            id = namespace.map_from_parent(kind, id)?;
        }
        Some(id)
    }

    fn maps_range_to_parent(&self, kind: MapKind, first: u32, count: u32) -> bool {
        if self.parent.is_none() {
            return u64::from(first) + u64::from(count) <= (1u64 << 32);
        }
        // `first` names an ID in this namespace, so the range must first be
        // translated through the namespace-local (`inside`) side of one extent.
        // The resulting ID belongs to the parent, and the parent must in turn
        // be able to translate it all the way to the initial namespace.
        let translated = {
            let state = self.map_state.lock();
            match kind {
                MapKind::Uid => state.uid_map.map_inside_range(first, count),
                MapKind::Gid => state.gid_map.map_inside_range(first, count),
            }
        };
        let Some(translated) = translated else {
            return false;
        };
        let Some(parent) = self.parent.as_ref() else {
            return false;
        };
        parent.maps_range_to_parent(kind, translated, count)
    }

    fn map_to_parent(&self, kind: MapKind, id: u32) -> Option<u32> {
        if self.parent.is_none() {
            return Some(id);
        }
        let state = self.map_state.lock();
        match kind {
            MapKind::Uid => state.uid_map.map_inside(id),
            MapKind::Gid => state.gid_map.map_inside(id),
        }
    }

    fn map_from_parent(&self, kind: MapKind, id: u32) -> Option<u32> {
        if self.parent.is_none() {
            return Some(id);
        }
        let state = self.map_state.lock();
        match kind {
            MapKind::Uid => state.uid_map.map_outside(id),
            MapKind::Gid => state.gid_map.map_outside(id),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MapKind {
    Uid,
    Gid,
}

fn validate_id_map(extents: &mut [IdMapExtent]) -> KResult<()> {
    if extents.is_empty() || extents.len() > ID_MAP_MAX_EXTENTS {
        return Err(KError::InvalidInput);
    }
    extents.sort_unstable_by_key(|extent| extent.inside());
    for pair in extents.windows(2) {
        if pair[0].inside_end() > u64::from(pair[1].inside()) {
            return Err(KError::InvalidInput);
        }
    }
    let mut by_outside = extents.to_vec();
    by_outside.sort_unstable_by_key(|extent| extent.outside());
    for pair in by_outside.windows(2) {
        if pair[0].outside_end() > u64::from(pair[1].outside()) {
            return Err(KError::InvalidInput);
        }
    }
    Ok(())
}

/// Returns a shared reference to the lazily allocated initial user namespace.
///
/// Its parent is `None` and it maps every UID and GID to itself. Repeated calls
/// clone the same `Arc`. Initialization requires an allocator, may spin on a
/// concurrent initializer, and must not be re-entered.
///
/// # Panics
///
/// Panics if an earlier initialization unwound and poisoned the internal
/// [`klazy::Once`]. Allocation failure follows the allocator's failure policy.
pub fn initial_user_namespace() -> alloc::sync::Arc<UserNamespace> {
    alloc::sync::Arc::clone(
        INIT_USER_NS.call_once(|| alloc::sync::Arc::new(UserNamespace::new_root())),
    )
}

unittest::path_mod_test!(tests, "../ut-tests/namespace_unittest.rs");
