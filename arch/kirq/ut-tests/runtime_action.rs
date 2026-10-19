// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::def_test;

use super::{IrqAction, IrqActionFlags, IrqActionId, IrqActionReturn, IrqActionToken};
use crate::{IrqEvent, Virq};

fn handled_handler(_irq: Virq) -> IrqEvent {
    IrqEvent::HANDLED
}

#[def_test]
fn test_irq_action_regular_construction_is_non_threaded() {
    let action = IrqAction::regular(Arc::new(handled_handler));

    assert_eq!(action.id(), IrqActionId::REGULAR);
    assert_eq!(action.token(), IrqActionToken::new(0));
    assert_eq!(action.flags(), IrqActionFlags::empty());
    assert_eq!(action.name(), None);
    assert!(action.is_currently_dispatchable());
}

#[def_test]
fn test_irq_action_shared_construction_has_token() {
    let token = IrqActionToken::new(7);
    let action = IrqAction::shared(token, Arc::new(handled_handler));

    assert_eq!(action.token(), token);
    assert_eq!(action.flags(), IrqActionFlags::SHARED);
    assert!(action.is_shared());
    assert!(action.is_currently_dispatchable());
}

#[def_test]
fn test_irq_action_thread_slot_is_future_only() {
    let action = IrqAction::test_new(
        Arc::new(handled_handler),
        Some(super::IrqThreadSlot),
        IrqActionFlags::NO_THREAD,
        Some("future"),
    );

    assert_eq!(action.id(), IrqActionId::REGULAR);
    assert_eq!(action.flags(), IrqActionFlags::NO_THREAD);
    assert_eq!(action.name(), Some("future"));
    assert!(!action.is_currently_dispatchable());
}

#[def_test]
fn test_irq_action_return_maps_public_events() {
    assert_eq!(
        IrqActionReturn::from_event(IrqEvent::HANDLED),
        IrqActionReturn::Handled { sources: 0 }
    );
    assert_eq!(
        IrqActionReturn::from_event(IrqEvent::from_sources(0b1010_0001)),
        IrqActionReturn::Handled {
            sources: 0b1010_0001
        }
    );
    assert_eq!(
        IrqActionReturn::from_event(IrqEvent::NOT_HANDLED),
        IrqActionReturn::Unhandled
    );
}

#[def_test]
fn test_not_handled_is_not_wake_thread() {
    let action_return = IrqActionReturn::from_event(IrqEvent::NOT_HANDLED);

    assert!(!matches!(action_return, IrqActionReturn::WakeThread { .. }));
}

#[def_test]
fn test_wake_thread_return_keeps_sources() {
    let action_return = IrqActionReturn::WakeThread { sources: 0x5a };

    assert_eq!(action_return.sources(), 0x5a);
}
