// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn ioctl_permissions_follow_command_and_open_mode() {
    use block::BlockOpenMode as Mode;
    use linux_raw_sys::ioctl::{BLKFRASET, BLKRASET};

    let disk = block::Gendisk::alloc(
        "test-loop-ioctl".into(),
        7,
        0,
        1,
        Box::new(Arc::new(LoopDevice::new(0))),
    )
    .expect("valid unpublished loop disk");
    let device = disk.part0();
    for (cmd, mode, has_sys_admin, error) in [
        (
            LOOP_SET_STATUS,
            Mode::READ,
            false,
            KError::from(LinuxError::EPERM),
        ),
        (LOOP_SET_STATUS, Mode::WRITE, false, KError::BadAddress),
        (LOOP_SET_STATUS, Mode::READ, true, KError::BadAddress),
        (LOOP_SET_FD, Mode::READ, false, KError::BadFileDescriptor),
        (
            LOOP_CLR_FD,
            Mode::READ,
            false,
            KError::from(LinuxError::ENXIO),
        ),
        (BLKRASET, Mode::WRITE, false, KError::PermissionDenied),
        (BLKFRASET, Mode::WRITE, false, KError::PermissionDenied),
    ] {
        assert_eq!(
            device.as_ref().ioctl(
                mode,
                cmd,
                usize::MAX,
                has_sys_admin,
                || panic!("these commands do not copy a generic int"),
                |_| panic!("these commands do not copy generic output"),
            ),
            Err(error)
        );
    }
}
