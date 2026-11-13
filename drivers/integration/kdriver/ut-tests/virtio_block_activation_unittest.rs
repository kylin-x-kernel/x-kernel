// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-context lifecycle tests with real dependency queues and host waiters.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_os_api::{IrqEvent, IrqHandlerToken, IrqTrigger, ResError, ResResult};
use kdevice::{
    BusId, DeviceIdentity, DeviceKind, DeviceLocation, DiscoveryOrigin, DriverId,
    PlatformIdentity, ResourceSet,
};
use kirq::context::test_support::ScopedHardIrqContext;
use unittest::{assert, assert_eq, def_test};
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

fn parent() -> Arc<DeviceObject> {
    Arc::new(DeviceObject::new(
        DeviceId::new(u64::MAX - 100),
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

/// Detached fixture with a live binding, so activation reaches IRQ registration.
fn bound_parent() -> Arc<DeviceObject> {
    let parent = parent();
    parent.install_bound_for_test(
        DriverId::new(u64::MAX - 101),
        "blk-activation-test",
        DeviceKind::Block,
    );
    parent
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
    assert_eq!(
        activate(parent(), MockTransport::new(), None),
        Err(DriverError::InvalidInput)
    );
    // A live binding lets activation reach IRQ registration; a failed request
    // must drop the activation bundle, which closes the transport without
    // publishing anything.
    {
        let parent = bound_parent();
        let id = parent.id();
        let (device, hardware) = block_test_disk(0, false);
        let disk_number = kdevice::DeviceNumber::new(
            virtio::VIRTIO_BLK_MAJOR,
            device.index() << virtio::VIRTIO_BLK_PART_BITS,
        );
        assert_eq!(
            activate_device(parent, device.clone(), resource(FAILED_IRQ), &PROVIDER),
            Err(DriverError::ResourceBusy)
        );
        assert!(!hardware.lock().is_queue_live());
        assert!(ACTIONS.lock().is_empty());
        assert!(!ACTIVATIONS.lock().contains_key(&id));
        assert!(block::lookup_block_device(disk_number).is_none());
        let closed_disk =
            Gendisk::alloc_mq(device.name().into(), 250, 0, 1, device.clone()).unwrap();
        assert_eq!(
            closed_disk.part0().read_block(0, &mut [0; 512]),
            Err(DriverError::Io)
        );
        close_device(id);
    }

    // An unbound parent is rejected while capturing the binding, before the
    // IRQ request or any activation state exists. The transport still belongs
    // to the caller, so this path makes no claim about closing it.
    {
        let parent = parent();
        let id = parent.id();
        let (device, _hardware) = block_test_disk(0, false);
        let disk_number = kdevice::DeviceNumber::new(
            virtio::VIRTIO_BLK_MAJOR,
            device.index() << virtio::VIRTIO_BLK_PART_BITS,
        );
        assert_eq!(
            activate_device(parent, device, resource(IRQ), &PROVIDER),
            Err(DriverError::BadState)
        );
        assert!(ACTIONS.lock().is_empty());
        assert!(!ACTIVATIONS.lock().contains_key(&id));
        assert!(block::lookup_block_device(disk_number).is_none());
        close_device(id);
    }
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
        device: device.clone(),
        irq: Some(irq),
        reclaimer,
        disk: Some(disk),
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
