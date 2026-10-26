// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::nodes::test_device::open_character_node;

/// `/dev/rtc0` is `rtc_dev_fops`, which provides `rtc_dev_poll`: `epoll_ctl`
/// accepts the node, and with no interrupt queue nothing is ever pending.
#[def_test(serial)]
fn dev_rtc_provides_a_poll_handler() {
    let file = open_character_node("rtc0", Arc::new(Rtc));
    assert!(file.can_poll());
    assert_eq!(file.poll_handler().map(|events| events.bits()), Some(0));
}