// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test(user)]
    fn default_cancellation_is_inactive_after_unmask() {
        with_tee_session_ctx_mut(|ctx| {
            ctx.cancel_mask = false;
            Ok(())
        })
        .unwrap();
        assert!(!with_tee_session_ctx(|ctx| Ok(tee_ta_session_is_cancelled(ctx, None))).unwrap());
    }
    #[def_test(user)]
    fn timed_wait_cancels_and_next_request_reuses_thread_state() {
        use crate::tee::tee_time::sys_tee_scn_wait;
        sys_tee_scn_invoke_scope(2, 0).unwrap();
        with_tee_session_ctx_mut(|ctx| {
            ctx.cancel_mask = false;
            ctx.clnt_id.login = 73;
            Ok(())
        })
        .unwrap();
        assert_eq!(sys_tee_scn_wait(1), Ok(()));
        let deadline = ktask::monotonic_time() + TimeSpan::from_millis(30);
        sys_tee_scn_invoke_scope(0, deadline.as_nanos_u64_saturating()).unwrap();
        assert_eq!(sys_tee_scn_wait(200), Err(TEE_ERROR_CANCEL));
        assert!(ktask::monotonic_time() >= deadline);
        sys_tee_scn_invoke_scope(1, 0).unwrap();
        assert!(!with_tee_session_ctx(|ctx| Ok(tee_ta_session_is_cancelled(ctx, None))).unwrap());
        assert_eq!(
            with_tee_session_ctx(|ctx| Ok(ctx.clnt_id.login)).unwrap(),
            73
        );
        assert_eq!(sys_tee_scn_wait(1), Ok(()));
    }

    #[def_test(user)]
    fn mask_hides_expired_deadline_until_unmask() {
        use crate::tee::tee_time::sys_tee_scn_wait;
        sys_tee_scn_invoke_scope(0, 0).unwrap();
        with_tee_session_ctx_mut(|ctx| {
            ctx.cancel_mask = true;
            Ok(())
        })
        .unwrap();
        assert!(!with_tee_session_ctx(|ctx| Ok(tee_ta_session_is_cancelled(ctx, None))).unwrap());
        assert_eq!(sys_tee_scn_wait(1), Ok(()));
        with_tee_session_ctx_mut(|ctx| {
            ctx.cancel_mask = false;
            Ok(())
        })
        .unwrap();
        assert_eq!(sys_tee_scn_wait(0), Err(TEE_ERROR_CANCEL));
        sys_tee_scn_invoke_scope(1, 0).unwrap();
    }

    #[def_test(user)]
    fn invalid_scope_does_not_replace_deadline() {
        sys_tee_scn_invoke_scope(0, 123).unwrap();
        assert_eq!(
            sys_tee_scn_invoke_scope(3, 0),
            Err(TEE_ERROR_BAD_PARAMETERS)
        );
        assert_eq!(
            sys_tee_scn_invoke_scope(1, 1),
            Err(TEE_ERROR_BAD_PARAMETERS)
        );
        assert_eq!(
            with_tee_session_ctx(|ctx| Ok(ctx.cancel_deadline.unwrap().as_nanos_u64_saturating()))
                .unwrap(),
            123
        );
        sys_tee_scn_invoke_scope(1, 0).unwrap();
    }
