//! Exercise both queue IDs at the two edges of the submit-to-sleep window.

use std::{
    cell::Cell,
    sync::{Arc, Mutex},
};

use super::*;
use crate::{
    hal::fake::FakeHal,
    transport::{
        DeviceStatus, DeviceType,
        fake::{FakeTransport, State},
    },
};

struct Channel {
    transport: FakeTransport<[u32; 8]>,
    early: bool,
    waits: Cell<u32>,
    corrupt_id: Option<u32>,
}

impl Channel {
    fn complete(&self) {
        let mut state = self.transport.state.lock().unwrap();
        for index in [QUEUE_TRANSMIT, QUEUE_CURSOR] {
            if !state.queues[index as usize]
                .notified
                .swap(false, core::sync::atomic::Ordering::Relaxed)
            {
                continue;
            }
            assert!(
                state.read_write_queue::<{ QUEUE_SIZE as usize }>(index, |input| {
                    assert_eq!(input, [index as u8, 42]);
                    if index == QUEUE_CURSOR {
                        vec![]
                    } else {
                        vec![77]
                    }
                })
            );
            if let Some(id) = self.corrupt_id {
                let queue = &state.queues[usize::from(index)];
                crate::queue::fake_corrupt_last_used_id::<{ QUEUE_SIZE as usize }>(
                    queue.device_area as *mut u8,
                    id,
                );
            }
        }
    }
}

impl GpuChannel for Channel {
    type Transport = FakeTransport<[u32; 8]>;

    fn with_transport<R>(&mut self, access: impl FnOnce(&mut Self::Transport) -> R) -> R {
        let result = access(&mut self.transport);
        if self.early {
            self.complete();
        }
        result
    }

    fn wait_until(&self, mut ready: impl FnMut() -> bool) {
        assert!(
            !self.early,
            "a completion before park must not need another IRQ"
        );
        assert!(!ready());
        self.waits.set(self.waits.get() + 1);
        self.complete();
        assert!(ready());
    }
}

fn test_gpu(early: bool, corrupt_id: Option<u32>) -> VirtIOGpu<FakeHal, Channel> {
    let state = Arc::new(Mutex::new(State::new(
        vec![Default::default(), Default::default()],
        [0; 8],
    )));
    let transport = FakeTransport {
        device_type: DeviceType::GPU,
        max_queue_size: QUEUE_SIZE.into(),
        device_features: 0,
        state,
    };
    VirtIOGpu::<FakeHal, Channel>::prepare(transport, |transport| Channel {
        transport,
        early,
        waits: Cell::new(0),
        corrupt_id,
    })
    .unwrap()
}

#[test]
fn both_queues_handle_early_and_delayed_completion() {
    for early in [true, false] {
        let mut gpu = test_gpu(early, None);
        let state = gpu.channel().transport.state.clone();
        assert!(
            !state
                .lock()
                .unwrap()
                .status_history
                .last()
                .unwrap()
                .contains(DeviceStatus::DRIVER_OK)
        );
        gpu.enable();
        let mut response = [0];
        gpu.exchange(QUEUE_TRANSMIT, &[&[0, 42]], &mut [&mut response])
            .unwrap();
        assert_eq!(response, [77]);
        gpu.exchange(QUEUE_CURSOR, &[&[1, 42]], &mut []).unwrap();
        assert_eq!(gpu.channel().waits.get(), if early { 0 } else { 2 });
    }
}

#[test]
fn wrong_completion_resets_and_retires_both_queues() {
    for (index, corrupt_id) in [(QUEUE_TRANSMIT, 1), (QUEUE_CURSOR, 1 << 16)] {
        let mut gpu = test_gpu(false, Some(corrupt_id));
        gpu.enable();
        let input = [index as u8, 42];
        let error = if index == QUEUE_CURSOR {
            gpu.exchange(index, &[&input], &mut [])
        } else {
            gpu.exchange(index, &[&input], &mut [&mut [0]])
        };
        assert_eq!(error, Err(Error::WrongToken));
        assert_eq!(gpu.control_queue.outstanding_heads(), 0);
        assert_eq!(gpu.cursor_queue.outstanding_heads(), 0);
        let state = gpu.channel().transport.state.clone();
        assert_eq!(
            state.lock().unwrap().status_history.last(),
            Some(&DeviceStatus::empty())
        );
        for index in [QUEUE_TRANSMIT, QUEUE_CURSOR] {
            assert_eq!(gpu.exchange(index, &[&input], &mut []), Err(Error::IoError));
        }
        let history = state.lock().unwrap().status_history.clone();
        gpu.enable();
        drop(gpu);
        assert_eq!(state.lock().unwrap().status_history, history);
    }
}

#[test]
fn failed_cursor_setup_resets_the_installed_control_ring() {
    let state = Arc::new(Mutex::new(State::new(
        vec![Default::default(), Default::default()],
        [0u32; 8],
    )));
    // Only the used flag is queried; this is never dereferenced as a ring.
    state.lock().unwrap().queues[QUEUE_CURSOR as usize].descriptors = 1;
    let transport = FakeTransport {
        device_type: DeviceType::GPU,
        max_queue_size: QUEUE_SIZE.into(),
        device_features: 0,
        state: state.clone(),
    };
    assert!(matches!(
        VirtIOGpu::<FakeHal, _>::new(transport),
        Err(Error::AlreadyUsed)
    ));
    assert_eq!(
        state.lock().unwrap().status_history.last(),
        Some(&DeviceStatus::empty())
    );
}
