// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Credential state and POSIX/Linux set-ID transitions.

use alloc::{sync::Arc, vec::Vec};

use kerrno::{KError, KResult};

use super::{
    CAP_FS_MASK, CAP_LAST_CAP, CAP_SETGID, CAP_SETPCAP, CAP_SETUID, CAP_SYS_NICE, CAP_SYS_PTRACE,
    CapSet, CapSets, FileCapabilities, Gid, Uid, is_valid_gid, is_valid_uid,
    securebits::SecureBits,
};
use crate::{UserNamespace, initial_user_namespace};

/// Credential mode used by Linux `PTRACE_MODE_READ_*` access checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtraceAccessMode {
    /// Compare against caller real IDs and permitted capabilities.
    RealCreds,
    /// Compare against caller filesystem IDs and effective capabilities.
    FsCreds,
}

/// Identity changes an executable's set-ID bits request at `execve(2)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecIds {
    /// Effective user ID after the exec.
    pub euid: Uid,
    /// Effective group ID after the exec.
    pub egid: Gid,
}

/// Executable-derived inputs to the exec credential transition.
///
/// `execve(2)` samples these from the resolved image: its set-ID bits (when the
/// mount is not `nosuid` and `no_new_privs` is clear) and its
/// `security.capability` attribute. A transition without either is the plain
/// no-file-capability exec that [`ExecSecurity::NONE`] describes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecSecurity {
    /// Set-ID identity change requested by the file's mode bits.
    pub ids: Option<ExecIds>,
    /// File capabilities stored in the file's `security.capability` attribute.
    pub file_caps: Option<FileCapabilities>,
}

impl ExecSecurity {
    /// An exec that grants nothing: no set-ID bits and no file capabilities.
    pub const NONE: Self = Self {
        ids: None,
        file_caps: None,
    };
}

/// User/group identity and keep-capabilities state for a task credential.
///
/// A committed credential is held through [`Arc<Cred>`] and treated as
/// immutable. Credential transitions operate on an uncommitted clone returned
/// by [`Cred::prepare`].
#[derive(Clone)]
pub struct Cred {
    ruid: Uid,
    euid: Uid,
    suid: Uid,
    fsuid: Uid,
    rgid: Gid,
    egid: Gid,
    sgid: Gid,
    fsgid: Gid,
    supplementary_groups: Arc<[Gid]>,
    /// Linux secure-bits controlling set-ID fixups and keep-capabilities.
    securebits: SecureBits,
    /// Effective, permitted and inheritable capability sets.
    capabilities: CapSets,
    /// Ambient capabilities preserved across exec by unprivileged tasks.
    cap_ambient: CapSet,
    /// Capabilities allowed in file capability sets installed by exec.
    cap_bounding: CapSet,
    /// User namespace in which the IDs and capability checks are interpreted.
    user_ns: Arc<UserNamespace>,
}

/// Returns the single-capability mask for `cap`.
fn cap_bit(cap: u32) -> KResult<CapSet> {
    if cap > CAP_LAST_CAP {
        return Err(KError::InvalidInput);
    }
    Ok(CapSet::from_bits(1u64 << cap))
}

impl Cred {
    /// Creates root credentials for the initial process.
    ///
    /// The result owns the full bounding, permitted, and effective sets in the
    /// initial user namespace. The inheritable and ambient sets start empty,
    /// matching a freshly booted Linux process.
    pub fn root() -> Self {
        Self::root_in_user_namespace(initial_user_namespace())
    }

    /// Creates root credentials interpreted in `user_ns`.
    ///
    /// All IDs are zero in `user_ns`, not in the initial namespace. The result
    /// owns the full capability sets, which makes it the credential a task
    /// receives immediately after creating a user namespace. This is a trusted
    /// constructor: it neither maps the IDs nor grants the caller authority
    /// over `user_ns`.
    pub fn root_in_user_namespace(user_ns: Arc<UserNamespace>) -> Self {
        let mut cred = Self::new_in_user_namespace(0, 0, user_ns);
        cred.capabilities = CapSets::FULL_PRIVILEGED;
        cred
    }

    /// Returns a copy whose capabilities are interpreted in `user_ns`.
    ///
    /// This is the credential transition performed by `set_cred_user_ns()`:
    /// entering a user namespace resets every piece of capability state that
    /// describes authority rather than identity. The permitted, effective and
    /// bounding sets become full in the new namespace, while the inheritable
    /// and ambient sets are cleared because their old values are only
    /// meaningful in the previous namespace. Securebits return to their
    /// defaults for the same reason. IDs are left untouched.
    pub fn with_user_namespace(&self, user_ns: Arc<UserNamespace>) -> Self {
        let mut cred = self.prepare();
        cred.user_ns = user_ns;
        cred.securebits = SecureBits::empty();
        cred.capabilities = CapSets::new(CapSet::FULL, CapSet::FULL, CapSet::EMPTY);
        cred.cap_ambient = CapSet::EMPTY;
        cred.cap_bounding = CapSet::FULL;
        cred
    }

    /// Creates credentials with all user IDs set to `uid` and all group IDs to `gid`.
    ///
    /// Supplementary groups are empty and securebits are clear. This trusted
    /// constructor performs no authorization or namespace mapping; the caller
    /// must authorize these IDs before publishing the credential. Allocation
    /// of the shared group storage follows the allocator's failure policy.
    pub fn new(uid: Uid, gid: Gid) -> Self {
        Self::new_in_user_namespace(uid, gid, initial_user_namespace())
    }

    /// Creates credentials with every user and group ID set to `uid` and `gid`
    /// in `user_ns`.
    ///
    /// Capability sets follow the same rules as [`Self::new`]: a numeric-root
    /// identity starts fully privileged inside `user_ns`. This trusted
    /// constructor performs no authorization or ID mapping; the caller must
    /// establish that the IDs are valid in `user_ns` before publishing.
    pub fn new_in_user_namespace(uid: Uid, gid: Gid, user_ns: Arc<UserNamespace>) -> Self {
        Self {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
            supplementary_groups: Arc::from([]),
            securebits: SecureBits::empty(),
            capabilities: if uid == 0 {
                CapSets::FULL_PRIVILEGED
            } else {
                CapSets::EMPTY
            },
            cap_ambient: CapSet::EMPTY,
            cap_bounding: CapSet::FULL,
            user_ns,
        }
    }

    /// Prepares an uncommitted copy for a credential transition.
    ///
    /// Scalar fields are copied and the immutable group array is shared through
    /// `Arc`. Mutating the returned value does not alter this credential.
    /// This method neither authorizes nor publishes a replacement; see the
    /// crate-level example for the prepare/transition sequence.
    pub fn prepare(&self) -> Self {
        self.clone()
    }

    /// Prepares credentials for an `access(2)` check using real IDs.
    ///
    /// The returned copy's filesystem IDs become its real IDs. Unless
    /// `SECURE_NO_SETUID_FIXUP` is set, its effective capability set is cleared
    /// for a non-root real UID or replaced by the permitted set for a root real
    /// UID, matching Linux's access override credential. The permitted,
    /// inheritable, ambient, and bounding sets, securebits, and all
    /// non-filesystem IDs are preserved. This prepares identity data and does
    /// not itself perform a file access check.
    pub fn for_access(&self) -> Self {
        let mut cred = self.prepare();
        cred.fsuid = cred.ruid;
        cred.fsgid = cred.rgid;
        if !cred.securebits.contains(SecureBits::NO_SETUID_FIXUP) {
            let effective = if cred.ruid == 0 {
                cred.capabilities.permitted()
            } else {
                CapSet::EMPTY
            };
            cred.capabilities = CapSets::new(
                effective,
                cred.capabilities.permitted(),
                cred.capabilities.inheritable(),
            );
        }
        cred
    }

    /// Returns the real user ID.
    pub fn ruid(&self) -> Uid {
        self.ruid
    }

    /// Returns the effective user ID.
    pub fn euid(&self) -> Uid {
        self.euid
    }

    fn is_root_id(&self, id: Uid) -> bool {
        self.user_ns.map_uid_to_initial(0) == Some(id)
    }

    /// Returns whether the effective user ID is root.
    ///
    /// Linux uses this identity check for `/proc/sys` access
    /// (`test_perm()`), where neither capability sets nor the filesystem UID
    /// participate. It is not a general privilege predicate; capability-gated
    /// operations must use [`Cred::has_cap`] instead.
    pub fn has_root_euid(&self) -> bool {
        self.euid == 0
    }

    /// Returns the saved set-user-ID.
    pub fn suid(&self) -> Uid {
        self.suid
    }

    /// Returns the filesystem user ID used for file access checks.
    pub fn fsuid(&self) -> Uid {
        self.fsuid
    }

    /// Returns the real group ID.
    pub fn rgid(&self) -> Gid {
        self.rgid
    }

    /// Returns the effective group ID.
    pub fn egid(&self) -> Gid {
        self.egid
    }

    /// Returns the saved set-group-ID.
    pub fn sgid(&self) -> Gid {
        self.sgid
    }

    /// Returns the filesystem group ID used for file access checks.
    pub fn fsgid(&self) -> Gid {
        self.fsgid
    }

    /// Returns the supplementary group list.
    pub fn supplementary_groups(&self) -> &[Gid] {
        &self.supplementary_groups
    }

    /// Returns the Linux secure-bits word.
    pub fn securebits(&self) -> u32 {
        self.securebits.bits()
    }

    /// Replaces the Linux secure-bits word (`prctl(PR_SET_SECUREBITS)`).
    ///
    /// Unsupported bits, changes to locked settings, and attempts to unlock a
    /// lock are always rejected. Without effective `CAP_SETPCAP`, the request
    /// must actually change the word and may touch only the exec-check pair
    /// and its locks (Linux's `SECURE_ALL_UNPRIVILEGED` range). A caller with
    /// `CAP_SETPCAP` may change every supported setting. The complete word is
    /// stored only after every check succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] for any rejected transition.
    /// The credential is unchanged on failure.
    pub fn set_securebits(&mut self, bits: usize) -> KResult<()> {
        let supported = (SecureBits::ALL_BITS | SecureBits::ALL_LOCKS).bits() as usize;
        if bits & !supported != 0 {
            return Err(KError::OperationNotPermitted);
        }

        let new = SecureBits::from_bits_retain(bits as u32);
        let old = self.securebits;
        let locks = SecureBits::ALL_LOCKS.bits();
        let changes_locked = ((old.bits() & locks) >> 1) & (old.bits() ^ new.bits());
        let unlocks = old.bits() & locks & !new.bits();
        if changes_locked != 0 || unlocks != 0 {
            return Err(KError::OperationNotPermitted);
        }
        if !self.has_cap_in(self.user_ns(), CAP_SETPCAP) {
            let unprivileged = SecureBits::UNPRIVILEGED.bits();
            if old == new || (old.bits() ^ new.bits()) & !unprivileged != 0 {
                return Err(KError::OperationNotPermitted);
            }
        }

        self.securebits = new;
        Ok(())
    }

    /// Returns whether the keep-capabilities flag is set.
    pub fn keep_caps(&self) -> bool {
        self.securebits.contains(SecureBits::KEEP_CAPS)
    }

    /// Returns whether the keep-capabilities flag is locked.
    fn keep_caps_locked(&self) -> bool {
        self.securebits.contains(SecureBits::KEEP_CAPS_LOCKED)
    }

    /// Enables the keep-capabilities flag (`prctl(PR_SET_KEEPCAPS, 1)`).
    ///
    /// The flag suppresses the capability drop performed when a set-user-ID
    /// transition moves all of the real, effective, and saved IDs away from
    /// root. `PR_GET_KEEPCAPS` observes it, and exec clears it.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] if `KEEP_CAPS_LOCKED` is set,
    /// even if the requested flag value is already present. The credential is unchanged.
    pub fn keep_caps_enable(&mut self) -> KResult<()> {
        if self.keep_caps_locked() {
            return Err(KError::OperationNotPermitted);
        }
        self.securebits.insert(SecureBits::KEEP_CAPS);
        Ok(())
    }

    /// Clears the keep-capabilities flag (`prctl(PR_SET_KEEPCAPS, 0)`).
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] if `KEEP_CAPS_LOCKED` is set,
    /// even if the requested flag value is already present. The credential is unchanged.
    pub fn keep_caps_disable(&mut self) -> KResult<()> {
        if self.keep_caps_locked() {
            return Err(KError::OperationNotPermitted);
        }
        self.securebits.remove(SecureBits::KEEP_CAPS);
        Ok(())
    }

    /// Locks the keep-capabilities flag for credential-model tests.
    #[cfg(unittest)]
    pub(crate) fn lock_keep_caps_for_test(&mut self) {
        self.securebits.insert(SecureBits::KEEP_CAPS_LOCKED);
    }

    /// Returns whether `gid` matches the filesystem or supplementary groups.
    ///
    /// This mirrors Linux `in_group_p()`: the effective GID participates only
    /// when it is also the filesystem GID or appears in the supplementary list.
    pub fn in_group(&self, gid: Gid) -> bool {
        self.fsgid == gid || self.supplementary_groups.binary_search(&gid).is_ok()
    }

    /// Returns the process's effective, permitted and inheritable capability
    /// sets.
    pub fn capabilities(&self) -> CapSets {
        self.capabilities
    }

    /// Returns the ambient capability set.
    ///
    /// Ambient capabilities survive exec for an unprivileged task and are
    /// always a subset of the permitted and inheritable sets. Linux does not
    /// expose this set through `capget(2)`; `PR_CAP_AMBIENT_IS_SET` queries it
    /// one capability at a time.
    pub fn cap_ambient(&self) -> CapSet {
        self.cap_ambient
    }

    /// Returns whether `cap` is in the ambient set.
    pub fn has_ambient_cap(&self, cap: u32) -> bool {
        self.cap_ambient.contains(cap)
    }

    /// Raises `cap` into the ambient set (`prctl(PR_CAP_AMBIENT_RAISE)`).
    ///
    /// Linux requires the capability to be present in both the permitted and
    /// inheritable sets and refuses the raise when
    /// `SECURE_NO_CAP_AMBIENT_RAISE` is set. The bounding set is deliberately
    /// not consulted: dropping a capability from the bounding set does not
    /// retroactively constrain an already-permitted ambient capability.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] for a number above [`CAP_LAST_CAP`],
    /// and [`KError::OperationNotPermitted`] when the ambient precondition is
    /// not met. The credential is unchanged on failure.
    pub fn raise_cap_ambient(&mut self, cap: u32) -> KResult<()> {
        let bit = cap_bit(cap)?;
        if !self.capabilities.permitted().contains(cap)
            || !self.capabilities.inheritable().contains(cap)
            || self.securebits.contains(SecureBits::NO_CAP_AMBIENT_RAISE)
        {
            return Err(KError::OperationNotPermitted);
        }
        self.cap_ambient = self.cap_ambient.union(bit);
        Ok(())
    }

    /// Drops `cap` from the ambient set (`prctl(PR_CAP_AMBIENT_LOWER)`).
    ///
    /// Lowering is always permitted for a valid capability number, even when
    /// the capability is not currently ambient.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] for a number above [`CAP_LAST_CAP`].
    pub fn lower_cap_ambient(&mut self, cap: u32) -> KResult<()> {
        let bit = cap_bit(cap)?;
        self.cap_ambient = self.cap_ambient.difference(bit);
        Ok(())
    }

    /// Clears the ambient set (`prctl(PR_CAP_AMBIENT_CLEAR_ALL)`).
    pub fn clear_cap_ambient(&mut self) {
        self.cap_ambient = CapSet::EMPTY;
    }

    /// Returns the capability bounding set.
    pub fn cap_bounding(&self) -> CapSet {
        self.cap_bounding
    }

    /// Returns whether the effective capability set contains `cap`.
    ///
    /// This is the authority predicate for operations gated by a Linux
    /// capability. Real, saved and filesystem IDs do not grant authority, and
    /// capability numbers above [`CAP_LAST_CAP`] are never reported as held.
    pub fn has_cap(&self, cap: u32) -> bool {
        core::ptr::eq(&*self.user_ns, &*crate::initial_user_namespace())
            && self.capabilities.has_effective(cap)
    }

    /// Returns the user namespace this credential is interpreted in.
    pub fn user_ns(&self) -> &UserNamespace {
        &self.user_ns
    }

    /// Returns the shared handle to this credential's user namespace.
    pub fn user_ns_arc(&self) -> &Arc<UserNamespace> {
        &self.user_ns
    }

    /// Returns whether this credential holds `cap` over `target_ns`.
    ///
    /// This is Linux `cap_capable()`: walk from the target namespace up the
    /// parent chain, granting authority through two implicit rules before
    /// consulting the caller's effective set in its own namespace:
    ///
    /// 1. The owner (`euid`) of a user namespace has all capabilities over
    ///    that namespace when acting from its direct parent.
    /// 2. Authority never flows downward: a target at or below the caller's
    ///    own namespace level is denied outright.
    pub fn has_cap_in(&self, target_ns: &UserNamespace, cap: u32) -> bool {
        let own_ns = self.user_ns();
        let mut namespace = target_ns;
        loop {
            if core::ptr::eq(namespace, own_ns) {
                return self.capabilities.has_effective(cap);
            }
            // A target that is not a strict descendant of the caller's
            // namespace cannot be administered from here.
            if namespace.level() <= own_ns.level() {
                return false;
            }
            // Rule 1: the creator of a user namespace owns all capabilities
            // inside it when checking from the namespace's direct parent.
            if let Some(parent) = namespace.parent()
                && core::ptr::eq(&**parent, own_ns)
                && namespace.owner_euid() == self.euid()
            {
                return true;
            }
            match namespace.parent() {
                Some(parent) => namespace = parent,
                // Walking past the initial namespace without meeting the
                // caller's namespace means the caller is unrelated.
                None => return false,
            }
        }
    }

    /// Applies Linux `capset(2)` transition rules and installs `new`.
    ///
    /// The effective, permitted and inheritable sets are changed together only
    /// after [`CapSets::validate_transition`] accepts the replacement. The
    /// current bounding set bounds the new inheritable set even though it is
    /// not changed by this operation. As in Linux, ambient capabilities outside
    /// the new permitted and inheritable intersection are dropped rather than
    /// making the transition fail.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] when the transition violates a
    /// Linux capability rule. The credential is unchanged on failure.
    pub fn set_capabilities(&mut self, new: CapSets) -> KResult<()> {
        self.capabilities
            .validate_transition(&new, self.cap_bounding)?;
        self.capabilities = new;
        self.cap_ambient = self
            .cap_ambient
            .intersection(new.permitted().intersection(new.inheritable()));
        Ok(())
    }

    /// Removes `cap` from the capability bounding set.
    ///
    /// This implements the policy of Linux `PR_CAPBSET_DROP`: the caller must
    /// hold `CAP_SETPCAP` in its effective set before the capability number is
    /// validated, and an out-of-range capability cannot be dropped. `cap` is
    /// the full syscall argument width, so a value that would truncate to a
    /// valid capability is still rejected rather than silently dropping that
    /// capability.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] without `CAP_SETPCAP`, or
    /// [`KError::InvalidInput`] when `cap` exceeds [`CAP_LAST_CAP`].
    pub fn drop_bounding_cap(&mut self, cap: usize) -> KResult<()> {
        if !self.has_cap_in(self.user_ns(), CAP_SETPCAP) {
            return Err(KError::OperationNotPermitted);
        }
        if cap > CAP_LAST_CAP as usize {
            return Err(KError::InvalidInput);
        }
        self.cap_bounding = self.cap_bounding.without(cap as u32);
        Ok(())
    }

    /// Returns this credential's `CAP_SYS_ADMIN` authorization decision.
    ///
    /// Consumers must use the current caller's subjective credential, not an
    /// opener's snapshot.
    pub fn has_sys_admin(&self) -> bool {
        self.has_cap(super::CAP_SYS_ADMIN)
    }

    /// Returns whether the caller holds `CAP_SYS_RESOURCE`.
    pub fn has_sys_resource(&self) -> bool {
        self.has_cap(super::CAP_SYS_RESOURCE)
    }

    /// Returns this credential's `CAP_SYS_TTY_CONFIG` authorization decision.
    pub fn has_sys_tty_config(&self) -> bool {
        self.has_cap(super::CAP_SYS_TTY_CONFIG)
    }

    /// Reports whether the effective set holds Linux `CAP_CHOWN`
    /// (changing a file's owner regardless of the caller's ownership).
    pub fn has_cap_chown(&self) -> bool {
        self.has_cap(super::CAP_CHOWN)
    }

    /// Reports whether the effective set holds Linux `CAP_FOWNER`
    /// (bypassing owner checks for metadata changes).
    pub fn has_cap_fowner(&self) -> bool {
        self.has_cap(super::CAP_FOWNER)
    }

    /// Returns whether Linux's `cap_safe_nice()` capability layer allows this
    /// caller to change `target`'s scheduling attributes.
    ///
    /// The caller must either hold `CAP_SYS_NICE` in its effective set or have
    /// a permitted set that contains every capability in the target's permitted
    /// set. This is the capability half of scheduler authorization; Linux's
    /// ownership check (`check_same_owner()`) remains the caller's responsibility.
    pub fn has_scheduler_capability_over(&self, target: &Self) -> bool {
        self.has_cap_in(target.user_ns(), CAP_SYS_NICE)
            || target
                .capabilities
                .permitted()
                .is_subset_of(self.capabilities.permitted())
    }

    /// Returns whether Linux `PTRACE_MODE_READ_*` authorization allows this
    /// caller to inspect `target`'s credentials.
    ///
    /// Identity is compared against the caller's real IDs for
    /// [`PtraceAccessMode::RealCreds`] or filesystem IDs for
    /// [`PtraceAccessMode::FsCreds`]. Matching identity is not sufficient by
    /// itself: the target's permitted set must also be a subset of the caller's
    /// permitted set in real-credentials mode, or effective set in
    /// filesystem-credentials mode. As in Linux, an effective `CAP_SYS_PTRACE`
    /// overrides both conditions.
    ///
    /// Same-thread-group callers are exempted by the process-layer access
    /// checks, not here.
    pub fn can_read_ptrace(&self, target: &Self, mode: PtraceAccessMode) -> bool {
        if self.has_cap_in(target.user_ns(), CAP_SYS_PTRACE) {
            return true;
        }
        let (caller_uid, caller_gid) = match mode {
            PtraceAccessMode::RealCreds => (self.ruid, self.rgid),
            PtraceAccessMode::FsCreds => (self.fsuid, self.fsgid),
        };
        if target.ruid != caller_uid
            || target.euid != caller_uid
            || target.suid != caller_uid
            || target.rgid != caller_gid
            || target.egid != caller_gid
            || target.sgid != caller_gid
        {
            return false;
        }
        let caller_capabilities = match mode {
            PtraceAccessMode::RealCreds => self.capabilities.permitted(),
            PtraceAccessMode::FsCreds => self.capabilities.effective(),
        };
        target
            .capabilities
            .permitted()
            .is_subset_of(caller_capabilities)
    }

    /// Implements Linux/POSIX `setuid`.
    ///
    /// A process with `CAP_SETUID` sets real, effective, saved, and filesystem
    /// UIDs. Without it, the process may set only its effective/filesystem UID,
    /// and only to its current real or saved set-user ID.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] for [`crate::INVALID_UID`]. Returns
    /// [`KError::OperationNotPermitted`] if the credential lacks `CAP_SETUID`
    /// and the requested UID matches neither the real nor saved set-user ID.
    /// No fields change on rejection.
    pub fn set_uid(&mut self, uid: Uid) -> KResult<()> {
        if !is_valid_uid(uid) {
            return Err(KError::InvalidInput);
        }

        let old = self.prepare();
        if self.has_cap_in(self.user_ns(), CAP_SETUID) {
            self.set_resuid_unchecked(Some(uid), Some(uid), Some(uid));
            self.apply_setid_caps(&old);
            return Ok(());
        }

        if uid != self.ruid && uid != self.suid {
            return Err(KError::OperationNotPermitted);
        }

        self.euid = uid;
        self.fsuid = uid;
        self.apply_setid_caps(&old);
        Ok(())
    }

    /// Implements Linux/POSIX `setgid`.
    ///
    /// A process with `CAP_SETGID` sets real, effective, saved, and filesystem
    /// GIDs. Without it, the process may set only its effective/filesystem GID,
    /// and only to its current real or saved set-group ID.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] for [`crate::INVALID_GID`]. Returns
    /// [`KError::OperationNotPermitted`] if the credential lacks `CAP_SETGID`
    /// and the requested GID matches neither the real nor saved set-group ID.
    /// No fields change on rejection.
    pub fn set_gid(&mut self, gid: Gid) -> KResult<()> {
        if !is_valid_gid(gid) {
            return Err(KError::InvalidInput);
        }

        if self.has_cap_in(self.user_ns(), CAP_SETGID) {
            self.set_resgid_unchecked(Some(gid), Some(gid), Some(gid));
            return Ok(());
        }

        if gid != self.rgid && gid != self.sgid {
            return Err(KError::OperationNotPermitted);
        }

        self.egid = gid;
        self.fsgid = gid;
        Ok(())
    }

    /// Implements Linux `setreuid`.
    ///
    /// `None` means the corresponding syscall argument was `-1`.
    /// The saved user ID becomes the final effective ID if a real-ID argument is
    /// provided, or if the effective-ID argument differs from the old real ID.
    /// On every successful call the filesystem ID becomes the effective ID,
    /// even when both arguments are `None`.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] for an unprivileged credential
    /// if the requested real ID matches neither the old real nor effective ID,
    /// or the requested effective ID matches none of the old real, effective,
    /// and saved IDs. All checks precede mutation; rejection leaves fields unchanged.
    pub fn set_reuid(&mut self, ruid: Option<Uid>, euid: Option<Uid>) -> KResult<()> {
        let old = self.prepare();
        let old_ruid = self.ruid;
        let old_euid = self.euid;

        if !self.has_cap_in(self.user_ns(), CAP_SETUID) {
            if let Some(ruid) = ruid
                && ruid != old_ruid
                && ruid != old_euid
            {
                return Err(KError::OperationNotPermitted);
            }

            if let Some(euid) = euid
                && euid != old_ruid
                && euid != old_euid
                && euid != self.suid
            {
                return Err(KError::OperationNotPermitted);
            }
        }

        if let Some(ruid) = ruid {
            self.ruid = ruid;
        }
        if let Some(euid) = euid {
            self.euid = euid;
        }
        if ruid.is_some() || euid.is_some_and(|euid| euid != old_ruid) {
            self.suid = self.euid;
        }
        self.fsuid = self.euid;
        self.apply_setid_caps(&old);
        Ok(())
    }

    /// Implements Linux `setregid`.
    ///
    /// `None` means the corresponding syscall argument was `-1`.
    /// The saved group ID becomes the final effective ID if a real-ID argument is
    /// provided, or if the effective-ID argument differs from the old real ID.
    /// On every successful call the filesystem ID becomes the effective ID,
    /// even when both arguments are `None`.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] for an unprivileged credential
    /// if the requested real ID matches neither the old real nor effective ID,
    /// or the requested effective ID matches none of the old real, effective,
    /// and saved IDs. All checks precede mutation; rejection leaves fields unchanged.
    pub fn set_regid(&mut self, rgid: Option<Gid>, egid: Option<Gid>) -> KResult<()> {
        let old_rgid = self.rgid;
        let old_egid = self.egid;

        if !self.has_cap_in(self.user_ns(), CAP_SETGID) {
            if let Some(rgid) = rgid
                && rgid != old_rgid
                && rgid != old_egid
            {
                return Err(KError::OperationNotPermitted);
            }

            if let Some(egid) = egid
                && egid != old_rgid
                && egid != old_egid
                && egid != self.sgid
            {
                return Err(KError::OperationNotPermitted);
            }
        }

        if let Some(rgid) = rgid {
            self.rgid = rgid;
        }
        if let Some(egid) = egid {
            self.egid = egid;
        }
        if rgid.is_some() || egid.is_some_and(|egid| egid != old_rgid) {
            self.sgid = self.egid;
        }
        self.fsgid = self.egid;
        Ok(())
    }

    /// Implements Linux `setresuid`.
    ///
    /// `None` means the corresponding syscall argument was `-1`.
    /// Each supplied value replaces the corresponding user ID. Filesystem ID
    /// synchronization occurs when a supplied real/saved ID changes, or when an
    /// explicit effective ID differs from either the old effective or filesystem ID.
    /// Otherwise the call is a no-op, preserving any distinct filesystem ID.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] if an unprivileged credential
    /// requests any ID outside its current real/effective/saved set. `None` is
    /// always permitted. All checks precede mutation; rejection leaves fields unchanged.
    pub fn set_resuid(
        &mut self,
        ruid: Option<Uid>,
        euid: Option<Uid>,
        suid: Option<Uid>,
    ) -> KResult<()> {
        if !self.has_cap_in(self.user_ns(), CAP_SETUID) {
            self.check_uid_change(ruid)?;
            self.check_uid_change(euid)?;
            self.check_uid_change(suid)?;
        }

        if self.is_resuid_noop(ruid, euid, suid) {
            return Ok(());
        }

        let old = self.prepare();
        self.set_resuid_unchecked(ruid, euid, suid);
        self.apply_setid_caps(&old);
        Ok(())
    }

    /// Implements Linux `setresgid`.
    ///
    /// `None` means the corresponding syscall argument was `-1`.
    /// Each supplied value replaces the corresponding group ID. Filesystem ID
    /// synchronization occurs when a supplied real/saved ID changes, or when an
    /// explicit effective ID differs from either the old effective or filesystem ID.
    /// Otherwise the call is a no-op, preserving any distinct filesystem ID.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] if an unprivileged credential
    /// requests any ID outside its current real/effective/saved set. `None` is
    /// always permitted. All checks precede mutation; rejection leaves fields unchanged.
    pub fn set_resgid(
        &mut self,
        rgid: Option<Gid>,
        egid: Option<Gid>,
        sgid: Option<Gid>,
    ) -> KResult<()> {
        if !self.has_cap_in(self.user_ns(), CAP_SETGID) {
            self.check_gid_change(rgid)?;
            self.check_gid_change(egid)?;
            self.check_gid_change(sgid)?;
        }

        if self.is_resgid_noop(rgid, egid, sgid) {
            return Ok(());
        }

        self.set_resgid_unchecked(rgid, egid, sgid);
        Ok(())
    }

    /// Implements Linux `setfsuid`.
    ///
    /// The old filesystem UID is always returned. If the requested UID is not
    /// permitted, the credential state is left unchanged.
    ///
    /// A credential with `CAP_SETUID` accepts any target; otherwise the target
    /// must match its real, effective, saved or current filesystem user ID.
    /// The returned old value does not indicate whether the change succeeded.
    ///
    /// Crossing the root filesystem ID boundary applies Linux's
    /// `CAP_FS_MASK` fixup to the effective set. Permitted capabilities are not
    /// affected by a filesystem-ID change.
    pub fn set_fsuid(&mut self, fsuid: Uid) -> Uid {
        let old_fsuid = self.fsuid;
        let permitted = self.has_cap_in(self.user_ns(), CAP_SETUID)
            || fsuid == self.ruid
            || fsuid == self.euid
            || fsuid == self.suid
            || fsuid == old_fsuid;
        if permitted && fsuid != old_fsuid {
            self.fsuid = fsuid;
            self.apply_setfsuid_caps(old_fsuid);
        }
        old_fsuid
    }

    /// Implements Linux `setfsgid`.
    ///
    /// The old filesystem GID is always returned. If the requested GID is not
    /// permitted, the credential state is left unchanged.
    ///
    /// A credential with `CAP_SETGID` accepts any target; otherwise the target
    /// must match its real, effective, saved or current filesystem group ID.
    /// The returned old value does not indicate whether the change succeeded.
    pub fn set_fsgid(&mut self, fsgid: Gid) -> Gid {
        let old_fsgid = self.fsgid;
        if self.has_cap_in(self.user_ns(), CAP_SETGID)
            || fsgid == self.rgid
            || fsgid == self.egid
            || fsgid == self.sgid
            || fsgid == old_fsgid
        {
            self.fsgid = fsgid;
        }
        old_fsgid
    }

    /// Replaces supplementary groups.
    ///
    /// Linux sorts supplementary groups for membership checks and preserves
    /// duplicate entries for syscall-visible output.
    ///
    /// This trusted setter does not check privilege or enforce a group-count limit.
    /// The caller must perform both checks before publishing a user-requested change.
    /// It consumes `groups`, preserves duplicates, and allocates shared array storage;
    /// allocation failure follows the allocator policy rather than returning `KError`.
    pub fn set_supplementary_groups(&mut self, mut groups: Vec<Gid>) {
        groups.sort_unstable();
        self.supplementary_groups = Arc::from(groups);
    }

    /// Sets real, effective, and saved user IDs. Filesystem UID tracks the
    /// final EUID for set-ID transitions, matching Linux `setresuid`.
    pub(crate) fn set_resuid_unchecked(
        &mut self,
        ruid: Option<Uid>,
        euid: Option<Uid>,
        suid: Option<Uid>,
    ) {
        if let Some(ruid) = ruid {
            self.ruid = ruid;
        }
        if let Some(euid) = euid {
            self.euid = euid;
        }
        if let Some(suid) = suid {
            self.suid = suid;
        }
        self.fsuid = self.euid;
    }

    /// Sets real, effective, and saved group IDs. Filesystem GID tracks the
    /// final EGID for set-ID transitions, matching Linux `setresgid`.
    pub(crate) fn set_resgid_unchecked(
        &mut self,
        rgid: Option<Gid>,
        egid: Option<Gid>,
        sgid: Option<Gid>,
    ) {
        if let Some(rgid) = rgid {
            self.rgid = rgid;
        }
        if let Some(egid) = egid {
            self.egid = egid;
        }
        if let Some(sgid) = sgid {
            self.sgid = sgid;
        }
        self.fsgid = self.egid;
    }

    /// Applies the no-file-capability exec credential transition.
    ///
    /// This kernel models executables without set-user-ID/set-group-ID bits or
    /// file capabilities. Linux's `get_file_caps()` therefore starts with an
    /// empty permitted set, after which `handle_privileged_root()` reconstructs
    /// it from the bounding and inheritable sets when the real or effective UID
    /// is 0 and `SECURE_NOROOT` is clear. The ambient set is then merged into
    /// the permitted set and becomes the effective set unless privileged root
    /// grants the full permitted set.
    ///
    /// Exec itself changes neither euid nor egid, but Linux still treats the
    /// exec as an identity transition when the new egid is not in the old
    /// filesystem/supplementary group set. Because `fsgid` is independent of
    /// `egid`, that condition can hold even without set-ID executable support;
    /// it clears the ambient set. A pre-existing euid/real-UID mismatch alone
    /// is not a trigger, because exec does not change euid.
    ///
    /// Linux's post-exec downgrade applies when the exec changes identity or
    /// would grow the permitted set, and either `no_new_privs` is set or the
    /// filesystem context is shared with another process. It clamps a growing
    /// set to the pre-exec permitted set. With `no_new_privs`, or when the
    /// pre-exec credential lacks effective `CAP_SETUID`, it also resets the
    /// effective user and group IDs to the real IDs before saved/filesystem
    /// IDs are synchronized. An exec with no identity change, no permitted
    /// growth, and no unsafe sharing is unaffected.
    ///
    /// Saved and filesystem IDs become the final effective IDs; real IDs and
    /// supplementary groups otherwise remain unchanged. The inheritable and
    /// bounding sets are preserved. `KEEP_CAPS` is cleared even when its lock
    /// bit is set; every other securebit survives. The caller must publish
    /// this credential atomically and must not call this method before the
    /// exec has passed its point of no return. No executable metadata is
    /// inspected here.
    ///
    /// `fs_shared_with_other_process` corresponds to Linux's
    /// `LSM_UNSAFE_SHARE`: another process shares this process's `fs_struct`
    /// through `CLONE_FS`, so a set-ID or capability-gaining exec must not let
    /// the two processes observe inconsistent privilege. Same-thread-group
    /// sharing is not unsafe and must be reported as `false`.
    pub fn apply_exec(
        &mut self,
        no_new_privs: bool,
        fs_shared_with_other_process: bool,
    ) -> KResult<()> {
        self.apply_exec_with_security(
            no_new_privs,
            fs_shared_with_other_process,
            ExecSecurity::NONE,
        )
        .map(|_| ())
    }

    /// Executes Linux's full exec transition, including set-ID identities and
    /// file capabilities, and reports whether the exec is privileged.
    ///
    /// `security` carries what `execve(2)` sampled from the executable: the
    /// set-ID identity changes authorized by its mode bits and mount, and the
    /// `security.capability` sets it stores. Under `no_new_privs` the loader
    /// omits the set-ID identities, matching Linux `bprm_fill_uid()`, but it
    /// still samples the file capabilities; the downgrade clamp below keeps
    /// them from growing the permitted set.
    ///
    /// Returns `true` when the result is a secure exec (`bprm->secureexec`):
    /// a set-ID identity change, or a non-real-root caller whose exec gains an
    /// effective set or a permitted set wider than the new ambient set.
    /// Callers publish that as `AT_SECURE`.
    pub fn apply_exec_with_security(
        &mut self,
        no_new_privs: bool,
        fs_shared_with_other_process: bool,
        security: ExecSecurity,
    ) -> KResult<bool> {
        let old_permitted = self.capabilities.permitted();
        let old_effective_setuid = self.capabilities.has_effective(CAP_SETUID);
        let inheritable = self.capabilities.inheritable();
        let old_ruid = self.ruid;
        let old_rgid = self.rgid;

        // Linux `bprm_fill_uid()` runs before the capability transition.
        if let Some(ids) = security.ids {
            self.euid = ids.euid;
            self.egid = ids.egid;
        }
        // Linux `__is_setuid()`/`__is_setgid()` compare the post-fill effective
        // IDs with the pre-exec REAL IDs. A set-GID target is not exempted by
        // group membership, and a pre-existing euid/egid mismatch is already a
        // set-ID condition. This drives ambient clearing, the downgrade clamp
        // and `bprm->secureexec`.
        let is_setid = self.euid != old_ruid || self.egid != old_rgid;

        // Linux `handle_privileged_root()` classifies by the post-fill IDs:
        // `__is_real` is the real UID and `__is_eff` the effective UID. A
        // non-real caller elevated to effective UID 0 by a set-UID file whose
        // own capability attribute exists keeps the file formula; every other
        // real/effective root caller overrides it with the root reconstruction.
        let root_privileged = !self.securebits.contains(SecureBits::NOROOT);
        let effective_root = self.euid == 0;
        let real_root = self.ruid == 0;
        let file_caps = security.file_caps;
        let has_fcap = file_caps.is_some();
        let file_effective = file_caps.is_some_and(FileCapabilities::effective);
        // Linux `bprm_caps_from_vfs_caps()`: pP' = (X & fP) | (pI & fI).
        let file_permitted = file_caps.map_or(CapSet::EMPTY, |caps| {
            caps.permitted()
                .intersection(self.cap_bounding)
                .union(inheritable.intersection(caps.inheritable()))
        });
        let mixed_setuid_fcaps = has_fcap && !real_root && effective_root;
        let root_reconstruction =
            root_privileged && !mixed_setuid_fcaps && (effective_root || real_root);
        let mut permitted = if root_reconstruction {
            // pP' = (cap_bset & ~0) | (pI & ~0)
            self.cap_bounding
                .union(inheritable)
                .intersection(CapSet::FULL)
        } else {
            file_permitted
        };
        // `handle_privileged_root()` forces the effective bit only for
        // effective UID 0; otherwise the file's flag survives, so a privileged
        // root caller that moves its effective UID off root keeps it.
        let grant_effective = file_effective || (root_reconstruction && effective_root);

        // Linux rejects an exec whose effective file capability set cannot be
        // delivered: `bprm_caps_from_vfs_caps()` fails with `EPERM` when the
        // file's permitted set is not contained in the resulting permitted set
        // (for example because the bounding set dropped one of its bits).
        if file_effective {
            let declared = file_caps.map_or(CapSet::EMPTY, FileCapabilities::permitted);
            // Check against the file formula, not the root reconstruction: a
            // bit that only the old inheritable set could deliver is not a
            // deliverable file capability.
            if !declared.is_subset_of(file_permitted) {
                return Err(KError::OperationNotPermitted);
            }
        }

        // A file capability set is also a privilege transition: ambient
        // capabilities do not survive it even when the IDs stay put.
        let ambient = if is_setid || has_fcap {
            CapSet::EMPTY
        } else {
            self.cap_ambient
        };

        let grows_permitted = !permitted.is_subset_of(old_permitted);
        if (is_setid || grows_permitted) && (no_new_privs || fs_shared_with_other_process) {
            // Linux tests the prospective credential's effective `CAP_SETUID`,
            // which is still the pre-exec effective set at this point. The
            // identity reset does not depend on the permitted set growing, so
            // a set-UID or set-GID file under a shared filesystem context is
            // normalized even when it confers no capability.
            if no_new_privs || !old_effective_setuid {
                self.euid = self.ruid;
                self.egid = self.rgid;
            }
            permitted = permitted.intersection(old_permitted);
        }

        // Ambient capabilities are already a subset of the old permitted and
        // inheritable sets, so merging them here cannot bypass the clamp above.
        permitted = permitted.union(ambient);

        self.suid = self.euid;
        self.fsuid = self.euid;
        self.sgid = self.egid;
        self.fsgid = self.egid;

        let effective = if grant_effective { permitted } else { ambient };
        self.capabilities = CapSets::new(effective, permitted, inheritable);
        self.cap_ambient = ambient;

        // Keep-capabilities does not survive exec, even when its lock is set.
        self.securebits.remove(SecureBits::KEEP_CAPS);

        // Linux `bprm->secureexec`: an identity change, or a non-real-root
        // caller whose effective set is granted or whose permitted set exceeds
        // the ambient set (`__cap_grew(permitted, ambient)`).
        Ok(is_setid || (!real_root && (grant_effective || !permitted.is_subset_of(ambient))))
    }

    /// Applies Linux's set-user-ID capability fixup (`cap_emulate_setxuid`).
    ///
    /// Moving every real, effective, and saved ID away from root clears both
    /// the effective and permitted sets unless `KEEP_CAPS` is set, and clears
    /// the ambient set even when `KEEP_CAPS` is set. Independently, leaving
    /// effective UID 0 clears the effective set, and returning to effective
    /// UID 0 restores it from the permitted set. The inheritable set and
    /// bounding set are never changed by a set-ID call.
    fn apply_setid_caps(&mut self, old: &Self) {
        if self.securebits.contains(SecureBits::NO_SETUID_FIXUP) {
            return;
        }

        if (old.is_root_id(old.ruid) || old.is_root_id(old.euid) || old.is_root_id(old.suid))
            && !self.is_root_id(self.ruid)
            && !self.is_root_id(self.euid)
            && !self.is_root_id(self.suid)
        {
            if !self.keep_caps() {
                self.capabilities = CapSets::new(
                    CapSet::EMPTY,
                    CapSet::EMPTY,
                    self.capabilities.inheritable(),
                );
            }
            // Pre-ambient programs rely on setresuid() to non-root followed by
            // exec dropping capabilities, so KEEP_CAPS does not preserve this.
            self.cap_ambient = CapSet::EMPTY;
        }

        if old.is_root_id(old.euid) && !self.is_root_id(self.euid) {
            let permitted = self.capabilities.permitted();
            self.capabilities =
                CapSets::new(CapSet::EMPTY, permitted, self.capabilities.inheritable());
        } else if !old.is_root_id(old.euid) && self.is_root_id(self.euid) {
            let permitted = self.capabilities.permitted();
            self.capabilities = CapSets::new(permitted, permitted, self.capabilities.inheritable());
        }
    }

    /// Applies Linux's filesystem UID capability fixup.
    ///
    /// Only [`CAP_FS_MASK`] capabilities follow the root filesystem ID
    /// boundary, and only the effective set changes. `SECURE_NO_SETUID_FIXUP`
    /// disables the fixup; the permitted and inheritable sets are unaffected.
    fn apply_setfsuid_caps(&mut self, old_fsuid: Uid) {
        if self.securebits.contains(SecureBits::NO_SETUID_FIXUP) {
            return;
        }

        let inheritable = self.capabilities.inheritable();
        if self.is_root_id(old_fsuid) && !self.is_root_id(self.fsuid) {
            let effective = self.capabilities.effective().difference(CAP_FS_MASK);
            self.capabilities = CapSets::new(effective, self.capabilities.permitted(), inheritable);
        } else if !self.is_root_id(old_fsuid) && self.is_root_id(self.fsuid) {
            let effective = self
                .capabilities
                .effective()
                .union(self.capabilities.permitted().intersection(CAP_FS_MASK));
            self.capabilities = CapSets::new(effective, self.capabilities.permitted(), inheritable);
        }
    }

    fn check_uid_change(&self, uid: Option<Uid>) -> KResult<()> {
        if let Some(uid) = uid
            && uid != self.ruid
            && uid != self.euid
            && uid != self.suid
        {
            return Err(KError::OperationNotPermitted);
        }
        Ok(())
    }

    fn is_resuid_noop(&self, ruid: Option<Uid>, euid: Option<Uid>, suid: Option<Uid>) -> bool {
        ruid.is_none_or(|ruid| ruid == self.ruid)
            && euid.is_none_or(|euid| euid == self.euid && euid == self.fsuid)
            && suid.is_none_or(|suid| suid == self.suid)
    }

    fn check_gid_change(&self, gid: Option<Gid>) -> KResult<()> {
        if let Some(gid) = gid
            && gid != self.rgid
            && gid != self.egid
            && gid != self.sgid
        {
            return Err(KError::OperationNotPermitted);
        }
        Ok(())
    }

    fn is_resgid_noop(&self, rgid: Option<Gid>, egid: Option<Gid>, sgid: Option<Gid>) -> bool {
        rgid.is_none_or(|rgid| rgid == self.rgid)
            && egid.is_none_or(|egid| egid == self.egid && egid == self.fsgid)
            && sgid.is_none_or(|sgid| sgid == self.sgid)
    }
}
