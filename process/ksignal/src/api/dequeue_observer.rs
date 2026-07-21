// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-signal dequeue observers.

use kspin::SpinNoIrq;

use crate::{MAX_SIGNALS, SignalInfo, Signo};

/// The observer's decision for a dequeued signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalDequeueAction {
    /// Delivers the signal to normal signal handling.
    Deliver,
    /// Drops the signal after dequeue.
    Drop,
}

type ObserverFn = fn(&SignalInfo) -> SignalDequeueAction;

const NONE: Option<ObserverFn> = None;

static OBSERVERS: SpinNoIrq<[Option<ObserverFn>; MAX_SIGNALS]> =
    SpinNoIrq::new([NONE; MAX_SIGNALS]);

/// Registers an observer invoked whenever `signo` is dequeued.
///
/// Only one observer per signal is supported; a second registration
/// overwrites the previous one.
///
/// The observer runs synchronously in the context that dequeues the signal.
/// Callbacks must therefore only rely on execution-context properties that are
/// guaranteed by every dequeue path that can deliver `signo`.
pub fn register_signal_observer(signo: Signo, observer: ObserverFn) {
    let idx = signo as usize;
    assert!((1..=MAX_SIGNALS).contains(&idx));
    OBSERVERS.lock()[idx - 1] = Some(observer);
}

/// Removes the observer previously registered for `signo`.
pub fn unregister_signal_observer(signo: Signo) {
    let idx = signo as usize;
    assert!((1..=MAX_SIGNALS).contains(&idx));
    OBSERVERS.lock()[idx - 1] = None;
}

/// Notifies the registered observer for `sig`, if any.
pub(crate) fn notify_signal_dequeued(sig: &SignalInfo) -> SignalDequeueAction {
    let idx = sig.signo() as usize;
    if !(1..=MAX_SIGNALS).contains(&idx) {
        return SignalDequeueAction::Deliver;
    }
    let observer = OBSERVERS.lock()[idx - 1];
    observer.map_or(SignalDequeueAction::Deliver, |observer_fn| observer_fn(sig))
}
