// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Devpts pseudo-terminal filesystem.
//!
//! Each mount is its own instance: it exposes the `ptmx` master multiplexer
//! and the slave nodes of the pty pairs allocated through that instance, so
//! the `open("/dev/ptmx")` → `open("/dev/pts/<n>")` flow works entirely
//! inside the mount — the shape runc's default OCI spec expects when it
//! mounts `devpts` with `newinstance` at `<rootfs>/dev/pts`.
//!
//! Slave pty node construction stays with the shared devfs PTY driver
//! (`ktty`); the root directory projects the live slaves of the pairs that
//! were allocated through this mount (the mount's filesystem is the owner
//! those queries are scoped by). Mount options follow Linux
//! `fs/devpts/inode.c`: `uid=`, `gid=`, `mode=` (slave ptys), `ptmxmode=`
//! (the `ptmx` node), `newinstance` (inherent — every mount is a fresh
//! instance) and `max=`, which is parsed and range-checked exactly like
//! Linux but cannot take effect: pty numbers come from one global allocator
//! that already bounds every instance.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

mod root;

use alloc::sync::Arc;

use kvfs::{
    FileSystemType, FsContext, FsContextOperations, MetadataUpdate, NodePermission, SimpleFs,
    SuperBlock, SuperBlockFlags, VfsResult, get_tree_nodev,
};

/// `DEVPTS_SUPER_MAGIC` (Linux `uapi/linux/magic.h`: 0x1cd1).
const DEVPTS_SUPER_MAGIC: u32 = 0x1CD1;

/// `DEVPTS_DEFAULT_MODE`: slave pty mode (Linux `fs/devpts/inode.c`).
const DEFAULT_SLAVE_MODE: u16 = 0o600;
/// `DEVPTS_DEFAULT_PTMX_MODE`: `ptmx` node mode (Linux `fs/devpts/inode.c`).
const DEFAULT_PTMX_MODE: u16 = 0o000;

/// Parsed `devpts` mount options (Linux `struct mount_opts`).
#[derive(Clone, Copy)]
struct MountOptions {
    uid: u32,
    set_uid: bool,
    gid: u32,
    set_gid: bool,
    /// Mode of newly created slave pty nodes.
    slave_mode: u16,
    /// Mode of the `ptmx` node.
    ptmx_mode: u16,
    /// The data-level `ro`/`rw` tokens (Linux `vfs_parse_sb_flag` writing
    /// `SB_RDONLY`): `true` after `ro`, `false` after `rw`, `None` when
    /// neither appeared. Applied to the superblock flags in `get_tree` so a
    /// data-only `ro` does not yield a writable mount.
    data_rdonly: Option<bool>,
}

impl MountOptions {
    /// Linux defaults: root ownership, slave mode `0600`, `ptmxmode` `0000`.
    const fn defaults() -> Self {
        Self {
            uid: 0,
            set_uid: false,
            gid: 0,
            set_gid: false,
            slave_mode: DEFAULT_SLAVE_MODE,
            ptmx_mode: DEFAULT_PTMX_MODE,
            data_rdonly: None,
        }
    }

    /// Parses the mount data the way Linux `devpts_parse_param` does: comma
    /// separated, `mode`/`ptmxmode` in octal, unknown or malformed options
    /// rejected with `EINVAL`.
    fn parse(data: Option<&[u8]>) -> VfsResult<Self> {
        let mut options = Self::defaults();
        let Some(data) = data else {
            return Ok(options);
        };
        // A legacy `mount(2)` data buffer is a fixed-size, NUL-padded block:
        // `posix/fs` copies `LEGACY_MOUNT_DATA_SIZE` bytes and zeroes only the
        // last one, so everything after the first NUL is padding rather than
        // option text. Parse the C string, as the other backends do;
        // otherwise the last option carries a trail of NULs and fails.
        let data = &data[..data
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(data.len())];
        let data = core::str::from_utf8(data).map_err(|_| kvfs::VfsError::InvalidInput)?;
        for part in data.split(',') {
            if part.is_empty() {
                continue;
            }
            if part == "newinstance" {
                // Inherent: every mount of this filesystem is a fresh
                // instance with its own pty set.
                continue;
            }
            if matches!(
                part,
                "dirsync" | "lazytime" | "mand" | "sync" | "async" | "nolazytime" | "nomand"
            ) {
                // VFS-level sb-flag tokens without a read-only effect:
                // `vfs_parse_sb_flag()` (`fs/fs_context.c`) consumes them
                // before the filesystem parser sees them, so Linux's devpts
                // accepts them in the data. This kernel carries them on the
                // mount(2) flags argument instead; accepted here for
                // mount-outcome parity.
                continue;
            }
            if part == "ro" || part == "rw" {
                // `ro`/`rw` write `SB_RDONLY` on Linux; honour them at the
                // superblock level so a data-only `ro` does not yield a
                // writable mount. Later tokens win, like the |=/~= pairs.
                options.data_rdonly = Some(part == "ro");
                continue;
            }
            let Some((key, value)) = part.split_once('=') else {
                return Err(kvfs::VfsError::InvalidInput);
            };
            if key.is_empty() {
                // Linux's parser silently skips an option whose key is empty
                // (a token like `=` carries no parameter name).
                continue;
            }
            match key {
                "uid" => {
                    options.uid = reject_unmappable_id(parse_base0(value)?)?;
                    options.set_uid = true;
                }
                "gid" => {
                    options.gid = reject_unmappable_id(parse_base0(value)?)?;
                    options.set_gid = true;
                }
                "mode" => {
                    options.slave_mode = parse_mode(value)?;
                }
                "ptmxmode" => {
                    options.ptmx_mode = parse_mode(value)?;
                }
                // Linux bounds per-instance pty allocation with `max=` and
                // rejects only out-of-range values; see `validate_max`.
                "max" => validate_max(value)?,
                _ => return Err(kvfs::VfsError::InvalidInput),
            }
        }
        Ok(options)
    }
}

/// Parses a numeric mount option value the way Linux's option parser does.
///
/// `uid=`, `gid=` and `max=` are `fsparam_uid`/`fsparam_gid`/`fsparam_s32`,
/// which convert with no explicit base, so the string is read base 0: a `0x`
/// prefix is hexadecimal and a leading `0` is octal (`fs/fs_parser.c`).
fn parse_base0(value: &str) -> VfsResult<u32> {
    // Linux's `kstrto*` converters tolerate exactly one trailing newline
    // (`kstrtouint` strips it before parsing), so a final `uid=1000\n` in the
    // mount data is accepted there; strip it for the same acceptance set.
    let value = value.strip_suffix('\n').unwrap_or(value);
    // Linux's `kstrtouint(value, 0, ..)` takes at most one leading `+` before
    // it looks for the base prefix (`-` is rejected for an unsigned
    // conversion), so `+010` is octal 8 and `+0x10` is hexadecimal 16, while
    // `0x+10` is not a number. Strip the sign first, then the prefix, and
    // never let a sign survive into the digits.
    let value = value.strip_prefix('+').unwrap_or(value);
    let (digits, radix) = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => (hex, 16),
        None if value.len() > 1 && value.starts_with('0') => (&value[1..], 8),
        None => (value, 10),
    };
    if digits.is_empty() || digits.starts_with(['+', '-']) {
        return Err(kvfs::VfsError::InvalidInput);
    }
    u32::from_str_radix(digits, radix).map_err(|_| kvfs::VfsError::InvalidInput)
}

/// `NR_UNIX98_PTY_MAX` (Linux `include/linux/tty.h`): the largest value
/// Linux's devpts parser accepts for `max=`.
const NR_UNIX98_PTY_MAX: u32 = 1 << 20;

/// Rejects the one id value no user namespace can map: `(uid_t)-1` /
/// `(gid_t)-1`.
///
/// Linux rounds `uid=`/`gid=` through `make_kuid()`/`make_kgid()` and fails the
/// mount with `EINVAL` when the result is `INVALID_UID`/`INVALID_GID`. This
/// kernel models no namespace id mapping (only the initial namespace is
/// exposed, see `kcred::UserNamespace`), and the initial namespace's map
/// covers `0..=4294967294` (`struct user_namespace init_user_ns`,
/// `kernel/user.c`), so `4294967295` is exactly the value that stays
/// unmappable here. Every other `u32` maps to itself; revisit this when id
/// maps land.
fn reject_unmappable_id(value: u32) -> VfsResult<u32> {
    if value == u32::MAX {
        return Err(kvfs::VfsError::InvalidInput);
    }
    Ok(value)
}

/// Validates `max=` the way Linux's parser does: a base-0 integer that is at
/// most `NR_UNIX98_PTY_MAX`; anything larger is `EINVAL`
/// (`fs/devpts/inode.c`: "max out of range" — the only rejection the reference
/// parser performs, so `max=0` mounts successfully there too and only leaves
/// the instance without its own allocation bound).
///
/// The value is not retained: Linux uses `max - 1` as the bound of the
/// instance's own pty-number ida, while this kernel numbers ptys from one
/// global allocator shared by every instance, so an instance-local bound has
/// nothing to apply to. Only the mount outcome (accept vs. `EINVAL`) can
/// match Linux here.
fn validate_max(value: &str) -> VfsResult<()> {
    // `max=` is Linux's only signed devpts option (`fsparam_s32` converted
    // by `kstrtoint`): one leading `-` is accepted and negated, so the
    // negative-zero spellings (`-0`, `-00`, `-0x0`) parse to zero and the
    // mount succeeds, while any other negative value fails the range check
    // through the `int_32`/`uint_32` union (`fs/devpts/inode.c`).
    // `parse_base0` applies the one-trailing-newline tolerance itself, so a
    // double newline stays rejected exactly like `kstrtoint`.
    let max = match value.strip_prefix('-') {
        Some(rest) => {
            let magnitude = if rest.starts_with('+') {
                return Err(kvfs::VfsError::InvalidInput);
            } else {
                parse_base0(rest)?
            };
            if magnitude != 0 {
                return Err(kvfs::VfsError::InvalidInput);
            }
            0
        }
        None => parse_base0(value)?,
    };
    if max > NR_UNIX98_PTY_MAX {
        return Err(kvfs::VfsError::InvalidInput);
    }
    Ok(())
}

/// Parses an octal permission value (`mode=`, `ptmxmode=`).
///
/// Linux parses both keys with `fsparam_u32oct`, i.e. `kstrtouint(value, 8)`
/// into a `u32`, and only then masks with `& S_IALLUGO`
/// (`fs/devpts/inode.c`). Any value that fits a `u32` is therefore a legal
/// mount option whose out-of-range bits are silently dropped — `mode=400000`
/// and even `mode=20000000000` mount with a zeroed permission set — while only
/// a `u32` overflow is `EINVAL`.
fn parse_mode(value: &str) -> VfsResult<u16> {
    // One trailing newline is tolerated, matching `kstrtouint` (see
    // `parse_base0`).
    let value = value.strip_suffix('\n').unwrap_or(value);
    let mode = u32::from_str_radix(value, 8).map_err(|_| kvfs::VfsError::InvalidInput)?;
    Ok((mode & 0o7777) as u16)
}

fn get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    let options = MountOptions::parse(context.data())?;
    let cred = context.cred();
    get_tree_nodev(context, |file_system_type, flags| {
        // A data-level `ro`/`rw` lands on the superblock like Linux's
        // `SB_RDONLY` (`vfs_parse_sb_flag` sets it on `ro` and clears it on
        // `rw`), on top of whatever the mount(2) flags argument carried.
        let flags = match options.data_rdonly {
            Some(true) => flags | SuperBlockFlags::RDONLY,
            Some(false) => flags - SuperBlockFlags::RDONLY,
            None => flags,
        };
        Ok(new_devpts_with_type(file_system_type, flags, options, cred))
    })
}

/// Applies a remount transaction to an existing devpts instance.
///
/// Linux `devpts_reconfigure()` copies the parsed options onto the mount and
/// calls `update_ptmx_mode()`, so the new `ptmxmode=` is visible on the live
/// `ptmx` node at once while the rest (slave mode, owner, `max=`) applies to
/// slaves created after the remount. The live node is reached through the
/// published superblock, so no extra VFS hook is needed.
fn devpts_reconfigure(context: &mut FsContext<'_>) -> VfsResult<()> {
    let options = MountOptions::parse(context.data())?;
    // A data-level `ro`/`rw` lands on the superblock on remount too, like
    // Linux's `vfs_parse_sb_flag`: extend the applied mask with RDONLY and
    // set/clear the proposed bit (`SuperBlock::reconfigure` computes the
    // stored flags after this callback returns).
    if let Some(ro) = options.data_rdonly {
        context.set_sb_flags_mask(context.sb_flags_mask() | SuperBlockFlags::RDONLY);
        let mut sb_flags = context.sb_flags();
        if ro {
            sb_flags |= SuperBlockFlags::RDONLY;
        } else {
            sb_flags -= SuperBlockFlags::RDONLY;
        }
        context.set_sb_flags(sb_flags);
    }
    let super_block = context.super_block()?;
    let fs = super_block.private::<Arc<SimpleFs>>()?;
    let shared = fs
        .private::<ksync::Mutex<MountOptions>>()
        .ok_or(kvfs::VfsError::InvalidInput)?;
    *shared.lock() = options;
    let ptmx = super_block.root_dir().lookup(root::PTMX_NAME)?;
    ptmx.update_metadata(MetadataUpdate {
        mode: Some(NodePermission::from_bits_truncate(options.ptmx_mode)),
        ..MetadataUpdate::default()
    })?;
    Ok(())
}

static FS_CONTEXT_OPERATIONS: FsContextOperations =
    FsContextOperations::with_reconfigure(get_tree, devpts_reconfigure);

fn init_fs_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&FS_CONTEXT_OPERATIONS);
    Ok(())
}

/// Registered devpts filesystem type.
pub(crate) static FILE_SYSTEM_TYPE: FileSystemType =
    FileSystemType::nodev("devpts", init_fs_context);

/// Registers the devpts filesystem type during kernel initialization.
#[macros::register_init]
fn init_devpts_fs() {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Each statically named filesystem is \
                  registered once by its unique linker initialization entry"
    )]
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE)
        .expect("devpts filesystem type must register once");
}

fn new_devpts_with_type(
    file_system_type: &'static FileSystemType,
    flags: SuperBlockFlags,
    options: MountOptions,
    cred: &kcred::Cred,
) -> Arc<SuperBlock> {
    SimpleFs::new_with_superblock_flags(file_system_type, DEVPTS_SUPER_MAGIC, flags, |fs| {
        root::builder(fs, options, cred)
    })
}

unittest::path_mod_test!(tests, "../ut-tests/devpts.rs");
