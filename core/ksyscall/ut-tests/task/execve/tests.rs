// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::format;

use ksignal::SignalStack;
use unittest::def_test;

use super::*;

#[def_test(user, serial)]
fn empty_exec_path_preserves_current_image() {
    let thread = kprocess::current_user_thread();
    let process = thread.process();
    let signal = thread.signal_manager();
    let original_tid = thread.clear_child_tid();
    let original_robust = thread.robust_list_head();
    let original_stack = signal.stack();
    let mm_id = process.mm_id().unwrap();
    let heap = process.heap_top().unwrap();
    let snapshot = || {
        process
            .address_space()
            .unwrap()
            .lock()
            .vmas()
            .map(|vma| (vma.start(), vma.end(), vma.flags()))
            .collect::<Vec<_>>()
    };
    let mappings = snapshot();
    let mut uctx = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
    uctx.set_tls(0x9000);
    // Debug includes private registers without reading struct padding.
    let context = format!("{uctx:?}");
    // These registrations are observed only; this path must never dereference them.
    thread.set_clear_child_tid(0x4000);
    thread.set_robust_list_head(0x5000);
    signal.set_stack(SignalStack {
        sp: 0x6000,
        flags: 0,
        size: 0x2000,
    });

    assert_eq!(
        exec_owned(&mut uctx, String::new(), Vec::new(), Vec::new(), None),
        Err(KError::NotFound)
    );
    assert_eq!(process.mm_id().unwrap(), mm_id);
    assert_eq!(snapshot(), mappings);
    assert_eq!(process.heap_top().unwrap(), heap);
    assert_eq!(thread.clear_child_tid(), 0x4000);
    assert_eq!(thread.robust_list_head(), 0x5000);
    let stack = signal.stack();
    assert_eq!((stack.sp, stack.flags, stack.size), (0x6000, 0, 0x2000));
    assert_eq!(format!("{uctx:?}"), context);
    assert!(!thread.is_exiting());
    thread.set_clear_child_tid(original_tid);
    thread.set_robust_list_head(original_robust);
    signal.set_stack(original_stack);
}
