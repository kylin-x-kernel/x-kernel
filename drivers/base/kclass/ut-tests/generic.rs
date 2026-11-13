// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Class-registry retirement protocol tests.
//!
//! These tests exercise the generic registry directly, so they can hold a
//! retirement claim open and observe exactly what publication and a duplicate
//! event see while the reservation exists.

use alloc::{string::String, sync::Arc};

use driver_base::{DeviceKind, DriverError};
use kdevice::{BindingRef, DeviceId, DeviceObject};
use unittest::{assert, assert_eq, def_test};

use super::*;

struct Fixture {
    parent: Arc<DeviceObject>,
    binding: BindingRef,
}

fn fixture(id: u64) -> Fixture {
    let parent = Arc::new(DeviceObject::new_bound_for_test(
        DeviceId::new(id),
        DeviceKind::Char,
    ));
    let binding = parent.binding_ref().expect("bound fixture has a binding");
    Fixture { parent, binding }
}

fn class_device(fixture: &Fixture, runtime: &'static str) -> ClassDevice<&'static str> {
    ClassDevice::try_new_with_binding(
        fixture.parent.clone(),
        fixture.binding.clone(),
        runtime,
        String::from("test-class"),
        DeviceKind::Char,
        None,
        ClassDeviceMetadata::empty(),
    )
    .expect("a bound fixture admits a class device")
}

/// A fixture whose captured binding is already terminal (`Removed`).
fn retired(id: u64) -> (Fixture, ClassDevice<&'static str>) {
    let fixture = fixture(id);
    let device = class_device(&fixture, "retired");
    fixture.parent.force_removed_for_test();
    core::assert!(fixture.binding.is_terminal());
    (fixture, device)
}

#[def_test(serial)]
fn test_class_retirement_placeholder_blocks_publish_until_unpublish() {
    const ID: u64 = 0xB1_1001;
    let (old, entry) = retired(ID);
    let replacement = fixture(ID);
    let mut registry = ClassRegistry::<&'static str>::new();
    registry.publish(entry).expect("terminal A publishes first");

    let claim = registry
        .claim_retirement(DeviceId::new(ID), core::slice::from_ref(&old.binding))
        .expect("the terminal entry can be claimed");
    let ClaimOutcome::Claimed { removed, .. } = &claim else {
        panic!("expected a claimed retirement, got a non-claim outcome");
    };
    assert_eq!(removed.len(), 1, "the claim owns exactly entry A");
    assert!(registry.entries_for_tests(DeviceId::new(ID)).is_empty());

    // The reservation is visible to publication even though A already left
    // the entry list.
    assert_eq!(
        registry.publish(class_device(&replacement, "replacement")),
        Err(DriverError::ResourceBusy)
    );
    assert!(registry.entries_for_tests(DeviceId::new(ID)).is_empty());

    registry.finish_retirement(&claim);
    registry
        .publish(class_device(&replacement, "replacement"))
        .expect("the released reservation admits B");
    assert_eq!(registry.entries_for_tests(DeviceId::new(ID)).len(), 1);
}

#[def_test(serial)]
fn test_class_retirement_single_executor() {
    const ID: u64 = 0xB1_1002;
    let (old, entry) = retired(ID);
    let mut registry = ClassRegistry::<&'static str>::new();
    registry.publish(entry).expect("terminal A publishes first");

    let claim = registry
        .claim_retirement(DeviceId::new(ID), core::slice::from_ref(&old.binding))
        .expect("the terminal entry can be claimed");
    assert!(matches!(&claim, ClaimOutcome::Claimed { .. }));

    // A duplicate Removed/BindingRetired delivery and a publisher both observe
    // the reservation rather than claiming a second time.
    assert!(matches!(
        registry
            .claim_retirement(DeviceId::new(ID), core::slice::from_ref(&old.binding))
            .expect("a retrying claim is not an error"),
        ClaimOutcome::Retiring
    ));
    assert_eq!(
        registry.publish(class_device(&fixture(ID), "replacement")),
        Err(DriverError::ResourceBusy)
    );

    // Finishing an outcome that never owned the reservation is a no-op.
    let other = ClaimOutcome::<&'static str>::Nothing;
    registry.finish_retirement(&other);
    assert_eq!(registry.entries_for_tests(DeviceId::new(ID)).len(), 0);

    registry.finish_retirement(&claim);
    registry
        .publish(class_device(&fixture(ID), "replacement"))
        .expect("single executor released the reservation exactly once");
}

#[def_test(serial)]
fn test_retirement_token_overflow_leaves_registry_untouched() {
    const ID: u64 = 0xB1_1003;
    let (old, entry) = retired(ID);
    let other = fixture(0xB1_1004);
    let mut registry = ClassRegistry::<&'static str>::new();
    registry.publish(entry).expect("terminal A publishes first");
    registry.set_next_retirement_token_for_tests(u64::MAX);

    assert!(matches!(
        registry.claim_retirement(DeviceId::new(ID), core::slice::from_ref(&old.binding)),
        Err(DriverError::BadState)
    ));

    // No entry was removed and no reservation was installed: A is still the
    // only entry for its ID and an unrelated ID can still publish.
    assert_eq!(registry.entries_for_tests(DeviceId::new(ID)).len(), 1);
    assert!(matches!(
        registry.terminal_entries_for_retirement(DeviceId::new(ID)),
        Ok(ref entries) if entries.len() == 1
    ));
    registry
        .publish(class_device(&other, "unrelated"))
        .expect("an overflow must not strand an unrelated publication");
}

#[def_test(serial)]
fn test_live_duplicate_class_publish_rejected() {
    const ID: u64 = 0xB1_1005;
    let live = fixture(ID);
    let mut registry = ClassRegistry::<&'static str>::new();
    registry
        .publish(class_device(&live, "live"))
        .expect("live A publishes");

    assert_eq!(
        registry.publish(class_device(&fixture(ID), "duplicate")),
        Err(DriverError::AlreadyExists)
    );
    assert!(
        matches!(
            registry.terminal_entries_for_retirement(DeviceId::new(ID)),
            Err(DriverError::AlreadyExists)
        ),
        "a live entry blocks predecessor eviction"
    );
    assert_eq!(registry.entries_for_tests(DeviceId::new(ID)).len(), 1);
}

#[def_test(serial)]
fn test_claim_only_matches_the_captured_binding() {
    const ID: u64 = 0xB1_1006;
    let (old, entry) = retired(ID);
    let unrelated = fixture(ID);
    let mut registry = ClassRegistry::<&'static str>::new();
    registry.publish(entry).expect("terminal A publishes");

    assert!(matches!(
        registry
            .claim_retirement(DeviceId::new(ID), core::slice::from_ref(&unrelated.binding))
            .expect("a non-matching claim is not an error"),
        ClaimOutcome::Nothing
    ));
    assert_eq!(
        registry.entries_for_tests(DeviceId::new(ID)).len(),
        1,
        "a stale owner cannot retire another instance's entry"
    );
    assert!(matches!(
        registry
            .claim_retirement(DeviceId::new(ID), core::slice::from_ref(&old.binding))
            .expect("the captured binding can still claim its entry"),
        ClaimOutcome::Claimed { .. }
    ));
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

/// The production path (`remove_class_device_for_tests` -> eviction ->
/// `retire_char_binding` -> `unpublish_class`) must refuse a same-ID
/// replacement while the retirement reservation is live, and admit it only
/// after every removed entry has been unpublished.
///
/// Both observations are taken from inside the pause hooks. The hooks run
/// outside the registry lock, so the reentrant `publish_char` call exercises
/// the real publication path rather than the registry primitives directly.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass::CharDeviceImpl is the production \
              Box<dyn CharDevice + Send + Sync> publication contract"
)]
fn test_production_retirement_orders_unpublish_before_replacement() {
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const ID: u64 = 0xB1_1007;
    let id = DeviceId::new(ID);
    let old = fixture(ID);
    crate::publish_char(old.parent.clone(), Box::new(TestCharRuntime))
        .expect("an active fixture publishes a char class");
    old.parent.force_removed_for_test();
    assert!(old.binding.is_terminal());

    let claimed = Arc::new(AtomicUsize::new(0));
    let unpublished = Arc::new(AtomicUsize::new(0));
    let busy_when_claimed = Arc::new(AtomicBool::new(false));
    let busy_when_unpublished = Arc::new(AtomicBool::new(false));
    let entry_gone_when_unpublished = Arc::new(AtomicBool::new(false));
    {
        let claimed = claimed.clone();
        let unpublished = unpublished.clone();
        let busy_when_claimed = busy_when_claimed.clone();
        let busy_when_unpublished = busy_when_unpublished.clone();
        let entry_gone_when_unpublished = entry_gone_when_unpublished.clone();
        crate::test_hooks::install_pause_hook(Arc::new(move |point| match point {
            crate::test_hooks::ClassPause::RetirementClaimed { id: paused_id }
                if paused_id == id =>
            {
                claimed.fetch_add(1, Ordering::SeqCst);
                let replacement = fixture(ID);
                busy_when_claimed.store(
                    matches!(
                        crate::publish_char(replacement.parent.clone(), Box::new(TestCharRuntime)),
                        Err(DriverError::ResourceBusy)
                    ),
                    Ordering::SeqCst,
                );
            }
            crate::test_hooks::ClassPause::RetirementUnpublished { id: paused_id }
                if paused_id == id =>
            {
                unpublished.fetch_add(1, Ordering::SeqCst);
                let replacement = fixture(ID);
                busy_when_unpublished.store(
                    matches!(
                        crate::publish_char(replacement.parent.clone(), Box::new(TestCharRuntime)),
                        Err(DriverError::ResourceBusy)
                    ),
                    Ordering::SeqCst,
                );
                entry_gone_when_unpublished.store(
                    crate::char_class_registry()
                        .lock()
                        .entries_for_tests(id)
                        .is_empty(),
                    Ordering::SeqCst,
                );
            }
            _ => {}
        }));
    }

    crate::remove_class_device_for_tests(id);
    crate::test_hooks::clear_pause_hook();

    assert_eq!(
        claimed.load(Ordering::SeqCst),
        1,
        "the production path claims exactly one retirement"
    );
    assert_eq!(
        unpublished.load(Ordering::SeqCst),
        1,
        "the unpublish loop runs exactly once"
    );
    assert!(
        busy_when_claimed.load(Ordering::SeqCst),
        "the reservation blocks a same-ID replacement before unpublish"
    );
    assert!(
        busy_when_unpublished.load(Ordering::SeqCst),
        "the reservation still blocks a replacement after unpublish"
    );
    assert!(
        entry_gone_when_unpublished.load(Ordering::SeqCst),
        "the removed entry has left the registry before the reservation is released"
    );
    assert!(
        crate::char_class_registry()
            .lock()
            .entries_for_tests(id)
            .is_empty()
    );

    let replacement = fixture(ID);
    let replacement_class =
        crate::publish_char(replacement.parent.clone(), Box::new(TestCharRuntime))
            .expect("the released reservation admits the replacement");
    assert_eq!(replacement_class.id(), id);
    let entries = crate::char_class_registry().lock().entries_for_tests(id);
    assert_eq!(entries.len(), 1, "only the replacement remains published");
    assert!(
        entries[0].binding_ref().same_binding(&replacement.binding),
        "the surviving entry belongs to the replacement binding"
    );
    // Retire the replacement through the same production path so the cleanup
    // leaves no live entry behind.
    replacement.parent.force_removed_for_test();
    crate::remove_class_device_for_tests(id);
}

/// A class entry must stay unpublished until its device node has finished
/// removal, and a same-ID replacement must stay blocked in that window.
#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass::CharDeviceImpl is the production \
              Box<dyn CharDevice + Send + Sync> publication contract"
)]
fn test_class_unpublish_waits_for_node_removal() {
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const ID: u64 = 0xB1_1008;
    let id = DeviceId::new(ID);
    let old = fixture(ID);
    let class = crate::publish_char(old.parent.clone(), Box::new(TestCharRuntime))
        .expect("A publishes a class entry");
    kdevice::register_device_node_for_binding(
        id,
        class.binding_ref(),
        kdevice::DeviceNodeClass::Drm(kdevice::DrmDeviceNodeMetadata::new(
            String::from("Virtual-1"),
            String::from("connected"),
            String::from("On"),
            String::from("1280x800\n"),
            alloc::vec::Vec::new(),
        )),
        String::from("ordered-node"),
        String::from("ordered-node-driver"),
        kdevice::DeviceLocation::PlatformStatic { id: 8 },
        None,
    )
    .expect("A's device node registers");
    old.parent.force_removed_for_test();
    assert!(old.binding.is_terminal());

    let claimed = Arc::new(AtomicUsize::new(0));
    let unpublished = Arc::new(AtomicUsize::new(0));
    let claimed_at_node = Arc::new(AtomicUsize::new(0));
    let unpublished_at_node = Arc::new(AtomicUsize::new(0));
    let replacement_blocked = Arc::new(AtomicBool::new(false));

    {
        let claimed = claimed.clone();
        let unpublished = unpublished.clone();
        crate::test_hooks::install_pause_hook(Arc::new(move |point| match point {
            crate::test_hooks::ClassPause::RetirementClaimed { id: paused } if paused == id => {
                claimed.fetch_add(1, Ordering::SeqCst);
            }
            crate::test_hooks::ClassPause::RetirementUnpublished { id: paused } if paused == id => {
                unpublished.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }));
    }
    {
        let claimed = claimed.clone();
        let unpublished = unpublished.clone();
        let claimed_at_node = claimed_at_node.clone();
        let unpublished_at_node = unpublished_at_node.clone();
        let replacement_blocked = replacement_blocked.clone();
        kdevice::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let kdevice::test_hooks::BindingPause::NodeRemoving { device_id } = point
                && device_id == id
            {
                claimed_at_node.store(claimed.load(Ordering::SeqCst), Ordering::SeqCst);
                unpublished_at_node.store(unpublished.load(Ordering::SeqCst), Ordering::SeqCst);
                let replacement = fixture(ID);
                let class = ClassDevice::<crate::CharDeviceImpl>::try_new_with_binding(
                    replacement.parent.clone(),
                    replacement.binding.clone(),
                    Box::new(TestCharRuntime),
                    String::from("replacement"),
                    DeviceKind::Char,
                    None,
                    ClassDeviceMetadata::empty(),
                )
                .expect("the replacement class handle builds");
                replacement_blocked.store(
                    matches!(
                        crate::char_class_registry().lock().publish(class),
                        Err(DriverError::AlreadyExists)
                    ),
                    Ordering::SeqCst,
                );
            }
        }));
    }

    crate::remove_class_device_for_tests(id);
    crate::test_hooks::clear_pause_hook();
    kdevice::test_hooks::clear_pause_hook();

    assert_eq!(
        claimed_at_node.load(Ordering::SeqCst),
        0,
        "A must not be claimed before its node has finished removal"
    );
    assert_eq!(
        unpublished_at_node.load(Ordering::SeqCst),
        0,
        "A must not be unpublished before its node has finished removal"
    );
    assert!(
        replacement_blocked.load(Ordering::SeqCst),
        "B's same-ID publication must be blocked while A's node is being removed"
    );
    assert_eq!(
        claimed.load(Ordering::SeqCst),
        1,
        "the production path claims A exactly once"
    );
    assert_eq!(
        unpublished.load(Ordering::SeqCst),
        1,
        "A is unpublished exactly once after node removal"
    );

    let replacement = fixture(ID);
    let replacement_class = crate::publish_char(replacement.parent.clone(), Box::new(TestCharRuntime))
        .expect("B publishes after A's node removal and unpublish");
    assert_eq!(replacement_class.id(), id);
    replacement.parent.force_removed_for_test();
    crate::remove_class_device_for_tests(id);
}
