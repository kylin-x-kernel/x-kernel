// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::Cell;

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::serial::test_support::SerialPortFixture;

fn new_state() -> SpinNoIrq<InputIrqState> {
    SpinNoIrq::new(InputIrqState {
        has_attempted: false,
        registration: None,
    })
}

fn descriptor() -> IrqDesc {
    IrqDesc::new(4, kirq::IrqTrigger::EdgeRising)
}

fn repeat_without_work(state: &SpinNoIrq<InputIrqState>, fixture: &SerialPortFixture) {
    prepare_input_with(
        state,
        Some((fixture.port(), descriptor())),
        |_| panic!("duplicate setup prepared storage"),
        |_| panic!("duplicate setup mapped an IRQ"),
        |_, _| panic!("duplicate setup registered an action"),
    );
}

#[def_test]
fn absent_irq_keeps_polled_io_and_does_not_retry() {
    let state = new_state();
    let fixture = SerialPortFixture::new(b"rx");
    prepare_input_with(
        &state,
        None,
        |_| panic!("absent IRQ prepared storage"),
        |_| panic!("absent IRQ was mapped"),
        |_, _| panic!("absent IRQ registered an action"),
    );
    repeat_without_work(&state, &fixture);
    assert!(state.lock().registration.is_none());
    assert_eq!(fixture.receive_enable_count(), 0);
    let mut bytes = [0; 2];
    assert_eq!(fixture.port().read_data(&mut bytes), 2);
    assert_eq!(&bytes, b"rx");
    fixture.port().write_data(b"tx");
    let output = fixture.output();
    assert_eq!(output.as_slice(), b"tx");
}

#[derive(Clone, Copy)]
enum Failure {
    Buffer,
    Mapping,
    RejectedAction,
    ActionError,
}

#[def_test]
fn setup_failures_never_enable_rx_or_publish_readiness() {
    for failure in [
        Failure::Buffer,
        Failure::Mapping,
        Failure::RejectedAction,
        Failure::ActionError,
    ] {
        let state = new_state();
        let fixture = SerialPortFixture::new(b"rx");
        let stage = Cell::new(0);
        prepare_input_with(
            &state,
            Some((fixture.port(), descriptor())),
            |port| {
                stage.set(1);
                if matches!(failure, Failure::Buffer) {
                    // Deterministic capacity failure, not global allocator injection.
                    alloc::vec::Vec::<u8>::new().try_reserve(usize::MAX)
                } else {
                    port.prepare_irq_input()
                }
            },
            |_| {
                core::assert_eq!(stage.get(), 1);
                stage.set(2);
                if matches!(failure, Failure::Mapping) {
                    Err(kirq::IrqDescError::UnknownIrq)
                } else {
                    Ok(91)
                }
            },
            |virq, _| {
                core::assert_eq!(virq, 91);
                core::assert_eq!(stage.get(), 2);
                core::assert!(state.lock().registration.is_none());
                core::assert_eq!(fixture.receive_enable_count(), 0);
                stage.set(3);
                match failure {
                    Failure::RejectedAction => Ok(None),
                    Failure::ActionError => Err(kirq::IrqDescError::UnknownIrq),
                    _ => panic!("setup continued after a preceding failure"),
                }
            },
        );
        let expected_stage = match failure {
            Failure::Buffer => 1,
            Failure::Mapping => 2,
            Failure::RejectedAction | Failure::ActionError => 3,
        };
        assert_eq!(stage.get(), expected_stage);
        repeat_without_work(&state, &fixture);
        assert!(state.lock().registration.is_none());
        assert_eq!(fixture.receive_enable_count(), 0);
        let mut bytes = [0; 2];
        assert_eq!(fixture.port().read_data(&mut bytes), 2);
        assert_eq!(&bytes, b"rx");
        fixture.port().write_data(b"tx");
        let output = fixture.output();
        assert_eq!(output.as_slice(), b"tx");
    }
}

#[def_test]
fn successful_setup_enables_rx_once_after_action_installation() {
    let state = new_state();
    let fixture = SerialPortFixture::new(b"rx");
    let stage = Cell::new(0);
    prepare_input_with(
        &state,
        Some((fixture.port(), descriptor())),
        |port| {
            stage.set(1);
            port.prepare_irq_input()
        },
        |_| {
            core::assert_eq!(stage.get(), 1);
            stage.set(2);
            Ok(91)
        },
        |virq, _| {
            core::assert_eq!(stage.get(), 2);
            core::assert_eq!(virq, 91);
            core::assert!(state.lock().registration.is_none());
            core::assert_eq!(fixture.receive_enable_count(), 0);
            stage.set(3);
            Ok(Some(IrqActionToken::new(7)))
        },
    );
    assert_eq!(stage.get(), 3);
    assert_eq!(fixture.receive_enable_count(), 1);
    assert_eq!(state.lock().registration, Some((91, IrqActionToken::new(7))));
    repeat_without_work(&state, &fixture);
    assert_eq!(fixture.receive_enable_count(), 1);
    assert_eq!(state.lock().registration, Some((91, IrqActionToken::new(7))));
}
