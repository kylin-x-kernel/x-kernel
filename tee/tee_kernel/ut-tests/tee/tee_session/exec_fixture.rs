// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Weak;
use core::cell::Cell;

use super::*;
use crate::tee::{
    tee_obj::{tee_obj_add, tee_obj_get},
    tee_svc_cryp2::{
        CrypCtx, CrypState, HashContext, tee_cryp_hash_init, tee_cryp_hash_update,
        tee_cryp_state_alloc, tee_cryp_state_get,
    },
    tee_svc_storage::tee_svc_storage_add_enum,
};

/// Real session resources observed across exec without owning the session.
pub struct ExecSessionFixture {
    session_identity: usize,
    object_id: usize,
    enum_id: usize,
    crypto: Arc<Mutex<TeeCrypState>>,
    object: Weak<Mutex<TeeObj>>,
    enumerator: Weak<Mutex<TeeStorageEnum>>,
    live_checks: Cell<usize>,
}

impl ExecSessionFixture {
    /// Provisions the current fresh thread using normal resource APIs.
    /// No session/resource guard escapes setup; only crypto has a strong observer.
    pub fn provision() -> Self {
        let thread = kprocess::current_user_thread();
        thread.with_tee_session_ctx(|slot| assert!(slot.is_none()));
        let object_id = tee_obj_add(TeeObj::default()).unwrap() as usize;
        let object = Arc::downgrade(&tee_obj_get(object_id as _).unwrap());
        // These registries are real, but have no persistent backend or open dir.
        let enum_id = tee_svc_storage_add_enum(TeeStorageEnum {
            id: 0,
            dir: None,
            fops: None,
        })
        .unwrap() as usize;
        let mut crypto_id = 0;
        tee_cryp_state_alloc(
            TEE_ALG_SHA256,
            TEE_OperationMode::TEE_MODE_DIGEST,
            None,
            None,
            &mut crypto_id,
        )
        .unwrap();
        tee_cryp_hash_init(crypto_id).unwrap();
        tee_cryp_hash_update(crypto_id, b"exec session cleanup").unwrap();
        let crypto = tee_cryp_state_get(crypto_id).unwrap();
        let (session_identity, enumerator) = thread.with_tee_session_ctx(|slot| {
            let ctx = slot
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<TeeSessionCtx>()
                .unwrap();
            (
                ctx as *const TeeSessionCtx as usize,
                Arc::downgrade(&ctx.storage_enums[enum_id]),
            )
        });
        let fixture = Self {
            session_identity,
            object_id,
            enum_id,
            crypto,
            object,
            enumerator,
            live_checks: Cell::new(0),
        };
        fixture.assert_live(&thread);
        fixture
    }

    /// Checks the same session and populated tables without lazy recreation.
    pub fn assert_live(&self, thread: &Thread) {
        let crypto_id = self.crypto.lock().id as usize;
        thread.with_tee_session_ctx(|slot| {
            let ctx = slot
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<TeeSessionCtx>()
                .unwrap();
            assert_eq!(ctx as *const TeeSessionCtx as usize, self.session_identity);
            assert_eq!(
                (
                    ctx.objects.len(),
                    ctx.storage_enums.len(),
                    ctx.cryp_state.len()
                ),
                (1, 1, 1)
            );
            assert_eq!(
                Arc::as_ptr(&ctx.objects[self.object_id - 1]),
                self.object.as_ptr()
            );
            assert_eq!(
                Arc::as_ptr(&ctx.storage_enums[self.enum_id]),
                self.enumerator.as_ptr()
            );
            assert!(Arc::ptr_eq(&ctx.cryp_state[crypto_id], &self.crypto));
        });
        {
            let crypto = self.crypto.lock();
            assert_eq!(crypto.algo, TEE_ALG_SHA256);
            assert_eq!(crypto.state, CrypState::Initialized);
            assert!(crypto.key1.is_none() && crypto.key2.is_none());
            assert!(matches!(
                crypto.ctx,
                CrypCtx::HashCtx(HashContext::Sha256(_))
            ));
        }
        self.live_checks.set(self.live_checks.get() + 1);
    }

    /// Distinguishes active fatal cleanup from success-only session removal.
    /// Consumes all strong resource observations before checking reclamation.
    pub fn finish(self, thread: &Thread, is_fatal: bool, expected_live_checks: usize) {
        assert_eq!(self.live_checks.get(), expected_live_checks);
        thread.with_tee_session_ctx(|slot| {
            if is_fatal {
                let ctx = slot
                    .as_ref()
                    .unwrap()
                    .as_any()
                    .downcast_ref::<TeeSessionCtx>()
                    .unwrap();
                assert_eq!(ctx as *const TeeSessionCtx as usize, self.session_identity);
                assert!(
                    ctx.objects.is_empty()
                        && ctx.storage_enums.is_empty()
                        && ctx.cryp_state.is_empty()
                );
            } else {
                assert!(slot.is_none());
            }
        });
        {
            let crypto = self.crypto.lock();
            if is_fatal {
                assert!(matches!(crypto.ctx, CrypCtx::Others));
            } else {
                // Success drops session ownership; it does not call state_free.
                assert!(matches!(
                    crypto.ctx,
                    CrypCtx::HashCtx(HashContext::Sha256(_))
                ));
            }
        }
        assert!(self.object.upgrade().is_none());
        assert!(self.enumerator.upgrade().is_none());
        let crypto = Arc::downgrade(&self.crypto);
        drop(self);
        assert!(crypto.upgrade().is_none());
    }
}
