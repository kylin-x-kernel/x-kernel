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

unittest::path_mod_test!(
    exec_fixture,
    "../../ut-tests/tee/tee_session/exec_fixture.rs"
);

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
// Compiled only in the cfg(unittest) guest test image.
unittest::path_mod_test!(pub(super) mod tests_tee_session, "../../ut-tests/tee/tee_session/tests_tee_session.rs");
