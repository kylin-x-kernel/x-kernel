// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-context lifecycle tests with real dependency queues and host waiters.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_os_api::{IrqEvent, IrqHandlerToken, IrqTrigger, ResError, ResResult};
use kdevice::{
    BusId, DeviceId, DeviceIdentity, DeviceKind, DeviceLocation, DeviceObject, DiscoveryOrigin,
    DriverId, PlatformIdentity, ResourceSet,
};
use kirq::context::test_support::ScopedHardIrqContext;
use ksync::{Mutex, static_lock};
use unittest::{assert, assert_eq, assert_ne, def_test};
use virtio::mock_virtio::{MockTransport, block_test_disk};

use super::*;

struct TestProvider;
static PROVIDER: TestProvider = TestProvider;
static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);
static_lock! {
    static ACTIONS: Mutex<BTreeMap<(usize, usize), Arc<dyn IrqHandler>>> = Mutex::new(BTreeMap::new());
}
const IRQ: usize = 0x5100;
const FAILED_IRQ: usize = 0x5101;

impl IrqOp for TestProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        if irq.virq().raw() == FAILED_IRQ {
            return Err(ResError::Busy);
        }
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        ACTIONS.lock().insert((irq.virq().raw(), token), handler);
        Ok(IrqHandlerToken::shared_action(token))
    }

    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken) {
        let IrqHandlerToken::SharedAction(token) = token else {
            panic!("shared action expected")
        };
        core::assert!(ACTIONS.lock().remove(&(irq.virq().raw(), token)).is_some());
    }

    fn set_irq_enabled(&self, _: IrqResource, _: bool) {
        panic!("block close must not mask the shared line");
    }
}

fn resource(number: usize) -> IrqResource {
    IrqResource::plain(driver_os_api::Virq::new(number), IrqTrigger::Unknown(0))
}

fn parent_with_id(id: u64) -> Arc<DeviceObject> {
    Arc::new(DeviceObject::new(
        DeviceId::new(id),
        BusId::new(u64::MAX),
        DeviceLocation::PlatformStatic { id: 0 },
        DiscoveryOrigin::PlatformStatic,
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("blk-activation-test"),
            firmware_id: None,
        }),
        None,
        ResourceSet::new(),
    ))
}

fn parent() -> Arc<DeviceObject> {
    parent_with_id(u64::MAX - 100)
}

/// Detached fixture with a live binding, so activation reaches IRQ registration.
fn bound_parent_with_id(id: u64) -> Arc<DeviceObject> {
    let parent = parent_with_id(id);
    parent.install_bound_for_test(
        DriverId::new(u64::MAX - 101),
        "blk-activation-test",
        DeviceKind::Block,
    );
    parent
}

fn bound_parent() -> Arc<DeviceObject> {
    bound_parent_with_id(u64::MAX - 100)
}

fn dispatch() {
    let handlers: Vec<_> = ACTIONS.lock().values().cloned().collect();
    let _irq = ScopedHardIrqContext::enter();
    for handler in handlers {
        handler.handle(IRQ);
    }
}

#[def_test(serial)]
fn failed_irq_and_unbound_parent_close_transport_without_disk() {
    let early_parent = bound_parent();
    assert_eq!(
        kdevice::ProbeContext::with_test_context(early_parent, |ctx| {
            activate(ctx, MockTransport::new(), None).err()
        })
        .unwrap(),
        Some(DriverError::InvalidInput)
    );
    // A live binding lets activation reach IRQ registration; a failed request
    // must drop the activation bundle, which closes the transport without
    // publishing anything.
    {
        let parent = bound_parent();
        let (device, hardware) = block_test_disk(0, false);
        let disk_number = kdevice::DeviceNumber::new(
            virtio::VIRTIO_BLK_MAJOR,
            device.index() << virtio::VIRTIO_BLK_PART_BITS,
        );
        let result = kdevice::ProbeContext::with_test_context(parent, |ctx| {
            activate_device(ctx, device.clone(), resource(FAILED_IRQ), &PROVIDER).err()
        })
        .unwrap();
        assert_eq!(result, Some(DriverError::ResourceBusy));
        assert!(!hardware.lock().is_queue_live());
        assert!(ACTIONS.lock().is_empty());
        assert!(block::lookup_block_device(disk_number).is_none());
        let closed_disk =
            Gendisk::alloc_mq(device.name().into(), 250, 0, 1, device.clone()).unwrap();
        assert_eq!(
            closed_disk.part0().read_block(0, &mut [0; 512]),
            Err(DriverError::Io)
        );
    }

    // An unbound parent has no probe context; activation cannot be entered.
    assert!(kdevice::ProbeContext::with_test_context(parent(), |_| ()).is_none());
}

#[def_test(serial)]
fn close_drains_pending_and_submitted_calls_before_destroying_retained_disk() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let (device, hardware) = block_test_disk(0, false);
    let operations: Arc<dyn BlockCompletionOperations> = device.clone();
    let reclaimer = BlockIoReclaimer::new(Arc::downgrade(&operations));
    let handler: Arc<dyn IrqHandler> = device.clone();
    let irq = block_irq::request_block_irq(
        &PROVIDER,
        resource(IRQ),
        Arc::downgrade(&handler),
        reclaimer.clone(),
    )
    .unwrap();
    let disk = Gendisk::alloc_mq(
        device.name().into(),
        virtio::VIRTIO_BLK_MAJOR,
        device.index() << virtio::VIRTIO_BLK_PART_BITS,
        16,
        device.clone(),
    )
    .unwrap();
    let retained = block::add_disk(disk.clone()).unwrap();
    let number = disk.device_number();
    let activation = BlockActivation {
        device: Arc::clone(&device) as Arc<dyn BlockActivationOps>,
        irq: Some(irq),
        reclaimer,
        disk: Some(disk),
        closed: false,
    };
    let neighbor_calls = Arc::new(AtomicUsize::new(0));
    let calls = neighbor_calls.clone();
    let neighbor = DevIrq::request_with(
        &PROVIDER,
        resource(IRQ),
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            IrqEvent::HANDLED
        }),
    )
    .unwrap();
    let mut tasks = Vec::new();
    for index in 0..24 {
        let disk = retained.clone();
        let task = ktask::TaskInner::new_kthread(
            move || {
                let mut buffer = [0u8; 512];
                disk.read_block(index, &mut buffer).unwrap();
                core::assert!(buffer.iter().all(|byte| *byte == 0xa5));
            },
            "blk-close-io".into(),
            0x8000,
        )
        .unwrap();
        let mut mask = ktask::KCpuMask::new();
        mask.set(index as usize % 2, true);
        task.set_cpumask(mask);
        tasks.push(ktask::spawn_task(task));
    }
    while !tasks
        .iter()
        .all(|task| task.state() == ktask::TaskState::Blocked)
    {
        ktask::yield_now();
    }
    assert_eq!(hardware.lock().submitted_count(), 5);
    let closer = ktask::spawn_task(
        ktask::TaskInner::new_kthread(move || drop(activation), "blk-close".into(), 0x8000)
            .unwrap(),
    );
    while block::lookup_block_device(number).is_some() {
        ktask::yield_now();
    }
    assert!(hardware.lock().is_queue_live());
    assert_eq!(ACTIONS.lock().len(), 2);
    assert_eq!(retained.read_block(0, &mut [0; 512]), Err(DriverError::Io));
    assert_eq!(retained.write_block(0, &[0; 512]), Err(DriverError::Io));
    assert_eq!(retained.flush(), Err(DriverError::Io));
    let mut completed = 0;
    while completed < 24 {
        let batch = hardware.lock().complete_pending();
        completed += batch;
        if batch != 0 {
            dispatch();
            kirq::softirq::run_pending_softirqs();
        }
        ktask::yield_now();
    }
    for task in tasks {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(closer.join(), 0);
    assert!(!hardware.lock().is_queue_live());
    assert_eq!(ACTIONS.lock().len(), 1);
    let before = neighbor_calls.load(Ordering::Relaxed);
    dispatch();
    assert_eq!(neighbor_calls.load(Ordering::Relaxed), before + 1);
    assert_eq!(retained.num_sectors(), 1024);
    assert_eq!(retained.block_size(), 512);
    assert_eq!(retained.flush(), Err(DriverError::Io));
    drop(neighbor);
    assert!(ACTIONS.lock().is_empty());
}

struct ClassCleanup(Vec<DeviceId>);

impl Drop for ClassCleanup {
    fn drop(&mut self) {
        for id in &self.0 {
            kclass::remove_class_device_for_tests(*id);
        }
    }
}

#[def_test(serial)]
fn two_block_activations_close_independently() {
    let (device_a, hardware_a) = block_test_disk(0, false);
    let (device_b, hardware_b) = block_test_disk(1, false);
    let parent_a = bound_parent_with_id(u64::MAX - 120);
    let parent_b = bound_parent_with_id(u64::MAX - 121);
    let id_a = parent_a.id();
    let id_b = parent_b.id();
    let _classes = ClassCleanup(alloc::vec![id_a, id_b]);
    let activation_a = kdevice::ProbeContext::with_test_context(parent_a, |ctx| {
        activate_device(ctx, device_a, resource(IRQ), &PROVIDER)
    })
    .unwrap()
    .unwrap();
    let activation_b = kdevice::ProbeContext::with_test_context(parent_b, |ctx| {
        activate_device(ctx, device_b, resource(IRQ), &PROVIDER)
    })
    .unwrap()
    .unwrap();

    assert_ne!(id_a, id_b);
    assert!(hardware_a.lock().is_queue_live());
    assert!(hardware_b.lock().is_queue_live());
    drop(activation_a);
    assert!(!hardware_a.lock().is_queue_live());
    assert!(
        hardware_b.lock().is_queue_live(),
        "closing one activation must not stop the other"
    );
    drop(activation_b);
    assert!(!hardware_b.lock().is_queue_live());
    assert!(
        ACTIONS.lock().is_empty(),
        "both IRQ actions must be released"
    );
}

#[def_test(serial)]
fn disk_alloc_failure_closes_only_the_new_activation() {
    let (device_a, hardware_a) = block_test_disk(0, false);
    let (device_b, hardware_b) = block_test_disk(1, false);
    let parent = bound_parent_with_id(u64::MAX - 122);
    let id = parent.id();
    let _classes = ClassCleanup(alloc::vec![id]);
    let activation = kdevice::ProbeContext::with_test_context(parent.clone(), |ctx| {
        activate_device(ctx, device_a, resource(IRQ), &PROVIDER)
    })
    .unwrap()
    .unwrap();

    fail_next_disk_alloc();
    let failed = kdevice::ProbeContext::with_test_context(parent, |ctx| {
        activate_device(ctx, device_b, resource(IRQ), &PROVIDER)
    })
    .unwrap();
    assert!(matches!(failed, Err(DriverError::NoMemory)));
    assert!(!hardware_b.lock().is_queue_live());
    assert!(hardware_a.lock().is_queue_live());
    assert_eq!(ACTIONS.lock().len(), 1, "only the first activation remains");

    drop(activation);
    assert!(ACTIONS.lock().is_empty());
}

#[def_test(serial)]
fn disk_alloc_failure_closes_irq_and_reclaimer_without_publish() {
    let parent = bound_parent_with_id(u64::MAX - 123);
    let id = parent.id();
    let _classes = ClassCleanup(alloc::vec![id]);
    let (device, hardware) = block_test_disk(0, false);
    let disk_number = kdevice::DeviceNumber::new(
        virtio::VIRTIO_BLK_MAJOR,
        device.index() << virtio::VIRTIO_BLK_PART_BITS,
    );
    fail_next_disk_alloc();
    let result = kdevice::ProbeContext::with_test_context(parent, |ctx| {
        activate_device(ctx, device, resource(IRQ), &PROVIDER)
    })
    .unwrap();
    assert!(matches!(result, Err(DriverError::NoMemory)));
    assert!(
        !hardware.lock().is_queue_live(),
        "the local activation must close the transport"
    );
    assert!(
        ACTIONS.lock().is_empty(),
        "IRQ registration must be released before returning"
    );
    assert!(
        block::lookup_block_device(disk_number).is_none(),
        "a failed disk allocation must not publish a class entry"
    );
}
