// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn vt_modes_reject_invalid_values_without_mutating_state() {
    let mut state = ConsoleState::default();
    state.set_display_mode(1).unwrap();
    state.set_keyboard_mode(4).unwrap();
    for value in [2, 3, 4, usize::MAX] {
        assert_eq!(state.set_display_mode(value), Err(KError::InvalidInput));
        assert_eq!(state.display, DisplayMode::Graphics);
    }
    for value in [5, usize::MAX] {
        assert_eq!(state.set_keyboard_mode(value), Err(KError::InvalidInput));
        assert_eq!(state.keyboard, KeyboardMode::Off);
    }
    for value in [0, 1, 2] {
        assert_eq!(
            state.set_keyboard_mode(value),
            Err(KError::OperationNotSupported)
        );
        assert_eq!(state.keyboard, KeyboardMode::Off);
    }
    assert_eq!(
        state.set_mode(
            VtMode {
                mode: 1,
                ..VtMode::automatic()
            },
            None
        ),
        Err(KError::NoSuchProcess)
    );
    assert_eq!(state.mode, VtMode::automatic());
    assert!(state.controller.is_none());
    state.set_display_mode(0).unwrap();
    state.set_keyboard_mode(3).unwrap();
    assert_eq!(state.display, DisplayMode::Text);
    assert_eq!(state.keyboard, KeyboardMode::Unicode);
    assert_eq!(VtMode::automatic().mode, 0);
}

#[def_test]
fn vt_fixed_console_rejects_unimplemented_switches() {
    assert_eq!(require_active_console(1), Ok(()));
    for number in [0, 2, 63, usize::MAX] {
        assert_eq!(require_active_console(number), Err(KError::InvalidInput));
    }
}

#[def_test]
fn vt_uapi_layout_and_process_mode_validation() {
    assert_eq!(core::mem::size_of::<VtMode>(), 8);
    assert_eq!(core::mem::size_of::<VtState>(), 6);
    let state = fixed_vt_state();
    assert_eq!(state.active, 1);
    assert_eq!(state.signal, 0);
    assert_eq!(state.allocated, 3);
    let automatic = VtMode::automatic();
    assert_eq!(automatic.validate(), Ok(automatic));
    assert_eq!(
        VtMode {
            mode: 1,
            ..automatic
        }
        .validate(),
        Ok(VtMode {
            mode: 1,
            ..automatic
        })
    );
    // Linux retains AUTO/PROCESS fields and only normalizes ignored frsig.
    for request in [
        VtMode {
            relsig: -1,
            ..automatic
        },
        VtMode {
            acqsig: 65,
            ..automatic
        },
        VtMode {
            frsig: i16::MIN,
            ..automatic
        },
    ] {
        let normalized = request.validate().unwrap();
        assert_eq!(
            normalized,
            VtMode {
                frsig: 0,
                ..request
            }
        );
        assert_eq!(normalized.frsig, 0);
    }
    for mode in [2, 65, u8::MAX] {
        assert_eq!(
            VtMode { mode, ..automatic }.validate(),
            Err(KError::InvalidInput)
        );
    }
}

#[def_test(user)]
fn vt_process_registration_keeps_identity_and_idle_ack_contract() {
    let caller = kprocess::current_user_process();
    let mut state = ConsoleState::default();
    let mode = VtMode {
        mode: 1,
        waitv: 7,
        relsig: 10,
        acqsig: 12,
        frsig: -1,
    };
    state.set_mode(mode, Some(&caller)).unwrap();
    let selected = state.controller.as_ref().unwrap().upgrade().unwrap();
    assert!(Arc::ptr_eq(&caller, &selected));
    assert_eq!(state.mode.mode, 1);
    assert_eq!(state.mode.waitv, 7);
    assert_eq!(state.mode.relsig, 10);
    assert_eq!(state.mode.acqsig, 12);
    assert_eq!(state.mode.frsig, 0);
    state.refresh_controller();
    assert_eq!(state.mode.mode, 1);
    assert_eq!(state.acknowledge(VT_ACKACQ), Ok(()));
    for request in [0, 1, 3, usize::MAX] {
        assert_eq!(state.acknowledge(request), Err(KError::InvalidInput));
    }
    state.set_mode(VtMode::automatic(), None).unwrap();
    assert!(state.controller.is_none());
    assert_eq!(state.acknowledge(VT_ACKACQ), Err(KError::InvalidInput));
}

#[def_test]
fn vt_permissions_follow_terminal_identity_and_current_credentials() {
    let target: Arc<dyn ControllingTerminal> = new_console();
    let other: Arc<dyn ControllingTerminal> = new_console();
    let unprivileged = kcred::Cred::new(1000, 1000);
    assert_eq!(
        configuration_permission(&unprivileged, &target, None),
        Err(KError::OperationNotPermitted)
    );
    assert_eq!(
        configuration_permission(&unprivileged, &target, Some(&other)),
        Err(KError::OperationNotPermitted)
    );
    assert_eq!(
        configuration_permission(&unprivileged, &target, Some(&target)),
        Ok(())
    );
    assert_eq!(
        configuration_permission(&kcred::Cred::root(), &target, None),
        Ok(())
    );
    let first = new_console();
    let second = new_console();
    assert!(!Arc::ptr_eq(&first.terminal, &second.terminal));
    assert!(!Arc::ptr_eq(&first.terminal, &super::super::N_TTY.terminal));
}
