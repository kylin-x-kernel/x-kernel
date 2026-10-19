// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::ToOwned, string::String, sync::Arc, vec, vec::Vec};

use kernel_elf_parser::AuxType;
use kerrno::KError;
use khal::paging::MappingFlags;
use kvfs::{
    Dentry, DirMapping, FileSystemType, Mount, NodeFlags, NodePermission, Path, SimpleDir,
    SimpleFile, SimpleFs, VfsInode,
};
use memaddr::VirtAddr;
use memspace::{MmSpace, VmBackingKind};
use unittest::def_test;
use xmas_elf::program::{FLAG_R, FLAG_W, FLAG_X, Flags};

use super::{
    ExecRequest, ExecSource, identity_aux_entries, mapping_flags, script_interpreter_args,
};
use crate::elf_image::tests::{
    executable_bytes, probe_bss_fixture, shared_page_bss_crosses_eof_fixture,
    shared_page_bss_fixture,
};

static TEST_FS: FileSystemType = FileSystemType::internal("kexec-test");

fn image_request(data: Vec<u8>, args: Vec<String>) -> ExecRequest {
    let cached_data = data.clone();
    let size = data.len() as u64;
    let mut inode = None;
    let super_block = SimpleFs::new_with(&TEST_FS, 0, |fs| {
        let file = SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::from_bits_truncate(0o555),
            move || Ok(data.clone()),
        );
        // Scalar reads expose the bytes; the loader also needs their inode size.
        let init = file.inode_init().with_size(size);
        inode = Some(VfsInode::new_file_with_operations(
            file.clone(),
            kvfs::empty_inode_operations(),
            file,
            NodeFlags::ALWAYS_CACHE,
            init,
        ));
        SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
    });
    let inode = inode.unwrap();
    let mount = Mount::new_root(&super_block);
    let dentry = Dentry::new_file_from_inode(inode, None, "app".into());
    let location = Path::new(mount, dentry);
    let file = super::open_exec_file(&location, kcred::initial_cred()).unwrap();
    for (index, bytes) in cached_data.chunks(4096).enumerate() {
        file.mapping()
            .with_folio_or_create(index as u64, |folio| {
                folio.data()[..bytes.len()].copy_from_slice(bytes);
                Ok(())
            })
            .expect("cache fixture bytes");
    }
    ExecRequest::from_resolved_with_display(
        location,
        "/fixture/app".into(),
        args,
        vec!["EXEC_TEST=owned".into()],
        kcred::initial_cred(),
    )
}

#[def_test]
fn loads_pure_bss_beyond_eof_as_zeroed_anonymous_memory() {
    let prepared =
        super::prepare_user_app_request(image_request(probe_bss_fixture(0), vec![])).unwrap();
    let mut mm = MmSpace::new_user_empty().unwrap();
    super::load_prepared_user_app(&mut mm, prepared).unwrap();
    let start = VirtAddr::from_usize(0x41f000);
    let vma = mm.find_vma(start).unwrap();
    assert_eq!(vma.start(), start);
    assert_eq!(vma.end(), VirtAddr::from_usize(0x421000));
    assert_eq!(
        vma.flags(),
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER
    );
    assert!(matches!(
        vma.backing().kind(),
        VmBackingKind::AnonymousPrivate { .. }
    ));
    mm.populate_area(start, 0x2000, MappingFlags::READ).unwrap();
    let mut bytes = vec![0xff; 0x2000];
    mm.read(start, &mut bytes).unwrap();
    assert!(bytes.iter().all(|byte| *byte == 0));
}

#[def_test]
fn pure_bss_preserves_shared_page_file_bytes_and_program_headers() {
    let data = shared_page_bss_fixture();
    let expected = data[0x1000..0x1800].to_vec();
    let prepared = super::prepare_user_app_request(image_request(data, vec![])).unwrap();
    let mut mm = MmSpace::new_user_empty().unwrap();
    super::load_prepared_user_app(&mut mm, prepared).unwrap();
    let start = VirtAddr::from_usize(0x401000);
    assert!(matches!(
        mm.find_vma(start).unwrap().backing().kind(),
        VmBackingKind::FilePrivate { .. }
    ));
    mm.populate_area(start, 0x1000, MappingFlags::READ).unwrap();
    let mut bytes = vec![0xff; 0x1000];
    mm.read(start, &mut bytes).unwrap();
    assert_eq!(&bytes[..0x800], expected.as_slice());
    assert!(bytes[0x800..].iter().all(|byte| *byte == 0));
}

#[def_test]
fn pure_bss_shared_page_crossing_eof_preserves_only_file_bytes() {
    let data = shared_page_bss_crosses_eof_fixture();
    let expected = data[0x1000..].to_vec();
    let prepared = super::prepare_user_app_request(image_request(data, vec![])).unwrap();
    // Poison the cached bytes past EOF: the loader must not expose them,
    // even if a backing implementation leaves a nonzero folio tail.
    prepared
        .executable
        .cache_entry
        .borrow_file()
        .mapping()
        .with_folio_or_create(1, |folio| {
            folio.data()[0x400..0x800].fill(0xcc);
            Ok(())
        })
        .unwrap();
    let mut mm = MmSpace::new_user_empty().unwrap();
    super::load_prepared_user_app(&mut mm, prepared).unwrap();
    let start = VirtAddr::from_usize(0x401000);
    assert!(matches!(
        mm.find_vma(start).unwrap().backing().kind(),
        VmBackingKind::FilePrivate { .. }
    ));
    mm.populate_area(start, 0x1000, MappingFlags::READ).unwrap();
    let mut bytes = vec![0xff; 0x1000];
    mm.read(start, &mut bytes).unwrap();
    assert_eq!(&bytes[..0x400], expected.as_slice());
    assert!(bytes[0x400..].iter().all(|byte| *byte == 0));
}

// The assertion observes a global loader lock; parallel exec fixtures may
// legitimately acquire it after preparation returns. Isolate this check.
#[def_test(serial)]
fn prepared_image_survives_cache_clear_and_rejections_preserve_installed_mm() {
    let request = image_request(
        executable_bytes(),
        vec!["app".into(), "owned-argument".into()],
    );
    let prepared = super::prepare_user_app_request(request).expect("prepare fixture");
    assert!(!prepared.stack_data.is_empty());
    assert!(prepared.stack_data.len() <= kaddr_layout::USER_STACK_SIZE);
    assert_eq!(
        prepared.user_sp.as_usize(),
        kaddr_layout::USER_STACK_TOP - prepared.stack_data.len()
    );
    assert_eq!(prepared.user_sp.as_usize() % 16, 0);
    for encoded in [
        b"app\0".as_slice(),
        b"owned-argument\0",
        b"EXEC_TEST=owned\0",
        b"/fixture/app\0",
    ] {
        assert!(
            prepared
                .stack_data
                .windows(encoded.len())
                .any(|s| s == encoded)
        );
    }
    assert!(super::ELF_LOADER.try_lock().is_some());
    super::clear_elf_cache();
    let expected_stack = prepared.stack_data.clone();
    let expected_entry = prepared.entry;
    let expected_sp = prepared.user_sp;
    let mut mm = MmSpace::new_user_empty().expect("empty user MM");
    assert!(mm.vmas().next().is_none());
    assert_eq!(
        super::load_prepared_user_app(&mut mm, prepared),
        Ok((expected_entry, expected_sp))
    );
    // Inspect the installed ELF mapping permissions.
    assert!(
        mm.find_vma(expected_entry)
            .unwrap()
            .flags()
            .contains(MappingFlags::USER | MappingFlags::EXECUTE)
    );
    assert!(
        mm.find_vma(VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE))
            .is_some()
    );
    let mut actual_stack = vec![0; expected_stack.len()];
    mm.read(expected_sp, &mut actual_stack).unwrap();
    assert_eq!(actual_stack, expected_stack);
    let vma_count = mm.vmas().count();

    let second = super::prepare_user_app_request(image_request(executable_bytes(), vec![]))
        .expect("prepare second fixture");
    assert_eq!(
        super::load_prepared_user_app(&mut mm, second),
        Err(KError::InvalidInput)
    );
    assert_eq!(mm.vmas().count(), vma_count);
    actual_stack.fill(0);
    mm.read(expected_sp, &mut actual_stack).unwrap();
    assert_eq!(actual_stack, expected_stack);

    for (data, args, error) in [
        (b"not ELF".to_vec(), vec![], KError::InvalidExecutable),
        (
            executable_bytes(),
            vec!["x".repeat(kaddr_layout::USER_STACK_SIZE)],
            KError::ArgumentListTooLong,
        ),
    ] {
        assert!(matches!(
            super::prepare_user_app_request(image_request(data, args)),
            Err(actual) if actual == error
        ));
        assert_eq!(mm.vmas().count(), vma_count);
        actual_stack.fill(0);
        mm.read(expected_sp, &mut actual_stack).unwrap();
        assert_eq!(actual_stack, expected_stack);
    }
    super::clear_elf_cache();
}

#[def_test]
fn test_mapping_flags_sets_user_and_requested_permissions() {
    let none = mapping_flags(Flags(0));
    assert_eq!(none, MappingFlags::USER);

    let read = mapping_flags(Flags(FLAG_R));
    assert!(read.contains(MappingFlags::USER | MappingFlags::READ));
    assert!(!read.contains(MappingFlags::WRITE));

    let write_exec = mapping_flags(Flags(FLAG_W | FLAG_X));
    assert!(write_exec.contains(MappingFlags::USER | MappingFlags::WRITE));
    assert!(write_exec.contains(MappingFlags::EXECUTE));
    assert!(!write_exec.contains(MappingFlags::READ));
}

#[def_test]
fn test_mapping_flags_all_bits_combination() {
    let flags = mapping_flags(Flags(FLAG_R | FLAG_W | FLAG_X));
    assert!(flags.contains(MappingFlags::USER));
    assert!(flags.contains(MappingFlags::READ));
    assert!(flags.contains(MappingFlags::WRITE));
    assert!(flags.contains(MappingFlags::EXECUTE));
}

#[def_test]
fn exec_request_owns_path_args_and_envs() {
    let mut args = vec!["app".to_owned(), "one".to_owned()];
    let mut envs = vec!["A=B".to_owned()];
    let request = ExecRequest::from_path(
        "/bin/app",
        args.clone(),
        envs.clone(),
        kcred::initial_cred(),
    );

    args[0].push_str("-changed");
    envs[0].push_str("-changed");

    match request.source() {
        ExecSource::Path(path) => assert_eq!(path, "/bin/app"),
        ExecSource::Resolved { .. } => panic!("unexpected resolved source"),
        ExecSource::File { .. } => panic!("unexpected file source"),
    }
    assert_eq!(request.args().len(), 2);
    assert_eq!(request.args()[0], "app");
    assert_eq!(request.args()[1], "one");
    assert_eq!(request.envs().len(), 1);
    assert_eq!(request.envs()[0], "A=B");
}

#[def_test]
fn script_interpreter_args_rewrites_linux_shape() {
    let original = vec![
        "/tmp/script.sh".to_owned(),
        "arg1".to_owned(),
        "arg2".to_owned(),
    ];
    let rewritten = script_interpreter_args("/bin/sh -e", "/tmp/script.sh", original.as_slice())
        .expect("valid interpreter line");

    assert_eq!(rewritten.len(), 5);
    assert_eq!(rewritten[0], "/bin/sh");
    assert_eq!(rewritten[1], "-e");
    assert_eq!(rewritten[2], "/tmp/script.sh");
    assert_eq!(rewritten[3], "arg1");
    assert_eq!(rewritten[4], "arg2");
}

#[def_test]
fn script_interpreter_args_rejects_empty_line() {
    assert_eq!(
        script_interpreter_args("   ", "/tmp/script.sh", &[]),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn identity_aux_entries_report_ids_and_secure_exec() {
    let mut cred = kcred::Cred::root();
    cred.set_resgid(Some(100), Some(200), Some(300))
        .expect("root must be able to set group IDs");
    cred.set_resuid(Some(1000), Some(2000), Some(3000))
        .expect("root must be able to set user IDs");

    let entries = identity_aux_entries(&cred);
    assert!(entries[0].get_type() == AuxType::UID);
    assert_eq!(entries[0].value(), 1000);
    assert!(entries[1].get_type() == AuxType::EUID);
    assert_eq!(entries[1].value(), 2000);
    assert!(entries[2].get_type() == AuxType::GID);
    assert_eq!(entries[2].value(), 100);
    assert!(entries[3].get_type() == AuxType::EGID);
    assert_eq!(entries[3].value(), 200);
    assert!(entries[4].get_type() == AuxType::SECURE);
    assert_eq!(entries[4].value(), 1);
}

#[def_test]
fn identity_aux_entries_include_zero_secure_value() {
    let entries = identity_aux_entries(&kcred::Cred::new(1000, 100));
    assert!(entries[4].get_type() == AuxType::SECURE);
    assert_eq!(entries[4].value(), 0);
}
