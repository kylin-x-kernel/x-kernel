// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use unittest::{assert, assert_eq, assert_ne, def_test};

use super::{LockOwner, LockOwnerState, holder_key};

/// Keeps the default `identity_token`, which is unknown.
struct UnknownTokenRuntime;

impl crate::UserTaskRuntime for UnknownTokenRuntime {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[def_test]
fn test_holder_key_is_never_zero() {
    let task = crate::TaskInner::new_user(
        || {},
        "unknown-token".into(),
        kidentity::allocate_root_pid_handle().expect("test task identity"),
        Box::new(UnknownTokenRuntime),
    );
    assert_eq!(task.owner_key(), 0);
    assert_ne!(holder_key(&task), 0);
    assert_eq!(
        holder_key(&task),
        &task as *const crate::TaskInner as usize as u64
    );
}

#[def_test]
fn test_lock_owner_records_current_task() {
    let owner = LockOwner::new();
    assert_eq!(owner.state(), LockOwnerState::Unowned);
    assert!(!owner.is_current());
    owner.set_current();
    assert!(owner.is_current());
    owner.clear();
    assert_eq!(owner.state(), LockOwnerState::Unowned);
    assert!(!owner.is_current());
}

/// Restores this CPU's running-task slot even when an assertion returns early.
#[cfg(feature = "smp")]
struct RunningTaskSlot {
    cpu: kcpu_id_map::LogicalCpuId,
    previous: Option<usize>,
}

#[cfg(feature = "smp")]
impl RunningTaskSlot {
    fn set(cpu: kcpu_id_map::LogicalCpuId, next: u64) -> Self {
        use core::sync::atomic::Ordering;

        let previous = kcpu_slot::area(cpu).map(|area| {
            super::CPU_RUNNING_TASK
                .shared_at(area)
                .load(Ordering::Relaxed)
        });
        super::note_cpu_running_task(cpu, next);
        Self { cpu, previous }
    }
}

#[cfg(feature = "smp")]
impl Drop for RunningTaskSlot {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            super::note_cpu_running_task(self.cpu, previous as u64);
        }
    }
}

#[cfg(feature = "smp")]
#[def_test]
fn test_lock_owner_tracks_holder_cpu() {
    use core::sync::atomic::Ordering;

    // The running-task slot is rewritten on every switch; stay on this
    // CPU while it names a fake holder.
    let _pin = kspin::NoPreemptIrqSave::new();
    let cpu = khal::percpu::this_cpu_id();
    let owner = LockOwner::new();
    owner.set_current();
    assert_eq!(owner.state(), LockOwnerState::Running);
    let fake = holder_key(&crate::current()).wrapping_add(1);
    owner.owner_key.store(fake, Ordering::Release);
    assert!(!owner.is_current());
    assert_eq!(owner.state(), LockOwnerState::NotRunning);
    let slot = RunningTaskSlot::set(cpu, fake);
    assert_eq!(owner.state(), LockOwnerState::Running);
    drop(slot);
    assert_eq!(owner.state(), LockOwnerState::NotRunning);
}
