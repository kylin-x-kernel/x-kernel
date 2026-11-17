// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use driver_base::{Device, DeviceKind};
use kdevice::DeviceObject;
use kclass::CharDevice;
use unittest::def_test;

use super::*;
use kdevice::DeviceId;

struct DummyConsole;

impl Device for DummyConsole {
    fn name(&self) -> &str {
        "console-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn irq(&self) -> Option<usize> {
        None
    }
}

impl CharDevice for DummyConsole {
    fn read(&self, _: &mut [u8]) -> DriverResult<usize> {
        panic!("identity test must not perform device I/O")
    }

    fn write(&self, _: &[u8]) -> DriverResult<usize> {
        panic!("identity test must not perform device I/O")
    }
}

#[def_test]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass::CharDeviceImpl is the production Box<dyn \
              CharDevice + Send + Sync> publication contract"
)]
fn stdout_identity_requires_local_handle_and_clears_on_removal() {
    let parent = Arc::new(DeviceObject::new_bound_for_test(
        DeviceId::new(1),
        DeviceKind::Char,
    ));
    let runtime: CharDeviceImpl = Box::new(DummyConsole);
    let stdout = ClassDevice::try_new_with_info(
        parent,
        runtime,
        "console-test".into(),
        DeviceKind::Char,
        None,
    )
    .unwrap();
    assert!(!stdout.is_available());
    let publication = stdout.publication_id();
    let mut subsystem = ConsoleSubsystem::new();
    subsystem.stdout = Some(publication);
    // The ID alone cannot resolve a handle absent from this local list.
    // Adding it below does not activate the independent Bound device.
    assert!(subsystem.stdout_handle().is_none());
    subsystem.add(stdout);
    assert_eq!(
        subsystem.stdout_handle().unwrap().publication_id(),
        publication
    );
    assert!(!subsystem.stdout_handle().unwrap().is_available());
    let other = kdevice::PublicationId::allocate().unwrap();
    assert!(subsystem.remove(other).is_none());
    assert_eq!(
        subsystem.stdout_handle().unwrap().publication_id(),
        publication
    );
    assert!(subsystem.remove(publication).is_some());
    assert!(subsystem.stdout_handle().is_none());
    assert!(subsystem.stdout.is_none());
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: CharDeviceImpl is the production Box trait object"
)]
fn stdout_console_does_not_fall_back_to_active_binding() {
    let parent = Arc::new(DeviceObject::new_bound_for_test(
        DeviceId::new(2),
        DeviceKind::Char,
    ));
    let runtime: CharDeviceImpl = Box::new(DummyConsole);
    let handle = ClassDevice::try_new_with_info(
        parent.clone(),
        runtime,
        "console-published".into(),
        DeviceKind::Char,
        None,
    )
    .unwrap();
    parent.force_active_for_test();
    assert!(handle.binding_ref().is_active());
    assert!(!handle.is_available());

    assert!(!stdout_handle_is_available(&handle));
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: CharDeviceImpl is the production Box trait object"
)]
fn stdout_console_hides_publication_during_revoke_window() {
    use core::sync::atomic::{AtomicBool, Ordering};

    let parent = Arc::new(DeviceObject::new_bound_for_test(
        DeviceId::new(3),
        DeviceKind::Char,
    ));
    parent.force_active_for_test();
    let publication = kclass::publish_char_for_test(parent, Box::new(DummyConsole))
        .expect("active fixture publishes");
    let publication_id = publication.publication_id();
    let handle = publication.device();
    assert!(handle.is_available());

    assert!(stdout_handle_is_available(&handle));
    let hidden = Arc::new(AtomicBool::new(false));
    let binding_active = Arc::new(AtomicBool::new(false));
    {
        let hidden = hidden.clone();
        let binding_active = binding_active.clone();
        let handle = handle.clone();
        kclass::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let kclass::test_hooks::ClassPause::BeforeUnpublish {
                publication: paused, ..
            } = point
                && paused == publication_id
            {
                hidden.store(!stdout_handle_is_available(&handle), Ordering::SeqCst);
                binding_active.store(handle.binding_ref().is_active(), Ordering::SeqCst);
            }
        }));
    }
    let revoke_result = publication.revoke();
    kclass::test_hooks::clear_pause_hook();
    let hidden = hidden.load(Ordering::SeqCst);
    let binding_active = binding_active.load(Ordering::SeqCst);
    let after = stdout_handle_is_available(&handle);
    assert_eq!(
        revoke_result.expect("revoke completes"),
        kclass::RevocationOutcome::Completed
    );
    assert!(hidden, "stdout must be hidden while revocation is in progress");
    assert!(binding_active, "binding must still be Active in the revoke window");
    assert!(!after, "stdout must stay hidden after revoke");
}
