// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Credential state and POSIX/Linux set-ID transitions.

use alloc::{sync::Arc, vec::Vec};

use kerrno::{KError, KResult};

use super::{Gid, Uid, securebits::SecureBits};

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
    /// Process secure-bits. Currently only KEEP_CAPS{,_LOCKED} are used.
    securebits: SecureBits,
}

impl Cred {
    /// Creates root credentials for the initial process.
    pub fn root() -> Self {
        Self::new(0, 0)
    }

    /// Creates credentials with all user IDs set to `uid` and all group IDs to `gid`.
    ///
    /// Supplementary groups are empty and securebits are clear. This trusted
    /// constructor performs no authorization or namespace mapping; the caller
    /// must authorize these IDs before publishing the credential. Allocation
    /// of the shared group storage follows the allocator's failure policy.
    pub fn new(uid: Uid, gid: Gid) -> Self {
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
    /// Only the returned copy's filesystem IDs change to its real IDs. Effective,
    /// saved and real IDs, supplementary groups and securebits are preserved.
    /// This prepares identity data and does not itself perform a file access check.
    pub fn for_access(&self) -> Self {
        let mut cred = self.prepare();
        cred.fsuid = cred.ruid;
        cred.fsgid = cred.rgid;
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
    /// Persists the flag on this credential. The capability-set fixup that
    /// consumes this state will be added together with capability sets. The
    /// bit is already process state: `PR_GET_KEEPCAPS` observes it, and exec
    /// clears it.
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
    pub fn in_group(&self, gid: Gid) -> bool {
        self.fsgid == gid || self.supplementary_groups.binary_search(&gid).is_ok()
    }

    /// Returns whether the process is privileged for set-ID operations.
    ///
    /// This is the pre-capabilities model used by x-kernel: effective UID 0
    /// stands in for Linux `CAP_SETUID` and `CAP_SETGID`.
    pub fn is_privileged(&self) -> bool {
        self.euid == 0
    }

    /// Returns this credential's `CAP_SYS_ADMIN` authorization decision.
    ///
    /// Until capability sets are implemented, effective UID 0 supplies this
    /// authority. Real, saved and filesystem IDs do not grant it. Consumers
    /// must use the current caller's subjective credential, not an opener's
    /// snapshot. This predicate is independent of the set-ID privilege policy.
    pub fn has_sys_admin(&self) -> bool {
        self.euid == 0
    }

    /// Returns whether this credential's real, effective, and saved IDs all
    /// match the real user and group IDs of `caller`.
    ///
    /// This is the credential-only identity predicate used by Linux
    /// `PTRACE_MODE_REALCREDS` checks. Filesystem IDs and supplementary groups
    /// do not participate in this comparison.
    pub fn matches_real_credential_ids(&self, caller: &Self) -> bool {
        let caller_uid = caller.ruid;
        let caller_gid = caller.rgid;

        self.ruid == caller_uid
            && self.euid == caller_uid
            && self.suid == caller_uid
            && self.rgid == caller_gid
            && self.egid == caller_gid
            && self.sgid == caller_gid
    }

    /// Implements Linux/POSIX `setuid`.
    ///
    /// A privileged process sets real, effective, saved, and filesystem UIDs.
    /// An unprivileged process may set only its effective/filesystem UID, and
    /// only to its current real or saved set-user ID.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] if the credential is not privileged
    /// and the requested UID matches neither the real nor saved set-user ID.
    /// No fields change on rejection.
    pub fn set_uid(&mut self, uid: Uid) -> KResult<()> {
        if self.is_privileged() {
            self.set_resuid_unchecked(Some(uid), Some(uid), Some(uid));
            return Ok(());
        }

        if uid != self.ruid && uid != self.suid {
            return Err(KError::OperationNotPermitted);
        }

        self.euid = uid;
        self.fsuid = uid;
        Ok(())
    }

    /// Implements Linux/POSIX `setgid`.
    ///
    /// A privileged process sets real, effective, saved, and filesystem GIDs.
    /// An unprivileged process may set only its effective/filesystem GID, and
    /// only to its current real or saved set-group ID.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] if the credential is not privileged
    /// and the requested GID matches neither the real nor saved set-group ID.
    /// No fields change on rejection.
    pub fn set_gid(&mut self, gid: Gid) -> KResult<()> {
        if self.is_privileged() {
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
        let old_ruid = self.ruid;
        let old_euid = self.euid;

        if !self.is_privileged() {
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

        if !self.is_privileged() {
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
        if !self.is_privileged() {
            self.check_uid_change(ruid)?;
            self.check_uid_change(euid)?;
            self.check_uid_change(suid)?;
        }

        if self.is_resuid_noop(ruid, euid, suid) {
            return Ok(());
        }

        self.set_resuid_unchecked(ruid, euid, suid);
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
        if !self.is_privileged() {
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
    /// A privileged credential accepts any target; otherwise the target must
    /// match its real, effective, saved or current filesystem user ID.
    /// The returned old value does not indicate whether the change succeeded.
    pub fn set_fsuid(&mut self, fsuid: Uid) -> Uid {
        let old_fsuid = self.fsuid;
        if self.is_privileged()
            || fsuid == self.ruid
            || fsuid == self.euid
            || fsuid == self.suid
            || fsuid == old_fsuid
        {
            self.fsuid = fsuid;
        }
        old_fsuid
    }

    /// Implements Linux `setfsgid`.
    ///
    /// The old filesystem GID is always returned. If the requested GID is not
    /// permitted, the credential state is left unchanged.
    ///
    /// A privileged credential accepts any target; otherwise the target must
    /// match its real, effective, saved or current filesystem group ID.
    /// The returned old value does not indicate whether the change succeeded.
    pub fn set_fsgid(&mut self, fsgid: Gid) -> Gid {
        let old_fsgid = self.fsgid;
        if self.is_privileged()
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

    /// Resets saved/filesystem IDs and clears keep-capabilities after successful exec.
    ///
    /// Saved and filesystem IDs become the effective IDs; real IDs and
    /// supplementary groups remain unchanged. `KEEP_CAPS_LOCKED` remains set if it was set, while
    /// `KEEP_CAPS` is cleared regardless of that lock. This method does not
    /// inspect an executable, apply setuid/setgid file bits, or publish credentials.
    ///
    /// `no_new_privileges` carries the caller's `PR_SET_NO_NEW_PRIVS` state:
    /// with the flag set, exec must neither elevate nor grant credentials
    /// (Linux ignores setuid/setgid file bits and file capabilities). Exec
    /// currently never elevates identity, so the reset below already
    /// satisfies the contract; future setuid/setgid or file-capability
    /// support must consult this flag here and refuse the transition
    /// instead of elevating (see `core/ksyscall/docs/security.md`).
    pub fn apply_exec(&mut self, no_new_privs: bool) {
        let _ = no_new_privs;
        // Future setuid/setgid executable support must update euid/egid before this reset.
        self.suid = self.euid;
        self.fsuid = self.euid;
        self.sgid = self.egid;
        self.fsgid = self.egid;
        // Keep-capabilities does not survive exec.
        self.securebits.remove(SecureBits::KEEP_CAPS);
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
