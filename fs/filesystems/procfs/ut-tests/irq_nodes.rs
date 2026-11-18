// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, vec::Vec};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use kerrno::LinuxError;
use kirq::{IrqDesc, tests_irq::SyntheticLine};
use kvfs::{
    DirContext, Filename, Mount, NodePermission, NodeType, OpenFlags, SuperBlockFlags, VfsError,
    VfsFile,
};
use unittest::{assert, assert_eq, def_test};

use kcpu_id_map::text::{CpuTextDomain, write_bitmap};
use crate::irq_nodes::file::MAX_COMMAND_BYTES;

fn cpu_mask(cpus: &[usize]) -> kcpu_id_map::KCpuMask {
    let mut mask = kcpu_id_map::KCpuMask::new();
    for cpu in cpus {
        mask.set_logical(LogicalCpuId::new(*cpu), true);
    }
    mask
}

fn open_at(
    file: &VfsFile,
    name: &str,
    flags: OpenFlags,
) -> alloc::sync::Arc<VfsFile> {
    open_at_result(file, name, flags)
        .unwrap_or_else(|error| panic!("open_at {name} failed: {error:?}"))
}

fn open_at_result(
    file: &VfsFile,
    name: &str,
    flags: OpenFlags,
) -> kvfs::VfsResult<alloc::sync::Arc<VfsFile>> {
    Filename::new(name).open_with_flags_at(
        file.path(),
        file.path(),
        flags.bits(),
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    )
}

fn open_path_cred(
    root: &kvfs::Path,
    path: &str,
    flags: OpenFlags,
    cred: alloc::sync::Arc<kcred::Cred>,
) -> kvfs::VfsResult<alloc::sync::Arc<VfsFile>> {
    Filename::new(path).open_with_flags_at(
        root,
        root,
        flags.bits(),
        NodePermission::empty(),
        NodePermission::empty(),
        cred,
    )
}

fn read_entries(dir: &VfsFile) -> Vec<(String, u64, NodeType)> {
    let mut entries = Vec::new();
    let mut sink = |name: &str, ino: u64, node_type: NodeType, _: u64| {
        entries.push((name.into(), ino, node_type));
        true
    };
    let mut context = DirContext::new(0, &mut sink);
    dir.iterate_dir(&mut context).unwrap();
    entries
}

fn expected_bitmap(mask: kcpu_id_map::KCpuMask) -> String {
    let domain = CpuTextDomain::runtime().expect("CPU text domain");
    let mut text = String::new();
    write_bitmap(mask, &domain, &mut text).expect("bitmap format");
    text.push('\n');
    text
}

fn irq_root() -> (alloc::sync::Arc<kvfs::SuperBlock>, alloc::sync::Arc<VfsFile>) {
    let fs = crate::new_procfs(SuperBlockFlags::empty()).unwrap();
    let root = Mount::new_root(&fs).root_path();
    let dir = Filename::new("/irq")
        .open_with_flags_at(
            &root,
            &root,
            OpenFlags::empty().bits(),
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    (fs, dir)
}

#[def_test(serial)]
fn root_directory_uses_short_read_cookies() {
    let line = SyntheticLine::new(IrqDesc::from_virq(0x7101));
    let name = format!("{}", line.virq());
    let (_fs, dir) = irq_root();

    let mut first = Vec::new();
    let mut sink = |entry: &str, _: u64, _: NodeType, cookie: u64| {
        if !first.is_empty() {
            return false;
        }
        first.push((entry.into(), cookie));
        true
    };
    let mut context = DirContext::new(0, &mut sink);
    assert_eq!(dir.iterate_dir(&mut context), Ok(1));
    assert_eq!(first, [(String::from("."), 1)]);
    assert_eq!(dir.position(), 1);

    let mut rest: Vec<(String, u64)> = Vec::new();
    let mut sink = |entry: &str, _: u64, _: NodeType, cookie: u64| {
        rest.push((entry.into(), cookie));
        true
    };
    let mut context = DirContext::new(dir.position(), &mut sink);
    assert!(dir.iterate_dir(&mut context).unwrap() > 0);
    assert!(rest.iter().any(|(entry, _)| entry == &name));
    assert!(rest.iter().all(|(_, cookie)| *cookie >= 2));
}

#[def_test(serial)]
fn root_enumeration_is_weak_across_insert_and_remove() {
    let first = SyntheticLine::new(IrqDesc::from_virq(0x7102));
    let first_name = format!("{}", first.virq());
    let (_fs, dir) = irq_root();
    assert!(read_entries(&dir)
        .iter()
        .any(|(name, _, _)| name == &first_name));

    let second = SyntheticLine::new(IrqDesc::from_virq(0x7103));
    let second_name = format!("{}", second.virq());
    dir.set_position(0);
    assert!(read_entries(&dir)
        .iter()
        .any(|(name, _, _)| name == &second_name));

    drop(first);
    dir.set_position(0);
    let after = read_entries(&dir);
    assert!(after.iter().all(|(name, _, _)| name != &first_name));
    drop(second);
}

#[def_test(serial)]
fn old_line_directory_cannot_follow_a_reused_virq() {
    let virq = 0x7104;
    let first = SyntheticLine::new(IrqDesc::from_virq(virq));
    let name = format!("{virq}");
    let (_fs, root_dir) = irq_root();
    let old_dir = open_at(&root_dir, &name, OpenFlags::empty());
    let historical = open_at(&old_dir, "smp_affinity", OpenFlags::empty());
    let mut historical_buf = [0u8; 128];
    let mut historical_pos = 0;
    assert_eq!(
        historical.read_from(&mut historical_buf, &mut historical_pos),
        Err(VfsError::from(LinuxError::ENODATA))
    );

    drop(first);
    let _second = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(
        open_at_result(&old_dir, "smp_affinity", OpenFlags::empty()).err(),
        Some(VfsError::NotFound)
    );
    assert_eq!(
        kvfs::dentry_open(
            historical.path().clone(),
            OpenFlags::empty().bits(),
            kcred::initial_cred(),
        )
        .err(),
        Some(VfsError::NotFound)
    );
    let mut historical_again_pos = 0;
    assert_eq!(
        historical.read_from(&mut historical_buf, &mut historical_again_pos),
        Err(VfsError::NotFound)
    );
    let fresh = open_at(&root_dir, &name, OpenFlags::empty());
    assert_ne!(fresh.inode().metadata().inode, old_dir.inode().metadata().inode);
    let fresh_smp = open_at(&fresh, "smp_affinity", OpenFlags::empty());
    let mut fresh_buf = [0u8; 128];
    let mut fresh_pos = 0;
    assert_eq!(
        fresh_smp.read_from(&mut fresh_buf, &mut fresh_pos),
        Err(VfsError::from(LinuxError::ENODATA))
    );
}
#[def_test(serial)]
fn directory_entries_report_consistent_labels_and_parents() {
    let virq = 0x7105;
    let _line = SyntheticLine::new(IrqDesc::from_virq(virq));
    let name = format!("{virq}");
    let (_fs, irq) = irq_root();
    let root_entries = read_entries(&irq);
    let root_ino = irq.inode().metadata().inode;
    let parent_ino = irq.path().dentry().parent().unwrap().metadata().inode;
    assert_eq!(
        root_entries[0],
        (String::from("."), root_ino, NodeType::Directory)
    );
    assert_eq!(
        root_entries[1],
        (String::from(".."), parent_ino, NodeType::Directory)
    );
    let root_child = root_entries
        .iter()
        .find(|(entry, _, _)| entry == &name)
        .expect("synthetic line is enumerated");
    assert_ne!(root_child.1, 0);

    let line_dir = open_at(&irq, &name, OpenFlags::empty());
    let line_entries = read_entries(&line_dir);
    let line_ino = line_dir.inode().metadata().inode;
    let line_parent_ino = line_dir.path().dentry().parent().unwrap().metadata().inode;
    assert_eq!(
        line_entries[0],
        (String::from("."), line_ino, NodeType::Directory)
    );
    assert_eq!(
        line_entries[1],
        (String::from(".."), line_parent_ino, NodeType::Directory)
    );
    assert_eq!(line_parent_ino, root_ino);

    let file = open_at(&line_dir, "smp_affinity", OpenFlags::empty());
    let file_ino = file.inode().metadata().inode;
    let file_entry = line_entries
        .iter()
        .find(|(entry, _, _)| entry == "smp_affinity")
        .expect("file entry exists");
    assert_eq!(file_entry.1, file_ino);
    assert_eq!(file_entry.2, NodeType::RegularFile);
    assert!(!line_entries
        .iter()
        .any(|(entry, _, _)| entry == "affinity_status"));
}

#[def_test(serial)]
fn maximum_virq_is_enumerated() {
    let line = SyntheticLine::new(IrqDesc::from_virq(usize::MAX));
    let name = format!("{}", line.virq());
    let (_fs, irq) = irq_root();
    let entries = read_entries(&irq);
    let entry = entries
        .iter()
        .find(|(entry, _, _)| entry == &name)
        .expect("maximum virq is enumerated");
    assert_ne!(entry.1, 0);
}

#[def_test(serial)]
fn numeric_directory_rejects_leading_zero_alias() {
    let line = SyntheticLine::new(IrqDesc::from_virq(0x7112));
    let name = format!("{}", line.virq());
    let (_fs, irq) = irq_root();
    assert_eq!(
        open_at_result(&irq, &format!("0{name}"), OpenFlags::empty()).err(),
        Some(VfsError::NotFound)
    );
    assert!(open_at_result(&irq, &name, OpenFlags::empty()).is_ok());
    drop(line);
}

#[def_test(serial)]
fn sparse_root_has_no_legacy_4096_entry_limit() {
    let lines: Vec<_> = (0..4097)
        .map(|index| SyntheticLine::new(IrqDesc::from_virq(0x8000 + index)))
        .collect();
    let (_fs, dir) = irq_root();
    let mut count = 0;
    let mut sink = |_: &str, _: u64, _: NodeType, _: u64| {
        count += 1;
        true
    };
    let mut context = DirContext::new(0, &mut sink);
    assert!(dir.iterate_dir(&mut context).unwrap() > 4096);
    assert!(count > 4096);
    drop(lines);
}
#[def_test(serial)]
fn requested_file_is_lazy_and_can_be_opened_before_first_affinity() {
    let virq = 0x7106;
    let line = SyntheticLine::new(IrqDesc::from_virq(virq));
    line.with_affinity_chip(&ACCEPT);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    let handle = kirq::lookup_irq_line(virq).unwrap().unwrap();
    assert_eq!(handle.irq_affinity().unwrap().requested, None);
    let file = open_at(&open_at(&irq_root().1, &format!("{virq}"), OpenFlags::empty()),
        "smp_affinity",
        OpenFlags::READ_WRITE);
    let mut buf = [0u8; 128];
    let mut pos = 0;
    assert_eq!(
        file.read_from(&mut buf, &mut pos),
        Err(VfsError::from(LinuxError::ENODATA))
    );
    assert_eq!(file.write(b"1\n"), Ok(2));
    assert_eq!(file.write(b"1\n"), Ok(2));
    assert_eq!(file.write(b""), Ok(0));
    let mut oversized = Vec::from([b'1'; MAX_COMMAND_BYTES + 1]);
    oversized.push(b'\n');
    assert_eq!(file.write(&oversized), Err(VfsError::FileTooLarge));
    let mut pos = 0;
    let len = file.read_from(&mut buf, &mut pos).unwrap();
    let expected = expected_bitmap(cpu_mask(&[0]));
    assert_eq!(&buf[..len], expected.as_bytes());

    // Command writes do not move f_pos, so a plain read starts at zero.
    let len = file.read(&mut buf).unwrap();
    assert_eq!(&buf[..len], expected.as_bytes());
}

#[def_test(serial)]
fn effective_snapshot_reports_unknown_at_first_read() {
    let virq = 0x7107;
    let line = SyntheticLine::new(IrqDesc::from_virq(virq));
    line.with_affinity_chip(&ACCEPT);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    let dir = open_at(&irq_root().1, &format!("{virq}"), OpenFlags::empty());
    let effective = open_at(&dir, "effective_affinity", OpenFlags::empty());
    let mut buf = [0u8; 128];
    let mut pos = 0;
    assert_eq!(
        effective.read_from(&mut buf, &mut pos),
        Err(VfsError::from(LinuxError::ENODATA))
    );
}

#[def_test(serial)]
fn requested_write_is_policy_checked_and_bounded() {
    let virq = 0x7108;
    let line = SyntheticLine::new(IrqDesc::from_virq(virq));
    line.with_affinity_chip(&ACCEPT);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled().restrict_user_affinity(),
        ),
        Ok(true)
    );
    let handle = kirq::lookup_irq_line(virq).unwrap().unwrap();
    handle.set_irq_affinity(&cpu_mask(&[0])).unwrap();
    let dir = open_at(&irq_root().1, &format!("{virq}"), OpenFlags::empty());
    let file = open_at(&dir, "smp_affinity", OpenFlags::READ_WRITE);
    let mut buffer = [0u8; 128];
    let mut pos = 0;
    assert!(file.read_from(&mut buffer, &mut pos).is_ok());
    assert_eq!(
        file.write(b"1\n"),
        Err(VfsError::from(LinuxError::EPERM))
    );
    let mut offset = 1;
    assert_eq!(
        file.write_from(b"1\n", &mut offset),
        Err(VfsError::from(LinuxError::EPERM))
    );
    assert_eq!(file.write(b"zzz\n"), Err(VfsError::from(LinuxError::EPERM)));
    assert_eq!(file.write(b""), Ok(0));
    let mut oversized = Vec::from([b'1'; MAX_COMMAND_BYTES + 1]);
    oversized.push(b'\n');
    assert_eq!(file.write(&oversized), Err(VfsError::from(LinuxError::EPERM)));
}

#[def_test(serial)]
fn requested_write_open_is_dac_protected() {
    let line = SyntheticLine::new(IrqDesc::from_virq(0x7109));
    let fs = crate::new_procfs(SuperBlockFlags::empty()).unwrap();
    let root = Mount::new_root(&fs).root_path();
    let result = Filename::new(format!("/irq/{}/smp_affinity", line.virq()))
        .open_with_flags_at(
            &root,
            &root,
            OpenFlags::READ_WRITE.bits(),
            NodePermission::empty(),
            NodePermission::empty(),
            alloc::sync::Arc::new(kcred::Cred::new(1000, 1000)),
        );
    assert_eq!(result.err(), Some(VfsError::PermissionDenied));
}

static ACCEPT: kirq::IrqAffinityChip = kirq::IrqAffinityChip {
    apply: |_, eligible, _| Ok(*eligible),
};

fn deferred_accept(
    _: kirq::IrqDesc,
    _: &kcpu_id_map::KCpuMask,
) -> Result<(), kirq::IrqAffinityApplyError> {
    Ok(())
}

fn deferred_prepare(
    _: kirq::IrqDesc,
    _: &kcpu_id_map::KCpuMask,
    _: kirq::IrqAffinityDelivery,
) -> Result<kirq::IrqAffinityPrepare, kirq::IrqAffinityApplyError> {
    Ok(kirq::IrqAffinityPrepare::Pending(0))
}

fn deferred_finish(
    _: kirq::IrqDesc,
    _: usize,
    _: kirq::IrqAffinityDelivery,
) -> Result<Option<kcpu_id_map::KCpuMask>, kirq::IrqAffinityApplyError> {
    Ok(None)
}

static DEFERRED: kirq::IrqDeferredAffinityChip = kirq::IrqDeferredAffinityChip {
    accept: deferred_accept,
    prepare: deferred_prepare,
    finish: deferred_finish,
};

fn fail_apply(
    _: kirq::IrqDesc,
    _: &kcpu_id_map::KCpuMask,
    _: kirq::IrqAffinityDelivery,
) -> Result<kcpu_id_map::KCpuMask, kirq::IrqAffinityApplyError> {
    Err(kirq::IrqAffinityApplyError::HardwareFailure)
}

static FAIL: kirq::IrqAffinityChip = kirq::IrqAffinityChip { apply: fail_apply };

#[def_test(serial)]
fn deferred_request_is_visible_and_write_stays_pending() {
    let virq = 0x710a;
    let line = SyntheticLine::new(IrqDesc::from_virq(virq));
    line.with_deferred_affinity_chip(&DEFERRED);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    let handle = kirq::lookup_irq_line(virq).unwrap().unwrap();
    handle.set_irq_affinity(&cpu_mask(&[0])).unwrap();
    let dir = open_at(&irq_root().1, &format!("{virq}"), OpenFlags::empty());
    let requested = open_at(&dir, "smp_affinity", OpenFlags::empty());
    let mut buf = [0u8; 128];
    let mut pos = 0;
    requested.read_from(&mut buf, &mut pos).unwrap();
    let effective = open_at(&dir, "effective_affinity", OpenFlags::empty());
    let mut effective_buf = [0u8; 128];
    let mut effective_pos = 0;
    assert_eq!(
        effective.read_from(&mut effective_buf, &mut effective_pos),
        Err(VfsError::from(LinuxError::ENODATA))
    );
    let file = open_at(&dir, "smp_affinity", OpenFlags::READ_WRITE);
    assert_eq!(file.write(b"1\n"), Ok(2));
}

#[def_test(serial)]
fn hardware_failure_write_error_and_effective_is_unknown() {
    let virq = 0x710b;
    let line = SyntheticLine::new(IrqDesc::from_virq(virq));
    line.with_affinity_chip(&FAIL);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    let handle = kirq::lookup_irq_line(virq).unwrap().unwrap();
    assert!(handle.set_irq_affinity(&cpu_mask(&[0])).is_err());
    let dir = open_at(&irq_root().1, &format!("{virq}"), OpenFlags::empty());
    let effective = open_at(&dir, "effective_affinity", OpenFlags::empty());
    let mut effective_buf = [0u8; 128];
    let mut effective_pos = 0;
    assert_eq!(
        effective.read_from(&mut effective_buf, &mut effective_pos),
        Err(VfsError::from(LinuxError::ENODATA))
    );
    let file = open_at(&dir, "smp_affinity", OpenFlags::READ_WRITE);
    assert_eq!(file.write(b"1\n"), Err(VfsError::from(LinuxError::EIO)));
}

#[def_test(serial)]
fn owner_fixed_policy_is_readable_and_user_write_is_denied() {
    let virq = 0x710c;
    let mut desc = IrqDesc::from_virq(virq);
    desc.affinity = kirq::IrqAffinity::Cpu(0);
    let line = SyntheticLine::new(desc);
    line.set_requested_policy_for_tests(cpu_mask(&[0]));
    let dir = open_at(&irq_root().1, &format!("{virq}"), OpenFlags::empty());
    let mask = open_at(&dir, "smp_affinity", OpenFlags::empty());
    let mut mask_buf = [0u8; 128];
    let mut mask_pos = 0;
    let len = mask.read_from(&mut mask_buf, &mut mask_pos).unwrap();
    let expected = expected_bitmap(cpu_mask(&[0]));
    assert_eq!(&mask_buf[..len], expected.as_bytes());
    let writable = open_at(&dir, "smp_affinity", OpenFlags::READ_WRITE);
    assert_eq!(writable.write(b"1\n"), Err(VfsError::from(LinuxError::EPERM)));
    assert_eq!(writable.write(b"zzz\n"), Err(VfsError::from(LinuxError::EPERM)));
    let mut oversized = Vec::from([b'1'; MAX_COMMAND_BYTES + 1]);
    oversized.push(b'\n');
    assert_eq!(writable.write(&oversized), Err(VfsError::from(LinuxError::EPERM)));
    drop(line);
}

#[def_test(serial)]
fn restriction_added_after_open_is_rechecked() {
    let virq = 0x710d;
    let line = SyntheticLine::new(IrqDesc::from_virq(virq));
    line.with_affinity_chip(&ACCEPT);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    let handle = kirq::lookup_irq_line(virq).unwrap().unwrap();
    let initial = cpu_mask(&[0]);
    handle.set_irq_affinity(&initial).unwrap();
    let dir = open_at(&irq_root().1, &format!("{virq}"), OpenFlags::empty());
    let file = open_at(&dir, "smp_affinity", OpenFlags::READ_WRITE);
    assert!(kirq::try_free_irq(virq).unwrap().is_some());
    let restricted: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            restricted,
            kirq::IrqRegisterOptions::disabled().restrict_user_affinity(),
        ),
        Ok(true)
    );
    assert_eq!(
        file.write(b"1\n"),
        Err(VfsError::from(LinuxError::EPERM))
    );
    let reopened = open_at(&dir, "smp_affinity", OpenFlags::READ_WRITE);
    assert_eq!(reopened.path().getattr().unwrap().mode.permission().bits(), 0o644);
    assert_eq!(
        reopened.write(b"1\n"),
        Err(VfsError::from(LinuxError::EPERM))
    );
    assert_eq!(handle.irq_affinity().unwrap().requested, Some(initial));
}

#[def_test(serial)]
fn old_writer_cannot_touch_a_replacement_line() {
    let virq = 0x710e;
    let first = SyntheticLine::new(IrqDesc::from_virq(virq));
    first.with_affinity_chip(&ACCEPT);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    let first_handle = kirq::lookup_irq_line(virq).unwrap().unwrap();
    let initial = cpu_mask(&[0]);
    first_handle.set_irq_affinity(&initial).unwrap();
    let dir = open_at(&irq_root().1, &format!("{virq}"), OpenFlags::empty());
    let old_file = open_at(&dir, "smp_affinity", OpenFlags::READ_WRITE);
    drop(first);

    let second = SyntheticLine::new(IrqDesc::from_virq(virq));
    second.with_affinity_chip(&ACCEPT);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    let second_handle = kirq::lookup_irq_line(virq).unwrap().unwrap();
    let second_initial = cpu_mask(&[0]);
    second_handle.set_irq_affinity(&second_initial).unwrap();
    assert_eq!(old_file.write(b"1\n"), Err(VfsError::NotFound));
    assert_eq!(
        second_handle.irq_affinity().unwrap().requested,
        Some(second_initial)
    );
}

#[def_test(serial)]
fn line_lookup_reuses_stable_inode() {
    let line = SyntheticLine::new(IrqDesc::from_virq(0x7113));
    let name = format!("{}", line.virq());
    let (_fs, root) = irq_root();
    let first = open_at(&root, &name, OpenFlags::empty());
    let second = open_at(&root, &name, OpenFlags::empty());
    assert_eq!(
        first.inode().metadata().inode,
        second.inode().metadata().inode
    );
    let first_file = open_at(&first, "smp_affinity", OpenFlags::empty());
    let second_file = open_at(&second, "smp_affinity", OpenFlags::empty());
    assert_eq!(
        first_file.inode().metadata().inode,
        second_file.inode().metadata().inode
    );
}

#[def_test(serial)]
fn retirement_removes_the_namespace_record_and_preserves_old_dentry() {
    let virq = 0x7115;
    let first = SyntheticLine::new(IrqDesc::from_virq(virq));
    let name = format!("{virq}");
    let (_fs, root) = irq_root();
    let old_dir = open_at(&root, &name, OpenFlags::empty());
    let old_ino = old_dir.inode().metadata().inode;
    let old_file = open_at(&old_dir, "smp_affinity", OpenFlags::empty());

    first.retire_for_tests();
    assert_eq!(
        open_at_result(&root, &name, OpenFlags::empty()).err(),
        Some(VfsError::NotFound)
    );
    assert_eq!(
        old_dir.inode().metadata().inode,
        old_ino
    );
    let mut buf = [0u8; 128];
    let mut pos = 0;
    assert_eq!(
        old_file.read_from(&mut buf, &mut pos),
        Err(VfsError::NotFound)
    );

    let successor = SyntheticLine::new(IrqDesc::from_virq(virq));
    let fresh = open_at(&root, &name, OpenFlags::empty());
    assert_ne!(fresh.inode().metadata().inode, old_ino);
    let fresh_file = open_at(&fresh, "smp_affinity", OpenFlags::empty());
    let mut fresh_buf = [0u8; 128];
    let mut fresh_pos = 0;
    assert_eq!(
        fresh_file.read_from(&mut fresh_buf, &mut fresh_pos),
        Err(VfsError::from(LinuxError::ENODATA))
    );
    drop(successor);
}

#[cfg(any(target_arch = "x86_64", unittest))]
#[def_test(serial)]
fn production_msi_free_removes_namespace_record() {
    let virq = 0x7116;
    let probe = kirq::tests_irq::RcuIndexProbe::new(virq, virq);
    probe.mark_action_history_for_tests();
    let name = format!("{virq}");
    let (_fs, root) = irq_root();
    let dir = open_at(&root, &name, OpenFlags::empty());
    let file = open_at(&dir, "smp_affinity", OpenFlags::empty());
    assert_eq!(file.path().getattr().unwrap().mode.permission().bits(), 0o444);
    assert!(probe.free_msi_via_production());
    assert_eq!(
        open_at_result(&root, &name, OpenFlags::empty()).err(),
        Some(VfsError::NotFound)
    );
    let mut buf = [0u8; 128];
    let mut pos = 0;
    assert_eq!(
        file.read_from(&mut buf, &mut pos),
        Err(VfsError::NotFound)
    );
}

#[def_test(serial)]
fn actionless_line_is_not_published() {
    let line = SyntheticLine::new(IrqDesc::from_virq(0x7114));
    line.clear_action_history_for_tests();
    let name = format!("{}", line.virq());
    let (_fs, root) = irq_root();
    assert_eq!(
        open_at_result(&root, &name, OpenFlags::empty()).err(),
        Some(VfsError::NotFound)
    );
}


#[def_test(serial)]
fn stable_permissions_survive_reopen_and_are_per_superblock() {
    let virq = 0x7117;
    let line = SyntheticLine::new(IrqDesc::from_virq(virq));
    line.with_affinity_chip(&ACCEPT);
    let handler: kirq::Handler = alloc::sync::Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert_eq!(
        kirq::try_register_with_options(
            virq,
            handler,
            kirq::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );

    let fs = crate::new_procfs(SuperBlockFlags::empty()).unwrap();
    let holder = Mount::new_root(&fs);
    let root = holder.root_path();
    let path = format!("/irq/{virq}/smp_affinity");
    let file = open_path_cred(&root, &path, OpenFlags::empty(), kcred::initial_cred()).unwrap();
    let root_cred = kcred::initial_cred();
    file.path()
        .chmod(NodePermission::from_bits_truncate(0o600), &root_cred)
        .unwrap();
    file.path().chown(Some(1000), Some(1000), &root_cred).unwrap();
    drop(file);
    drop(root);
    let root = Mount::new_root(&fs).root_path();

    let reopened = open_path_cred(&root, &path, OpenFlags::empty(), kcred::initial_cred()).unwrap();
    let metadata = reopened.path().getattr().unwrap();
    assert_eq!(metadata.mode.permission().bits(), 0o600);
    assert_eq!(metadata.uid, 1000);
    assert_eq!(metadata.gid, 1000);
    drop(reopened);

    let owner = alloc::sync::Arc::new(kcred::Cred::new(1000, 1000));
    assert!(open_path_cred(&root, &path, OpenFlags::READ_WRITE, owner).is_ok());
    let other = alloc::sync::Arc::new(kcred::Cred::new(1001, 1001));
    assert_eq!(
        open_path_cred(&root, &path, OpenFlags::empty(), other).err(),
        Some(VfsError::PermissionDenied)
    );

    let other_fs = crate::new_procfs(SuperBlockFlags::empty()).unwrap();
    let other_root = Mount::new_root(&other_fs).root_path();
    let isolated =
        open_path_cred(&other_root, &path, OpenFlags::empty(), kcred::initial_cred()).unwrap();
    let isolated_metadata = isolated.path().getattr().unwrap();
    assert_eq!(isolated_metadata.mode.permission().bits(), 0o644);
    assert_eq!(isolated_metadata.uid, 0);
    assert_eq!(isolated_metadata.gid, 0);

    line.retire_for_tests();
    let successor = SyntheticLine::new(IrqDesc::from_virq(virq));
    successor.with_affinity_chip(&ACCEPT);
    let fresh = open_path_cred(&root, &path, OpenFlags::empty(), kcred::initial_cred()).unwrap();
    let fresh_metadata = fresh.path().getattr().unwrap();
    assert_eq!(fresh_metadata.mode.permission().bits(), 0o644);
    assert_eq!(fresh_metadata.uid, 0);
    assert_eq!(fresh_metadata.gid, 0);
    drop(successor);
}

#[def_test(serial)]
fn fixed_source_requested_files_start_read_only() {
    let virq = 0x7118;
    let mut desc = IrqDesc::from_virq(virq);
    desc.affinity = kirq::IrqAffinity::Cpu(0);
    let line = SyntheticLine::new(desc);
    line.set_requested_policy_for_tests(cpu_mask(&[0]));
    let root = {
        let fs = crate::new_procfs(SuperBlockFlags::empty()).unwrap();
        Mount::new_root(&fs).root_path()
    };
    let path = format!("/irq/{virq}/smp_affinity");
    let file = open_path_cred(&root, &path, OpenFlags::empty(), kcred::initial_cred()).unwrap();
    assert_eq!(file.path().getattr().unwrap().mode.permission().bits(), 0o444);
    let effective = open_path_cred(
        &root,
        &format!("/irq/{virq}/effective_affinity"),
        OpenFlags::empty(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert_eq!(
        effective.path().getattr().unwrap().mode.permission().bits(),
        0o444
    );
}


#[def_test(serial)]
fn retired_dirfd_rejects_first_and_continued_getdents() {
    let first = SyntheticLine::new(IrqDesc::from_virq(0x7119));
    let name = format!("{}", first.virq());
    let (_fs, root) = irq_root();
    let dir = open_at(&root, &name, OpenFlags::empty());
    first.retire_for_tests();

    let mut sink = |_: &str, _: u64, _: NodeType, _: u64| true;
    let mut context = DirContext::new(0, &mut sink);
    assert_eq!(dir.iterate_dir(&mut context), Err(VfsError::NotFound));

    let second = SyntheticLine::new(IrqDesc::from_virq(0x711a));
    let second_name = format!("{}", second.virq());
    let second_dir = open_at(&root, &second_name, OpenFlags::empty());
    let mut first_read = |_: &str, _: u64, _: NodeType, _: u64| false;
    let mut context = DirContext::new(0, &mut first_read);
    assert_eq!(second_dir.iterate_dir(&mut context), Ok(0));
    let position = second_dir.position();
    second.retire_for_tests();
    let mut continued = |_: &str, _: u64, _: NodeType, _: u64| true;
    let mut context = DirContext::new(position, &mut continued);
    assert_eq!(second_dir.iterate_dir(&mut context), Err(VfsError::NotFound));
}
