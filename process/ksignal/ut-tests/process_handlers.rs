// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[cfg(unittest)]
#[unittest::def_test]
fn test_captured_notification_survives_exec_without_owning_handlers() {
    let manager = ProcessSignalManager::new(SignalActions::default(), 0);
    let source = {
        let owner = manager.handlers.lock();
        let handlers = owner.as_ref().unwrap();
        manager.put_pending_signal(SignalInfo::new_kernel(Signo::SIGUSR1));
        handlers.pending_changed.clone()
    };
    let before = Arc::as_ptr(manager.handlers.lock().as_ref().unwrap());
    manager.reset_actions_for_exec().unwrap();
    unittest::assert_eq!(
        before,
        Arc::as_ptr(manager.handlers.lock().as_ref().unwrap())
    );

    let peer = manager.fork_shared().unwrap();
    let old_registration = source.register(core::task::Waker::noop()).unwrap();
    manager.reset_actions_for_exec().unwrap();
    let current = {
        let owner = manager.handlers.lock();
        owner.as_ref().unwrap().pending_changed.clone()
    };
    let new_registration = current.register(core::task::Waker::noop()).unwrap();
    // Drive the delayed notification only after exec has detached the sender.
    source.wake();
    unittest::assert!(!old_registration.cancel());
    unittest::assert!(new_registration.cancel());
    unittest::assert!(manager.pending().has(Signo::SIGUSR1));
    unittest::assert!(peer.pending().is_empty());
}
#[cfg(unittest)]
#[unittest::def_test]
fn released_manager_cannot_retain_or_recreate_handler_ownership() {
    let parent = ProcessSignalManager::new(SignalActions::default(), 0);
    let child = Arc::new(parent.fork_shared().unwrap());
    let observer = child.clone();
    let original = Arc::downgrade(parent.handlers.lock().as_ref().unwrap());
    child.release_handlers();
    child.release_handlers();
    unittest::assert_eq!(original.strong_count(), 1);
    unittest::assert_eq!(
        observer.can_restart(Signo::SIGUSR1),
        Err(KError::NoSuchProcess)
    );
    unittest::assert_eq!(
        observer.signal_ignored(Signo::SIGUSR1),
        Err(KError::NoSuchProcess)
    );
    unittest::assert!(matches!(
        observer.signal_action(Signo::SIGUSR1),
        Err(KError::NoSuchProcess)
    ));
    unittest::assert!(matches!(
        observer.replace_signal_action(Signo::SIGUSR1, None),
        Err(KError::NoSuchProcess)
    ));
    unittest::assert_eq!(
        observer.set_signal_action(Signo::SIGUSR1, SignalAction::default()),
        Err(KError::NoSuchProcess)
    );
    unittest::assert!(matches!(observer.fork_shared(), Err(KError::NoSuchProcess)));
    unittest::assert!(matches!(
        observer.fork_private(),
        Err(KError::NoSuchProcess)
    ));
    unittest::assert_eq!(
        observer.reset_actions_for_exec(),
        Err(KError::NoSuchProcess)
    );
    unittest::assert_eq!(
        observer.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)),
        None
    );
    let thread = ThreadSignalManager::new(8_150, observer);
    unittest::assert!(!thread.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)));
    unittest::assert!(thread.pending().is_empty());
    unittest::assert!(child.pending().is_empty());
    parent.reset_actions_for_exec().unwrap();
    unittest::assert_eq!(
        original.as_ptr(),
        Arc::as_ptr(parent.handlers.lock().as_ref().unwrap())
    );
    parent.release_handlers();
    unittest::assert!(original.upgrade().is_none());
}
#[cfg(unittest)]
#[unittest::def_test]
fn test_unshared_exec_retains_table_after_prepared_child_drop() {
    let manager = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let borrowed_manager = manager.clone();
    let before = Arc::as_ptr(manager.handlers.lock().as_ref().unwrap());
    let prepared_child = manager.fork_shared().unwrap();
    drop(prepared_child);
    manager.reset_actions_for_exec().unwrap();
    unittest::assert_eq!(
        before,
        Arc::as_ptr(borrowed_manager.handlers.lock().as_ref().unwrap())
    );
    let child = manager.fork_shared().unwrap();
    child.reset_actions_for_exec().unwrap();
    unittest::assert!(before != Arc::as_ptr(child.handlers.lock().as_ref().unwrap()));
    unittest::assert_eq!(
        before,
        Arc::as_ptr(manager.handlers.lock().as_ref().unwrap())
    );
}
