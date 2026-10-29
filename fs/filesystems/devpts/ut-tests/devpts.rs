// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kvfs::{FsContext, SuperBlockFlags};
use unittest::def_test;

use super::{MountOptions, FILE_SYSTEM_TYPE, DEVPTS_SUPER_MAGIC, new_devpts_with_type};

#[def_test]
fn filesystem_type_is_registered_as_devpts() {
    assert_eq!(FILE_SYSTEM_TYPE.name(), "devpts");
}

/// Linux `include/uapi/linux/magic.h`: `#define DEVPTS_SUPER_MAGIC 0x1cd1`.
/// `statfs`/`fstatfs` report this as `f_type`, and mount tools identify the
/// filesystem by it.
#[def_test]
fn superblock_magic_matches_linux() {
    assert_eq!(DEVPTS_SUPER_MAGIC, 0x1cd1);
}

#[def_test]
fn no_mount_data_yields_linux_defaults() {
    let options = MountOptions::parse(None).unwrap();
    assert_eq!(options.slave_mode, 0o600);
    assert_eq!(options.ptmx_mode, 0o000);
    assert!(!options.set_uid);
    assert!(!options.set_gid);
}

#[def_test]
fn container_style_options_parse_fully() {
    // The option set runc's default OCI spec mounts devpts with.
    let options = MountOptions::parse(Some(b"gid=5,mode=620,newinstance,ptmxmode=0666")).unwrap();
    assert_eq!(options.gid, 5);
    assert!(options.set_gid);
    assert_eq!(options.slave_mode, 0o620);
    assert_eq!(options.ptmx_mode, 0o666);
}

#[def_test]
fn uid_and_mode_masks_follow_linux() {
    // `mode=` is octal and masked with S_IALLUGO; `uid=` is decimal.
    let options = MountOptions::parse(Some(b"uid=1000,mode=07777")).unwrap();
    assert_eq!(options.uid, 1000);
    assert!(options.set_uid);
    assert_eq!(options.slave_mode, 0o7777);
}

#[def_test]
fn out_of_range_mode_bits_are_dropped_not_rejected() {
    // `fsparam_u32oct` accepts any value that fits a u32 before `& S_IALLUGO`,
    // so a redundant high bit is a legal mount that keeps only the low bits —
    // `mode=20000000000` is 2^31, still inside the u32 the parser converts to.
    let options = MountOptions::parse(Some(b"mode=400000,ptmxmode=10000")).unwrap();
    assert_eq!(options.slave_mode, 0o000);
    assert_eq!(options.ptmx_mode, 0o000);
    let options = MountOptions::parse(Some(b"mode=1006420")).unwrap();
    assert_eq!(options.slave_mode, 0o6420);
    let options = MountOptions::parse(Some(b"mode=20000000000")).unwrap();
    assert_eq!(options.slave_mode, 0o000);
    // A value that overflows the u32 conversion is EINVAL, as in Linux.
    assert!(MountOptions::parse(Some(b"mode=100000000000")).is_err());
}

#[def_test]
fn uid_and_gid_follow_linux_base_zero() {
    // `fsparam_uid`/`fsparam_gid` convert base 0: a `0x` prefix is hex and a
    // leading `0` is octal, so `010` is eight, not ten.
    assert_eq!(MountOptions::parse(Some(b"uid=010")).unwrap().uid, 8);
    assert_eq!(MountOptions::parse(Some(b"gid=0x10")).unwrap().gid, 16);
    assert_eq!(MountOptions::parse(Some(b"uid=1000")).unwrap().uid, 1000);
    assert!(MountOptions::parse(Some(b"uid=0x")).is_err());
    assert!(MountOptions::parse(Some(b"gid=-1")).is_err());
    // `(uid_t)-1` is the one value no user namespace maps (the initial one
    // covers 0..=4294967294), so Linux fails the mount with `EINVAL`.
    assert!(MountOptions::parse(Some(b"uid=4294967295")).is_err());
    assert!(MountOptions::parse(Some(b"gid=4294967295")).is_err());
    assert_eq!(
        MountOptions::parse(Some(b"uid=4294967294")).unwrap().uid,
        4294967294
    );
}

#[def_test]
fn mount_data_is_parsed_up_to_its_first_nul() {
    // `mount(2)` hands a fixed-size, NUL-padded block to the filesystem
    // (`posix/fs` zeroes only its last byte), so the trailing padding must not
    // reach the option parser: otherwise the last option fails and an ordinary
    // container mount returns EINVAL.
    let text = b"newinstance,ptmxmode=0666,mode=0620,gid=5";
    let mut data = [0u8; 4096];
    data[..text.len()].copy_from_slice(text);
    let options = MountOptions::parse(Some(&data)).expect("padded mount data parses");
    assert!(options.set_gid);
    assert_eq!(options.gid, 5);
    assert_eq!(options.slave_mode, 0o620);
    assert_eq!(options.ptmx_mode, 0o666);
}

#[def_test]
fn a_sign_before_the_base_prefix_is_handled_like_linux() {
    // `kstrtouint(.., 0, ..)` accepts one leading `+` before the prefix.
    assert_eq!(MountOptions::parse(Some(b"uid=+010")).unwrap().uid, 8);
    assert_eq!(MountOptions::parse(Some(b"uid=+0x10")).unwrap().uid, 16);
    // A sign after the prefix is not a number.
    assert!(MountOptions::parse(Some(b"uid=0x+10")).is_err());
    assert!(MountOptions::parse(Some(b"uid=+")).is_err());
}

#[def_test]
fn max_is_accepted_within_the_linux_range() {
    // Linux accepts `max=` in `0..=NR_UNIX98_PTY_MAX` — the parser only
    // rejects out-of-range values ("max out of range"), so `max=0` mounts and
    // merely leaves the instance without its own allocation bound
    // (`fs/devpts/inode.c`); the global pty allocator bounds every instance
    // regardless of the requested value.
    assert!(MountOptions::parse(Some(b"max=0")).is_ok());
    assert!(MountOptions::parse(Some(b"max=1")).is_ok());
    assert!(MountOptions::parse(Some(b"max=4096")).is_ok());
    assert!(MountOptions::parse(Some(b"max=1048576")).is_ok());
    // Base 0, like the rest of the parser.
    assert!(MountOptions::parse(Some(b"max=0x1000")).is_ok());
    // `max=` is Linux's only signed option (`fsparam_s32`): the
    // negative-zero spellings parse to zero and mount.
    assert!(MountOptions::parse(Some(b"max=-0")).is_ok());
    assert!(MountOptions::parse(Some(b"max=-00")).is_ok());
    assert!(MountOptions::parse(Some(b"max=-0x0")).is_ok());
    // Any other negative value fails the range check through the union.
    assert!(MountOptions::parse(Some(b"max=-1")).is_err());
    assert!(MountOptions::parse(Some(b"max=-00x0")).is_err());
    assert!(MountOptions::parse(Some(b"max=--0")).is_err());
    assert!(MountOptions::parse(Some(b"max=-+0")).is_err());
    // Out of range or malformed, exactly as Linux rejects them.
    assert!(MountOptions::parse(Some(b"max=1048577")).is_err());
    assert!(MountOptions::parse(Some(b"max=x")).is_err());
}

#[def_test]
fn unknown_options_are_rejected() {
    // Unknown options fail like Linux's param parser.
    assert!(MountOptions::parse(Some(b"bogus")).is_err());
    assert!(MountOptions::parse(Some(b"mode=600,extra=1")).is_err());
    // Malformed values are rejected.
    assert!(MountOptions::parse(Some(b"gid=x")).is_err());
    assert!(MountOptions::parse(Some(b"mode=abc")).is_err());
}

/// The VFS-level sb-flag tokens Linux consumes before the filesystem parser
/// (`vfs_parse_sb_flag`) are accepted; unknown value-less options are not.
#[def_test]
fn vfs_level_tokens_are_accepted_but_unknown_flags_are_not() {
    assert!(MountOptions::parse(Some(b"rw,mode=620")).is_ok());
    assert!(MountOptions::parse(Some(b"ro,ptmxmode=600")).is_ok());
    assert!(MountOptions::parse(Some(b"nosuid,mode=620")).is_err());
    assert!(MountOptions::parse(Some(b"noexec,mode=620")).is_err());
    assert!(MountOptions::parse(Some(b"nodev,mode=620")).is_err());
}

/// Numeric values tolerate exactly one trailing newline (Linux `kstrto*`)
/// and an empty option key is skipped silently, matching the reference
/// parser's handling of a bare `=` token.
/// The data-level `ro`/`rw` tokens land on the superblock read-only flag
/// (Linux `vfs_parse_sb_flag` writing `SB_RDONLY`), later token winning.
#[def_test]
fn data_level_ro_rw_is_recorded() {
    assert_eq!(
        MountOptions::parse(Some(b"ro,mode=620")).unwrap().data_rdonly,
        Some(true)
    );
    assert_eq!(
        MountOptions::parse(Some(b"mode=620,ro")).unwrap().data_rdonly,
        Some(true)
    );
    assert_eq!(
        MountOptions::parse(Some(b"ro,rw")).unwrap().data_rdonly,
        Some(false),
        "the later token wins, like the |=/~= pairs"
    );
    assert_eq!(
        MountOptions::parse(Some(b"mode=620")).unwrap().data_rdonly,
        None
    );
}

#[def_test]
fn parse_tolerates_trailing_newline_and_empty_key() {
    assert!(MountOptions::parse(Some(b"uid=1000\n")).is_ok());
    assert!(MountOptions::parse(Some(b"gid=5\n")).is_ok());
    assert!(MountOptions::parse(Some(b"mode=620\n")).is_ok());
    assert!(MountOptions::parse(Some(b"ptmxmode=600\n")).is_ok());
    assert!(MountOptions::parse(Some(b"max=4096\n")).is_ok());
    // One newline, not two and not one in the middle.
    assert!(MountOptions::parse(Some(b"uid=1000\n\n")).is_err());
    assert!(MountOptions::parse(Some(b"uid=10\n00")).is_err());
    // A bare `=` token carries no key and is skipped, not a failure.
    assert!(MountOptions::parse(Some(b"=,mode=620")).is_ok());
}

/// A remount applies `ptmxmode=` to the live `ptmx` node immediately (Linux
/// `devpts_reconfigure()` -> `update_ptmx_mode()`), instead of silently
/// succeeding while the node keeps its old mode.
#[def_test(serial)]
fn reconfigure_updates_the_live_ptmx_mode() {
    let fs = new_devpts_with_type(
        &FILE_SYSTEM_TYPE,
        SuperBlockFlags::empty(),
        MountOptions::parse(None).unwrap(),
        &kcred::initial_cred(),
    );
    let root = fs.root_dir();
    let ptmx_mode = || {
        root.lookup("ptmx")
            .expect("the instance has a ptmx node")
            .metadata()
            .mode
            .permission()
            .bits()
    };
    assert_eq!(ptmx_mode(), 0o000, "Linux's default ptmx mode");

    let cred = kcred::initial_cred();
    let mut context = FsContext::new_reconfigure(
        fs.as_ref(),
        None,
        Some(b"ptmxmode=0620"),
        SuperBlockFlags::empty(),
        SuperBlockFlags::empty(),
        &cred,
    )
    .expect("a remount context");
    fs.reconfigure(&mut context).expect("the remount applies");
    assert_eq!(ptmx_mode(), 0o620);
}

/// A data-level `ro`/`rw` lands on the superblock on remount, like Linux's
/// `vfs_parse_sb_flag` — the parser's `data_rdonly` record flows through
/// `devpts_reconfigure` into the context's proposed flags/mask.
#[def_test(serial)]
fn remount_data_ro_rw_lands_on_superblock_flags() {
    let fs = new_devpts_with_type(
        &FILE_SYSTEM_TYPE,
        SuperBlockFlags::empty(),
        MountOptions::parse(None).unwrap(),
        &kcred::initial_cred(),
    );
    assert!(!fs.flags().contains(SuperBlockFlags::RDONLY));
    let cred = kcred::initial_cred();

    let mut context = FsContext::new_reconfigure(
        fs.as_ref(),
        None,
        Some(b"ro"),
        SuperBlockFlags::empty(),
        SuperBlockFlags::empty(),
        &cred,
    )
    .expect("a remount context");
    fs.reconfigure(&mut context).expect("the remount applies");
    assert!(
        fs.flags().contains(SuperBlockFlags::RDONLY),
        "a data-only ro remount sets the superblock read-only flag"
    );

    let mut context = FsContext::new_reconfigure(
        fs.as_ref(),
        None,
        Some(b"rw"),
        SuperBlockFlags::empty(),
        SuperBlockFlags::empty(),
        &cred,
    )
    .expect("a remount context");
    fs.reconfigure(&mut context).expect("the remount applies");
    assert!(
        !fs.flags().contains(SuperBlockFlags::RDONLY),
        "a data-level rw remount clears the superblock read-only flag"
    );
}
