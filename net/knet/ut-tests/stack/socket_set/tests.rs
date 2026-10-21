// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use smoltcp::{socket::tcp, time::Instant};
use unittest::{assert_eq, def_test};

use super::{ORPHAN_FIN_WAIT2_TIMEOUT, orphan_fin_wait2_deadline};

#[def_test]
fn only_orphan_fin_wait2_gets_a_cleanup_deadline() {
    let current = Instant::from_millis(10);

    for state in [
        tcp::State::FinWait1,
        tcp::State::Closing,
        tcp::State::LastAck,
        tcp::State::TimeWait,
    ] {
        assert_eq!(orphan_fin_wait2_deadline(state, current, None), None);
    }
    assert_eq!(
        orphan_fin_wait2_deadline(tcp::State::FinWait2, current, None),
        Some(current + ORPHAN_FIN_WAIT2_TIMEOUT)
    );
}

#[def_test]
fn orphan_fin_wait2_keeps_its_original_cleanup_deadline() {
    let current = Instant::from_millis(10);
    let deadline = current + ORPHAN_FIN_WAIT2_TIMEOUT;

    assert_eq!(
        orphan_fin_wait2_deadline(
            tcp::State::FinWait2,
            Instant::from_millis(20),
            Some(deadline)
        ),
        Some(deadline)
    );
}
