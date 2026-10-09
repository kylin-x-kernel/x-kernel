// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::{any::Any, default::Default};

#[cfg(unittest)]
use kprocess::Thread;
use kprocess::{self, TeeSessionCtxTrait};
use ksync::Mutex;
use slab::Slab;
use tee_raw_sys::*;
use tee_task_iface::TeeTaCtx;

use crate::tee::{
    TeeResult,
    tee_obj::{TeeObj, tee_obj_close_all},
    tee_svc_cryp2::{TeeCrypState, tee_cryp_state_close_all},
    tee_svc_storage::{TeeStorageEnum, tee_svc_storage_close_all_enum},
};

/// TEE Session Context
/// This context stores per-session information and state for a client session
///
/// Parameters:
/// - session_id: Unique identifier for this session
/// - login_type: Type of login/authentication used for this session
/// - user_id: Identifier of the user associated with this session
///
/// This structure is attached to each thread handling a client session
pub(super) struct TeeSessionCtx {
    pub clnt_id: TEE_Identity,
    pub cancel: bool,
    pub cancel_mask: bool,
    pub cancel_time: TeeTime,
    pub objects: Slab<Arc<Mutex<TeeObj>>>,
    pub storage_enums: Slab<Arc<Mutex<TeeStorageEnum>>>,
    pub cryp_state: Slab<Arc<Mutex<TeeCrypState>>>,
}

impl TeeSessionCtxTrait for TeeSessionCtx {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl Default for TeeSessionCtx {
    fn default() -> Self {
        TeeSessionCtx {
            clnt_id: TEE_Identity {
                login: 0,
                uuid: TEE_UUID {
                    timeLow: 0,
                    timeMid: 0,
                    timeHiAndVersion: 0,
                    clockSeqAndNode: [0; 8],
                },
            },
            cancel: false,
            cancel_mask: false,
            cancel_time: TeeTime {
                seconds: 0,
                millis: 0,
            },
            objects: Slab::new(),
            storage_enums: Slab::new(),
            cryp_state: Slab::new(),
        }
    }
}

/// Acquire a mutable reference to the current thread's tee_session_ctx
/// Executes the provided closure with the mutable reference
///
/// # Parameters
/// - `f`: Closure that takes `&mut TeeSessionCtx` and returns `TeeResult<R>`
///
/// # Returns
/// The result of the closure execution
///
/// # Note
/// Creates a default session context if none exists for the current thread
pub(super) fn with_tee_session_ctx_mut<F, R>(f: F) -> TeeResult<R>
where
    F: FnOnce(&mut TeeSessionCtx) -> TeeResult<R>,
{
    kprocess::with_current_user_thread(|thread| {
        thread.set_tee_session_ctx(Box::new(TeeSessionCtx::default()));

        thread.with_tee_session_ctx_mut(|lock| {
            let concrete = {
                let boxed = lock.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;
                boxed
                    .as_any_mut()
                    .downcast_mut::<TeeSessionCtx>()
                    .ok_or(TEE_ERROR_BAD_STATE)?
            };

            f(concrete)
        })
    })
}

/// Acquire an immutable reference to the current thread's tee_session_ctx
/// Executes the provided closure with the immutable reference
///
/// # Parameters
/// - `f`: Closure that takes `&TeeSessionCtx` and returns `TeeResult<R>`
///
/// # Returns
/// The result of the closure execution
///
/// # Note
/// Creates a default session context if none exists for the current thread
pub(super) fn with_tee_session_ctx<F, R>(f: F) -> TeeResult<R>
where
    F: FnOnce(&TeeSessionCtx) -> TeeResult<R>,
{
    kprocess::with_current_user_thread(|thread| {
        thread.set_tee_session_ctx(Box::new(TeeSessionCtx::default()));

        thread.with_tee_session_ctx(|lock| {
            let concrete = {
                let boxed = lock.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
                boxed
                    .as_any()
                    .downcast_ref::<TeeSessionCtx>()
                    .ok_or(TEE_ERROR_BAD_STATE)?
            };

            f(concrete)
        })
    })
}

/// Release session-scoped TEE resources before TA thread/process exit.
///
/// Closes all open objects, storage enumerators, and crypto states in the current
/// thread's session context. Only runs when that context already exists.
///
/// # Caller context
///
/// Must not be called while `tee_session_ctx` is already locked on the current thread
/// (non-reentrant `Mutex`). Intended for thread exit after syscall/trap handling returns.
pub fn tee_session_release_state() -> TeeResult {
    let has_ctx = kprocess::with_current_user_thread(|thread| {
        thread.with_tee_session_ctx(|tee_session_ctx| tee_session_ctx.is_some())
    });
    if !has_ctx {
        return Ok(());
    }

    tee_obj_close_all()?;
    tee_svc_storage_close_all_enum()?;
    tee_cryp_state_close_all()?;
    Ok(())
}

#[cfg(unittest)]
pub fn set_tee_session_ctx(thread: &Thread) {
    thread.set_tee_session_ctx(Box::new(TeeSessionCtx::default()));
}

#[cfg(unittest)]
pub use exec_fixture::ExecSessionFixture;

#[unittest::mod_test]
mod exec_fixture {
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
}

/// Acquire a mutable reference to the global tee_ta_ctx
/// Executes the provided closure with the mutable reference
/// The closure pattern ensures proper lock release
///
/// # Parameters
/// - `f`: Closure that takes `&mut tee_ta_ctx` and returns `TeeResult<R>`
///
/// # Returns
/// The result of the closure execution
pub(super) fn with_tee_ta_ctx_mut<F, R>(f: F) -> TeeResult<R>
where
    F: FnOnce(&mut TeeTaCtx) -> TeeResult<R>,
{
    let process = kprocess::current_user_process();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: A running TEE user session retains its \
                  live process TEE context during the call"
    )]
    process
        .with_tee_ta_ctx_mut(f)
        .expect("current tee session requires a live process tee context")
}

/// Acquire an immutable reference to the global tee_ta_ctx
/// Executes the provided closure with the immutable reference
/// The closure pattern ensures proper lock release
///
/// # Parameters
/// - `f`: Closure that takes `&tee_ta_ctx` and returns `TeeResult<R>`
///
/// # Returns
/// The result of the closure execution
pub(super) fn with_tee_ta_ctx<F, R>(f: F) -> TeeResult<R>
where
    F: FnOnce(&TeeTaCtx) -> TeeResult<R>,
{
    let process = kprocess::current_user_process();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: A running TEE user session retains its \
                  live process TEE context during the call"
    )]
    process
        .with_tee_ta_ctx(f)
        .expect("current tee session requires a live process tee context")
}

// Test module for TEE session functionality
// Only compiled when the tee_test feature is enabled
#[unittest::mod_test]
pub(super) mod tests_tee_session {
    use alloc::string::String;

    use unittest::assert_eq;

    use super::*;

    // Test function for with_tee_ta_ctx helper functions
    #[unittest::def_test(user)]
    fn test_with_tee_ta_ctx() {
        let mut uuid_back: String = String::new();

        with_tee_ta_ctx(|ta_ctx| {
            uuid_back = ta_ctx.uuid.clone();
            Ok(())
        })
        .unwrap();
        assert_eq!(uuid_back, "00000000-0000-0000-0000-000000000000");

        with_tee_ta_ctx_mut(|ta_ctx| {
            ta_ctx.uuid = String::from("936da01f-9abd-4d9d-80c7-02af85c822a8");
            Ok(())
        })
        .unwrap();

        let mut uuid_modify: String = String::new();
        with_tee_ta_ctx(|ta_ctx| {
            uuid_modify = ta_ctx.uuid.clone();
            Ok(())
        })
        .unwrap();
        assert_eq!(uuid_modify, "936da01f-9abd-4d9d-80c7-02af85c822a8");

        with_tee_ta_ctx_mut(|ta_ctx| {
            ta_ctx.uuid = uuid_back;
            Ok(())
        })
        .unwrap();
    }
}
