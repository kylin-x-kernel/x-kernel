// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Class-registry retirement protocol tests.
//!
//! These tests exercise the generic registry directly, so they can hold a
//! retirement claim open and observe exactly what publication and a duplicate
//! event see while the reservation exists.

use alloc::sync::Arc;

use driver_base::DeviceKind;
use kdevice::{DeviceId, DeviceObject};
use unittest::{assert, assert_eq, def_test};

use crate::{Event, EventType, RevocationOutcome};

struct Fixture {
    parent: Arc<DeviceObject>,
}

fn fixture(id: u64) -> Fixture {
    let parent = Arc::new(DeviceObject::new_bound_for_test(
        DeviceId::new(id),
        DeviceKind::Char,
    ));
    let _binding = parent.binding_ref().expect("bound fixture has a binding");
    Fixture { parent }
}

/// A minimal char runtime for the production publication/retirement path.
#[cfg(feature = "char")]
struct TestCharRuntime;

#[cfg(feature = "char")]
impl driver_base::Device for TestCharRuntime {
    fn name(&self) -> &str {
        "retirement-char"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }
}

#[cfg(feature = "char")]
impl crate::CharDevice for TestCharRuntime {
    fn read(&self, _buf: &mut [u8]) -> driver_base::DriverResult<usize> {
        Ok(0)
    }

    fn write(&self, buf: &[u8]) -> driver_base::DriverResult<usize> {
        Ok(buf.len())
    }
}

/// Two publications of the same device and binding are independent.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn test_two_same_device_publications_revoke_exact_one() {
    const ID: u64 = 0xB1_2001;
    let fixture = fixture(ID);
    fixture.parent.force_active_for_test();
    let first = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("first publication succeeds");
    let second = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("second publication succeeds");

    crate::notify_char_available(fixture.parent.id());
    assert_ne!(first.publication_id(), second.publication_id());
    let first_handle = first.device();
    let second_handle = second.device();
    assert!(first_handle.is_available());
    assert!(second_handle.is_available());
    assert_eq!(crate::char_publications_for_device(fixture.parent.id()).len(), 2);

    assert_eq!(
        first.revoke().expect("first revoke completes"),
        RevocationOutcome::Completed
    );
    assert!(!first.is_available());
    assert!(!first_handle.is_available());
    assert!(second.is_available());
    assert!(second_handle.is_available());
    assert!(crate::find_char_publication(first.publication_id()).is_none());
    assert!(crate::find_char_publication(second.publication_id()).is_some());
    assert_eq!(
        crate::char_publications_for_device(fixture.parent.id()).len(),
        1
    );
    assert_eq!(
        first.revoke().expect("repeat revoke is idempotent"),
        RevocationOutcome::AlreadyRevoked
    );
}

/// A revoke request during the callback-commit window closes availability
/// before activation resumes, while the activation executor owns cleanup.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn test_revoke_during_activation_closes_availability() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    const ID: u64 = 0xB1_2002;
    let fixture = fixture(ID);
    let publication = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("publication starts in Published");
    fixture.parent.force_active_for_test();
    let handle = publication.device();
    let publication = Arc::new(publication);
    let deferred = Arc::new(AtomicUsize::new(0));
    let invisible = Arc::new(AtomicUsize::new(0));
    {
        let publication = publication.clone();
        let deferred = deferred.clone();
        let invisible = invisible.clone();
        let handle = handle.clone();
        let id = fixture.parent.id();
        crate::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let crate::test_hooks::ClassPause::BeforeCallbackCommit {
                id: paused,
                publication: paused_publication,
            } = point
                && paused == id
            {
                core::assert_eq!(
                    publication.revoke().expect("request is accepted"),
                    RevocationOutcome::Deferred
                );
                deferred.fetch_add(1, Ordering::SeqCst);
                if !handle.is_available()
                    && crate::find_char_publication(paused_publication).is_none()
                {
                    invisible.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    crate::notify_char_available(fixture.parent.id());
    crate::test_hooks::clear_pause_hook();

    assert_eq!(deferred.load(Ordering::SeqCst), 1);
    assert_eq!(invisible.load(Ordering::SeqCst), 1);
    assert!(!handle.is_available());
    assert!(crate::find_char_publication(handle.publication_id()).is_none());
    assert_eq!(
        publication.revoke().expect("already completed"),
        RevocationOutcome::AlreadyRevoked
    );
}

/// A second revoke while removed callbacks are pending must observe Deferred,
/// never AlreadyRevoked before unpublish/callback completion.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn test_second_revoke_is_deferred_until_removed_callbacks_finish() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    const ID: u64 = 0xB1_2003;
    let fixture = fixture(ID);
    let publication = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("publication");
    fixture.parent.force_active_for_test();
    crate::notify_char_available(fixture.parent.id());
    let publication = Arc::new(publication);
    let deferred = Arc::new(AtomicUsize::new(0));
    {
        let publication = publication.clone();
        let deferred = deferred.clone();
        crate::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let crate::test_hooks::ClassPause::BeforeRemovedCallback { .. } = point {
                core::assert_eq!(
                    publication.revoke().expect("second revoke accepted"),
                    RevocationOutcome::Deferred
                );
                deferred.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    assert_eq!(
        publication.revoke().expect("first revoke completes"),
        RevocationOutcome::Completed
    );
    crate::test_hooks::clear_pause_hook();
    assert_eq!(deferred.load(Ordering::SeqCst), 1);
    assert_eq!(
        publication.revoke().expect("completed"),
        RevocationOutcome::AlreadyRevoked
    );
}

/// A forced node-owner failure must enter retryable RevokeFailed, not a fake
/// Deferred, and a later revoke may complete.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn test_node_owner_error_enters_revoke_failed() {
    const ID: u64 = 0xB1_2004;
    let fixture = fixture(ID);
    let publication = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("publication");
    fixture.parent.force_active_for_test();
    crate::notify_char_available(fixture.parent.id());
    let publication_id = publication.publication_id();
    crate::test_hooks::force_node_not_owner();
    assert!(publication.revoke().is_err());
    assert!(!publication.is_available());
    assert!(crate::find_char_publication(publication_id).is_none());
    assert_eq!(
        publication.revoke().expect("retry completes"),
        RevocationOutcome::Completed
    );
}

/// Deferred revocation hides the target from public device-level enumeration
/// immediately while leaving an earlier sibling publication visible.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn test_deferred_public_enumeration_hides_only_target() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const ID: u64 = 0xB1_2005;
    let fixture = fixture(ID);
    let sibling = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("sibling publication");
    let target = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("target publication");
    fixture.parent.force_active_for_test();
    let sibling_id = sibling.publication_id();
    let target_id = target.publication_id();
    let target = Arc::new(target);
    let deferred = Arc::new(AtomicUsize::new(0));
    let target_hidden = Arc::new(AtomicBool::new(false));
    let sibling_visible = Arc::new(AtomicBool::new(false));
    {
        let target = target.clone();
        let deferred = deferred.clone();
        let target_hidden = target_hidden.clone();
        let sibling_visible = sibling_visible.clone();
        let id = fixture.parent.id();
        crate::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let crate::test_hooks::ClassPause::BeforeCallbackCommit {
                id: paused,
                publication,
            } = point
                && paused == id
                && publication == target_id
            {
                core::assert_eq!(
                    target.revoke().expect("target revoke accepted"),
                    RevocationOutcome::Deferred
                );
                deferred.fetch_add(1, Ordering::SeqCst);
                let visible = crate::char_publications_for_device(id);
                target_hidden.store(
                    !visible
                        .iter()
                        .any(|device| device.publication_id() == target_id),
                    Ordering::SeqCst,
                );
                sibling_visible.store(
                    visible
                        .iter()
                        .any(|device| device.publication_id() == sibling_id),
                    Ordering::SeqCst,
                );
            }
        }));
    }

    crate::notify_char_available(fixture.parent.id());
    crate::test_hooks::clear_pause_hook();

    assert_eq!(deferred.load(Ordering::SeqCst), 1);
    assert!(target_hidden.load(Ordering::SeqCst));
    assert!(sibling_visible.load(Ordering::SeqCst));
    assert!(sibling.is_available());
    assert!(!target.is_available());
    assert!(crate::find_char_publication(target_id).is_none());
    assert!(crate::find_char_publication(sibling_id).is_some());
}

/// Two real input publications under one binding produce two real nodes; only
/// the revoked publication's node disappears.
#[cfg(feature = "input")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn test_two_input_publications_have_independent_real_nodes() {
    use alloc::boxed::Box;
    use driver_base::{DriverError, DriverResult};
    use crate::InputDeviceId;

    struct TestInput;
    impl driver_base::Device for TestInput {
        fn name(&self) -> &str {
            "input-test"
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Input
        }
    }
    impl crate::InputDevice for TestInput {
        fn device_id(&self) -> InputDeviceId {
            InputDeviceId::UNKNOWN
        }

        fn physical_location(&self) -> &str {
            "test"
        }

        fn unique_id(&self) -> &str {
            "input-test"
        }

        fn get_event_bits(&self, _ty: EventType, _out: &mut [u8]) -> DriverResult<bool> {
            Ok(false)
        }

        fn read_event(&self) -> DriverResult<Event> {
            Err(DriverError::WouldBlock)
        }
    }

    const ID: u64 = 0xB1_2006;
    let fixture = fixture(ID);
    fixture.parent.force_active_for_test();
    let first = kdevice::ProbeContext::with_test_context(fixture.parent.clone(), |ctx| {
        crate::publish_input(ctx, Box::new(TestInput))
    })
    .expect("probe context")
    .expect("first input publication");
    let second = kdevice::ProbeContext::with_test_context(fixture.parent.clone(), |ctx| {
        crate::publish_input(ctx, Box::new(TestInput))
    })
    .expect("probe context")
    .expect("second input publication");

    let first_id = first.publication_id();
    let second_id = second.publication_id();
    let first_node = kdevice::device_nodes_snapshot()
        .into_iter()
        .find(|node| node.publication_id() == first_id)
        .expect("first real node");
    let second_node = kdevice::device_nodes_snapshot()
        .into_iter()
        .find(|node| node.publication_id() == second_id)
        .expect("second real node");
    assert_ne!(first_node.device_number(), second_node.device_number());

    assert_eq!(
        first.revoke().expect("first revoke"),
        RevocationOutcome::Completed
    );
    assert!(
        kdevice::device_nodes_snapshot()
            .iter()
            .all(|node| node.publication_id() != first_id)
    );
    assert!(
        kdevice::device_nodes_snapshot()
            .iter()
            .any(|node| node.publication_id() == second_id)
    );
    assert!(second.is_available());
    assert_eq!(
        second.revoke().expect("second revoke"),
        RevocationOutcome::Completed
    );
}

/// Revoke at the after-commit/pre-first-callback seam: the committed callback
/// still runs, but with an already-unavailable handle; cleanup follows.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn test_after_callback_commit_revoke_finishes_with_unavailable_handle() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const ID: u64 = 0xB1_2007;
    let fixture = fixture(ID);
    let publication = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("publication");
    fixture.parent.force_active_for_test();
    let target_id = publication.publication_id();
    let publication = Arc::new(publication);
    let deferred = Arc::new(AtomicUsize::new(0));
    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_saw_available = Arc::new(AtomicBool::new(true));
    {
        let publication = publication.clone();
        let deferred = deferred.clone();
        crate::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let crate::test_hooks::ClassPause::AfterCallbackCommit {
                publication: paused, ..
            } = point
                && paused == target_id
            {
                core::assert_eq!(
                    publication.revoke().expect("revoke accepted"),
                    RevocationOutcome::Deferred
                );
                deferred.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    {
        let callbacks = callbacks.clone();
        let callback_saw_available = callback_saw_available.clone();
        crate::subscribe_char_available(Arc::new(move |device| {
            if device.publication_id() == target_id {
                callbacks.fetch_add(1, Ordering::SeqCst);
                callback_saw_available.store(device.is_available(), Ordering::SeqCst);
            }
        }));
    }

    crate::notify_char_available(fixture.parent.id());
    crate::test_hooks::clear_pause_hook();

    assert_eq!(deferred.load(Ordering::SeqCst), 1);
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    assert!(!callback_saw_available.load(Ordering::SeqCst));
    assert!(crate::find_char_publication(target_id).is_none());
    assert_eq!(
        publication.revoke().expect("already completed"),
        RevocationOutcome::AlreadyRevoked
    );
}

/// Revoke before Available finalization must not re-open availability; the
/// activation executor performs cleanup and the state never returns to Available.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn test_before_available_finalize_revoke_cleans_up() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    const ID: u64 = 0xB1_2008;
    let fixture = fixture(ID);
    let publication = crate::publish_char_for_test(
        fixture.parent.clone(),
        alloc::boxed::Box::new(TestCharRuntime),
    )
    .expect("publication");
    fixture.parent.force_active_for_test();
    let target_id = publication.publication_id();
    let publication = Arc::new(publication);
    let deferred = Arc::new(AtomicUsize::new(0));
    {
        let publication = publication.clone();
        let deferred = deferred.clone();
        crate::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let crate::test_hooks::ClassPause::BeforeAvailableFinalize {
                publication: paused, ..
            } = point
                && paused == target_id
            {
                core::assert_eq!(
                    publication.revoke().expect("revoke accepted"),
                    RevocationOutcome::Deferred
                );
                deferred.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }

    crate::notify_char_available(fixture.parent.id());
    crate::test_hooks::clear_pause_hook();

    assert_eq!(deferred.load(Ordering::SeqCst), 1);
    assert!(crate::find_char_publication(target_id).is_none());
    assert!(!publication.is_available());
    assert_eq!(
        publication.revoke().expect("already completed"),
        RevocationOutcome::AlreadyRevoked
    );
}
