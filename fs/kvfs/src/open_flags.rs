// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File open flag normalization.

use linux_raw_sys::general::{
    FASYNC, O_ACCMODE, O_APPEND, O_CLOEXEC, O_CREAT, O_DIRECT, O_DIRECTORY, O_DSYNC, O_EXCL,
    O_LARGEFILE, O_NOATIME, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK, O_PATH, O_RDONLY, O_RDWR, O_SYNC,
    O_TRUNC, O_WRONLY,
};

use crate::{LookupFlags, NodePermission, NodeType, Permission, Umode, VfsError, VfsResult};

bitflags::bitflags! {
    /// Normalized Linux `O_*` bits used inside the VFS open path.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct OpenFlags: u32 {
        /// Open for writing only.
        const WRITE_ONLY = O_WRONLY;
        /// Open for reading and writing.
        const READ_WRITE = O_RDWR;
        /// Create the file when it does not exist.
        const CREATE = O_CREAT;
        /// Require exclusive creation.
        const EXCLUSIVE = O_EXCL;
        /// Do not assign a controlling terminal.
        const NO_CONTROLLING_TTY = O_NOCTTY;
        /// Truncate a regular file after opening it.
        const TRUNCATE = O_TRUNC;
        /// Append writes to the end of the file.
        const APPEND = O_APPEND;
        /// Use nonblocking I/O semantics.
        const NONBLOCK = O_NONBLOCK;
        /// Request data-integrity synchronized writes.
        const DSYNC = O_DSYNC;
        /// Enable signal-driven I/O.
        const ASYNC = FASYNC;
        /// Request direct I/O.
        const DIRECT = O_DIRECT;
        /// Allow large-file offsets.
        const LARGE_FILE = O_LARGEFILE;
        /// Require the opened object to be a directory.
        const DIRECTORY = O_DIRECTORY;
        /// Do not follow the final symbolic link.
        const NO_FOLLOW = O_NOFOLLOW;
        /// Do not update access time.
        const NO_ATIME = O_NOATIME;
        /// Close the descriptor during exec.
        const CLOSE_ON_EXEC = O_CLOEXEC;
        /// Request file-integrity synchronized writes.
        const SYNC = O_SYNC;
        /// Open only a path reference.
        const PATH = O_PATH;
    }

    /// Namei intent bits derived from open flags.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub(crate) struct OpenIntent: u32 {
        const OPEN = 1 << 16;
        const CREATE = 1 << 17;
        const EXCL = 1 << 18;
    }
}

const O_PATH_FLAGS: OpenFlags = OpenFlags::DIRECTORY
    .union(OpenFlags::NO_FOLLOW)
    .union(OpenFlags::PATH)
    .union(OpenFlags::CLOSE_ON_EXEC);
const VALID_OPEN_FLAGS: u32 = OpenFlags::all().bits();

/// `OPENAT2_REGULAR`: the resolved object must be a regular file.
///
/// The low 32 bits of `open_how.flags` are the Linux `O_*` namespace; this
/// project-level flag lives in the upper half so it cannot collide with them.
pub const OPENAT2_REGULAR: u64 = 1u64 << 32;
/// Every raw `open_how.flags` bit this kernel accepts.
const VALID_OPENAT2_FLAGS: u64 = VALID_OPEN_FLAGS as u64 | OPENAT2_REGULAR;

fn is_create_like_open(flags: OpenFlags) -> bool {
    flags.contains(OpenFlags::CREATE)
}

bitflags::bitflags! {
    /// `openat2(2)` `open_how.resolve` policy (`uapi/linux/openat2.h`).
    ///
    /// The ABI word is `u64`; this is the validated in-kernel form. Converting
    /// with [`ResolveFlags::from_bits`] at the boundary rejects unknown bits
    /// instead of letting raw resolve bits reach the open algorithm.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct ResolveFlags: u64 {
        /// `RESOLVE_NO_XDEV`: do not cross mount boundaries.
        const NO_XDEV = 0x01;
        /// `RESOLVE_NO_MAGICLINKS`: do not follow magic links.
        const NO_MAGICLINKS = 0x02;
        /// `RESOLVE_NO_SYMLINKS`: do not follow symbolic links.
        const NO_SYMLINKS = 0x04;
        /// `RESOLVE_BENEATH`: keep the walk below the dirfd subtree.
        const BENEATH = 0x08;
        /// `RESOLVE_IN_ROOT`: treat the dirfd subtree as the root.
        const IN_ROOT = 0x10;
        /// `RESOLVE_CACHED`: only cached lookups, otherwise `EAGAIN`.
        const CACHED = 0x20;
    }
}

/// The `flags`/`mode`/`resolve` triple of `openat2(2)`'s `struct open_how`,
/// split out so the syscall layer does not need to know the internal layout.
///
/// This is the raw ABI carrier: Linux declares all three `open_how` fields as
/// `__u64`, so they stay `u64` here (the upper half of `flags` carries project
/// extensions such as [`OPENAT2_REGULAR`]). Validation converts them into the
/// in-kernel semantic types (`OpenFlags`, `NodePermission`, [`ResolveFlags`])
/// before namei consumes them.
#[derive(Clone, Copy, Debug)]
pub struct Openat2How {
    /// `open_how.flags` (`O_*` bits plus project flags in the upper half).
    pub flags: u64,
    /// `open_how.mode` (creation permission bits).
    pub mode: u64,
    /// `open_how.resolve` (`RESOLVE_*` bits).
    pub resolve: u64,
}

impl Openat2How {
    /// Validates the raw `open_how` fields with the same rules namei applies.
    ///
    /// The syscall layer calls this before it resolves the directory fd, so a
    /// malformed request reports `EINVAL` rather than an fd error.
    pub fn validate(self) -> VfsResult<Self> {
        OpenHow::from_openat2(self.flags, self.mode, self.resolve)?;
        Ok(self)
    }
}

/// Raw open arguments after legacy syscall cleanup.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OpenHow {
    pub(crate) flags: u64,
    pub(crate) mode: NodePermission,
    /// Validated `openat2` resolve policy; empty for legacy `open`/`openat`.
    pub(crate) resolve: ResolveFlags,
    /// `OPENAT2_REGULAR`: require the resolved object to be a regular file.
    pub(crate) regular_only: bool,
}

impl OpenHow {
    /// Builds open arguments from legacy `open`/`openat` syscall inputs.
    pub(crate) fn from_legacy(flags: u32, mode: NodePermission) -> Self {
        let mut how = Self {
            flags: u64::from(flags & VALID_OPEN_FLAGS),
            mode,
            resolve: ResolveFlags::empty(),
            regular_only: false,
        };
        if OpenFlags::from_bits_retain(how.flags as u32).contains(OpenFlags::PATH) {
            how.flags &= u64::from(O_PATH_FLAGS.bits());
        }
        if !is_create_like_open(OpenFlags::from_bits_retain(how.flags as u32)) {
            how.mode = NodePermission::empty();
        }
        how
    }

    /// Validates raw `openat2(2)` arguments (`fs/open.c build_open_flags()`).
    ///
    /// `flags`, `mode` and `resolve` come straight from `struct open_how`.
    /// Unknown flag or resolve bits are `EINVAL`, a non-zero `mode` without
    /// `O_CREAT` is `EINVAL`, and the two subtree constraints
    /// (`RESOLVE_BENEATH` and `RESOLVE_IN_ROOT`) are mutually exclusive.
    pub(crate) fn from_openat2(flags: u64, mode: u64, resolve: u64) -> VfsResult<Self> {
        // Check the whole 64-bit flag word, not a `u32` truncation: a flag this
        // kernel accepts may legitimately live above `O_*` (see
        // [`OPENAT2_REGULAR`]), and an unknown bit must be `EINVAL` because it
        // is unknown, not because it did not fit in 32 bits.
        if flags & !VALID_OPENAT2_FLAGS != 0 {
            return Err(VfsError::InvalidInput);
        }
        let raw_flags = (flags & u64::from(VALID_OPEN_FLAGS)) as u32;
        // Creation permissions are a mode, not a permission "hint": reject any
        // bit outside `0o7777` instead of silently truncating it (a truncated
        // `0o10000` used to create a zero-permission file).
        if mode & !0o7777 != 0 {
            return Err(VfsError::InvalidInput);
        }
        let mode_permission = NodePermission::from_bits_truncate(mode as u16);
        if !is_create_like_open(OpenFlags::from_bits_retain(raw_flags)) && mode != 0 {
            return Err(VfsError::InvalidInput);
        }
        let resolve = ResolveFlags::from_bits(resolve).ok_or(VfsError::InvalidInput)?;
        if resolve.contains(ResolveFlags::BENEATH) && resolve.contains(ResolveFlags::IN_ROOT) {
            return Err(VfsError::InvalidInput);
        }
        let regular_only = flags & OPENAT2_REGULAR != 0;
        // `OPENAT2_REGULAR` contradicts `O_PATH` (no file type is opened) and
        // `O_DIRECTORY` (a directory is required); Linux rejects both with
        // `EINVAL` rather than honoring the combination.
        if regular_only && raw_flags & (O_PATH | O_DIRECTORY) != 0 {
            return Err(VfsError::InvalidInput);
        }
        // Contradictory flag combinations are `EINVAL` in Linux
        // `build_open_flags`, and that check precedes the `RESOLVE_CACHED`
        // rule, so it must run before the `EAGAIN` below.
        let open_flags = OpenFlags::from_bits_retain(raw_flags);
        if open_flags.contains(OpenFlags::DIRECTORY | OpenFlags::CREATE) {
            return Err(VfsError::InvalidInput);
        }
        if open_flags.contains(OpenFlags::PATH) && !O_PATH_FLAGS.contains(open_flags) {
            return Err(VfsError::InvalidInput);
        }
        // `RESOLVE_CACHED` forbids side effects; Linux `build_open_flags`
        // rejects create/truncate before the pathname or dirfd is touched.
        if resolve.contains(ResolveFlags::CACHED) && raw_flags & (O_CREAT | O_TRUNC) != 0 {
            return Err(VfsError::WouldBlock);
        }
        Ok(Self {
            flags: u64::from(raw_flags),
            mode: mode_permission,
            resolve,
            regular_only,
        })
    }

    /// Converts raw open arguments into normalized parameters for namei.
    pub(crate) fn into_open_params(self) -> VfsResult<OpenParams> {
        let raw_flags = u32::try_from(self.flags).map_err(|_| VfsError::InvalidInput)?;
        let mut acc_mode = AccMode::from_access_mode(raw_flags & O_ACCMODE);

        let mut flags = OpenFlags::from_bits(raw_flags).ok_or(VfsError::InvalidInput)?;

        flags.remove(OpenFlags::CLOSE_ON_EXEC);

        let mode = if is_create_like_open(flags) {
            Umode::new(NodeType::RegularFile, self.mode)
        } else {
            if !self.mode.is_empty() {
                return Err(VfsError::InvalidInput);
            }
            Umode::new(NodeType::RegularFile, NodePermission::empty())
        };

        if flags.contains(OpenFlags::DIRECTORY | OpenFlags::CREATE) {
            return Err(VfsError::InvalidInput);
        }

        if flags.contains(OpenFlags::PATH) {
            if !O_PATH_FLAGS.contains(flags) {
                return Err(VfsError::InvalidInput);
            }
            acc_mode = AccMode::empty();
        }

        if flags.contains(OpenFlags::SYNC) {
            flags.insert(OpenFlags::DSYNC);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            acc_mode.insert_write();
        }
        if flags.contains(OpenFlags::APPEND) {
            acc_mode.insert_append();
        }

        let mut intent = if flags.contains(OpenFlags::PATH) {
            OpenIntent::empty()
        } else {
            OpenIntent::OPEN
        };
        if flags.contains(OpenFlags::CREATE) {
            intent |= OpenIntent::CREATE;
            if flags.contains(OpenFlags::EXCLUSIVE) {
                intent |= OpenIntent::EXCL;
                flags.insert(OpenFlags::NO_FOLLOW);
            }
        }

        let mut lookup_flags = LookupFlags::empty();
        if flags.contains(OpenFlags::DIRECTORY) {
            lookup_flags |= LookupFlags::DIRECTORY;
        }
        if !flags.contains(OpenFlags::NO_FOLLOW) {
            lookup_flags |= LookupFlags::FOLLOW_FINAL;
        }
        // `openat2(2)` resolve constraints. `Nameidata` owns the anchored
        // subtree semantics for `IN_ROOT`/`BENEATH`; the syscall layer only
        // selects the directory fd as the walk base.
        if self.resolve.contains(ResolveFlags::NO_XDEV) {
            lookup_flags |= LookupFlags::NO_XDEV;
        }
        if self.resolve.contains(ResolveFlags::NO_MAGICLINKS) {
            lookup_flags |= LookupFlags::NO_MAGIC_LINKS;
        }
        if self.resolve.contains(ResolveFlags::NO_SYMLINKS) {
            lookup_flags |= LookupFlags::NO_SYMLINKS;
        }
        if self.resolve.contains(ResolveFlags::BENEATH) {
            lookup_flags |= LookupFlags::BENEATH;
        }
        if self.resolve.contains(ResolveFlags::IN_ROOT) {
            lookup_flags |= LookupFlags::IN_ROOT;
        }
        // `RESOLVE_CACHED` is a real constraint: the walk may only complete
        // from cached state. `Nameidata` turns a slow lookup (or a request
        // with side effects such as create/truncate) into `EAGAIN`.
        if self.resolve.contains(ResolveFlags::CACHED) {
            lookup_flags |= LookupFlags::CACHED;
        }

        Ok(OpenParams {
            flags,
            mode,
            acc_mode,
            intent,
            lookup_flags,
            regular_only: self.regular_only,
        })
    }
}

/// Open parameters consumed by namei.
#[derive(Clone, Debug)]
pub(crate) struct OpenParams {
    flags: OpenFlags,
    mode: Umode,
    acc_mode: AccMode,
    intent: OpenIntent,
    lookup_flags: LookupFlags,
    /// `OPENAT2_REGULAR`: the resolved object must be a regular file.
    regular_only: bool,
}

impl OpenParams {
    pub(crate) const fn will_create(&self) -> bool {
        self.intent.contains(OpenIntent::CREATE)
    }

    pub(crate) const fn is_exclusive_create(&self) -> bool {
        self.intent.contains(OpenIntent::EXCL)
    }

    pub(crate) const fn mode(&self) -> Umode {
        self.mode
    }

    pub(crate) const fn lookup_flags(&self) -> LookupFlags {
        self.lookup_flags
    }

    pub(crate) const fn requires_regular(&self) -> bool {
        self.regular_only
    }

    pub(crate) const fn is_path(&self) -> bool {
        self.flags.contains(OpenFlags::PATH)
    }

    pub(crate) const fn file_flags(&self) -> OpenFlags {
        self.flags
    }

    pub(crate) fn may_open_args(&self, was_created: bool) -> (OpenFlags, AccMode) {
        let mut open_flags = self.flags;
        let mut acc_mode = self.acc_mode;
        if was_created {
            open_flags.remove(OpenFlags::TRUNCATE);
            acc_mode = AccMode::empty();
        }
        (open_flags, acc_mode)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccMode(Permission);

impl AccMode {
    pub(crate) const fn empty() -> Self {
        Self(Permission::empty())
    }

    fn from_access_mode(access_mode: u32) -> Self {
        match access_mode {
            O_RDONLY => Self(Permission::MAY_READ),
            O_WRONLY => Self(Permission::MAY_WRITE),
            O_RDWR => Self(Permission::MAY_READ | Permission::MAY_WRITE),
            _ => Self(Permission::MAY_READ | Permission::MAY_WRITE),
        }
    }

    fn insert_write(&mut self) {
        self.0 |= Permission::MAY_WRITE;
    }

    fn insert_append(&mut self) {
        self.0 |= Permission::MAY_APPEND;
    }

    pub(crate) const fn requires_write(self) -> bool {
        self.0.contains(Permission::MAY_WRITE)
    }

    pub(crate) const fn permission(self) -> Permission {
        self.0
    }
}

unittest::path_mod_test!(tests, "../ut-tests/open_flags.rs");
