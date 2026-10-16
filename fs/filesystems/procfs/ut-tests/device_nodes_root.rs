// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn named_cdev_output_follows_successful_registration() {
    struct Backend;
    impl kvfs::DeviceFileOps for Backend {}
    let device = kvfs::DeviceId::new(246, 900002);
    kvfs::cdev_add_named(
        device,
        String::from("proc_projection_test"),
        String::from("test"),
        Arc::new(Backend),
    )
    .unwrap();
    let mut iter = DevicesIter::new();
    iter.rewind();
    assert!(iter.text.contains("proc_projection_test [active]"));
    assert!(
        iter.text
            .contains("Registered character devices (major:minor):\n")
    );
    assert!(
        iter.text
            .contains("246:900002 proc_projection_test [active]")
    );
    kvfs::cdev_del(device, 1).unwrap();
    iter.rewind();
    assert!(!iter.text.contains("proc_projection_test"));
}
