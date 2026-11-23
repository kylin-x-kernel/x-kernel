// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

/// DesignWare reports busy detect as `IIR[3:0] == 0x7`, whose bit 0 is set.
/// Decoding by bit 0 — the classic 16550 "no interrupt pending" test —
/// classifies it as an idle line, so nothing ever reads `USR` and the
/// interrupt line stays asserted; on a level-triggered GIC input that is an
/// unbounded storm. `0xC7` is the FIFO-enabled readback: `IIR[7:6]` mirror
/// `FCR[7:6]`.
#[def_test]
fn busy_detect_is_not_decoded_as_nothing_pending() {
    assert_eq!(IirSource::decode(0xC7), IirSource::BusyDetect);
    assert_eq!(IirSource::decode(0x07), IirSource::BusyDetect);
    assert_eq!(IirSource::decode(0xC1), IirSource::None);
}

/// The receiver-side sources must stay distinct from busy detect: they are
/// deasserted by draining the FIFO and reading `LSR`, not by a `USR` read.
#[def_test]
fn receiver_sources_decode_distinctly_from_busy_detect() {
    assert_eq!(IirSource::decode(0xC4), IirSource::Receive);
    assert_eq!(IirSource::decode(0xCC), IirSource::Receive);
    assert_eq!(IirSource::decode(0xC6), IirSource::LineStatus);
}

/// Identifiers this driver neither enables nor services must not be reported
/// as serviced, so the unclaimed-interrupt watchdog still sees them.
#[def_test]
fn unserviced_identifiers_are_not_claimed() {
    assert_eq!(IirSource::decode(0xC0), IirSource::Other);
    assert_eq!(IirSource::decode(0xC2), IirSource::Other);
}
