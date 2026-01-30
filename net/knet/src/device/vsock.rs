//! Vsock device integration helpers.
use alloc::collections::VecDeque;
use core::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use kdriver::prelude::*;
use kerrno::{KError, KResult, k_bail};
use ksync::Mutex;
use ktask::future::{block_on, interruptible};

use crate::{alloc::string::ToString, vsock::connection_manager::VSOCK_CONN_MANAGER};

// A single global vsock device instance.
static VSOCK_DEV: Mutex<Option<VsockDevice>> = Mutex::new(None);
static VSOCK_EVENT_QUEUE: Mutex<VecDeque<VsockDriverEventType>> = Mutex::new(VecDeque::new());

const VSOCK_RX_SCRATCH_SIZE: usize = 0x1000; // 4KiB scratch buffer for vsock receive

/// Registers a vsock device. Only one vsock device can be registered.
pub fn register_vsock_dev(dev: VsockDevice) -> KResult {
    let mut guard = VSOCK_DEV.lock();
    if guard.is_some() {
        k_bail!(AlreadyExists, "vsock device already registered");
    }
    *guard = Some(dev);
    drop(guard);
    Ok(())
}

static POLL_USERS: Mutex<usize> = Mutex::new(0);
static POLL_ACTIVE: AtomicBool = AtomicBool::new(false);
static POLL_BACKOFF: PollBackoff = PollBackoff::new();

struct PollBackoff {
    consecutive_idle: AtomicU64,
}

impl PollBackoff {
    const fn new() -> Self {
        Self {
            consecutive_idle: AtomicU64::new(0),
        }
    }

    fn next_interval(&self) -> Duration {
        let idle = self.consecutive_idle.load(Ordering::Relaxed);
        let interval_us = match idle {
            0..=3 => 100,     //  3 ：100μs
            4..=10 => 500,    // 4-10 ：500μs
            11..=20 => 2_000, // 11-20 ：2ms
            _ => 10_000,      // 20+ ：10ms
        };
        Duration::from_micros(interval_us)
    }

    fn on_activity(&self) {
        self.consecutive_idle.store(0, Ordering::Release);
    }

    fn on_idle_tick(&self) {
        self.consecutive_idle.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64) {
        let idle = self.consecutive_idle.load(Ordering::Relaxed);
        let interval = self.next_interval().as_micros() as u64;
        (idle, interval)
    }
}

/// Start the background vsock polling task if needed.
pub fn start_vsock_polling() {
    let mut count = POLL_USERS.lock();
    *count += 1;
    let new_count = *count;
    debug!("start_vsock_polling: ref_count -> {}", new_count);
    if new_count == 1 {
        if !POLL_ACTIVE.swap(true, Ordering::SeqCst) {
            drop(count);
            debug!("Starting vsock poll task");
            ktask::spawn_with_name(vsock_poll_task, "vsock-poll".to_string());
        } else {
            warn!("Poll task already running!");
        }
    }
}

pub fn stop_vsock_polling() {
    let mut count = POLL_USERS.lock();
    if *count == 0 {
        // this should not happen, log a warning
        warn!("stop_vsock_polling called but ref_count already 0");
        return;
    }
    *count -= 1;
    let new_count = *count;
    debug!("stop_vsock_polling: ref_count -> {new_count}");
}

fn vsock_poll_task() {
    loop {
        let ref_count = *POLL_USERS.lock();
        if ref_count == 0 {
            POLL_ACTIVE.store(false, Ordering::SeqCst);
            debug!("Vsock poll task exiting (no active connections)");
            break;
        }
        let _ = block_on(interruptible(poll_vsock_adaptive()));
    }
}

async fn poll_vsock_adaptive() -> KResult<()> {
    let has_events = poll_vsock_devices()?;

    if has_events {
        POLL_BACKOFF.on_activity();
    } else {
        POLL_BACKOFF.on_idle_tick();
    }

    let interval = POLL_BACKOFF.next_interval();

    let (idle_count, interval_us) = POLL_BACKOFF.snapshot();
    if idle_count > 0 && idle_count % 10 == 0 {
        trace!("Poll frequency: idle_count={idle_count}, interval={interval_us}μs",);
    }
    ktask::future::sleep(interval).await;
    Ok(())
}

fn poll_vsock_devices() -> KResult<bool> {
    let mut guard = VSOCK_DEV.lock();
    let dev = guard.as_mut().ok_or(KError::NotFound)?;
    let mut event_count = 0;
    let mut buf = alloc::vec![0; VSOCK_RX_SCRATCH_SIZE];

    // Process pending events first
    // Use core::mem::take to atomically move all events out and empty the global queue
    let pending_events = core::mem::take(&mut *VSOCK_EVENT_QUEUE.lock());
    for event in pending_events {
        handle_vsock_event(event, dev, &mut buf);
    }

    loop {
        match dev.poll_event() {
            Ok(None) => break, // no more events
            Ok(Some(event)) => {
                event_count += 1;
                handle_vsock_event(event, dev, &mut buf);
            }
            Err(e) => {
                info!("Failed to poll vsock event: {e:?}");
                break;
            }
        }
    }
    Ok(event_count > 0)
}

fn handle_vsock_event(event: VsockDriverEventType, dev: &mut VsockDevice, buf: &mut [u8]) {
    let mut manager = VSOCK_CONN_MANAGER.lock();
    debug!("Handling vsock event: {event:?}");

    match event {
        VsockDriverEventType::ConnectionRequest(conn_id) => {
            if let Err(e) = manager.on_connection_request(conn_id) {
                info!("Connection request failed: {conn_id:?}, error={e:?}");
            }
        }

        VsockDriverEventType::Received(conn_id, len) => {
            let free_space = if let Some(conn) = manager.get_connection(conn_id) {
                conn.lock().rx_buffer_free()
            } else {
                info!("Received data for unknown connection: {conn_id:?}");
                return;
            };

            if free_space == 0 {
                VSOCK_EVENT_QUEUE
                    .lock()
                    .push_back(VsockDriverEventType::Received(conn_id, len));
                return;
            }

            let max_read = core::cmp::min(free_space, buf.len());
            match dev.recv(conn_id, &mut buf[..max_read]) {
                Ok(read_len) => {
                    if let Err(e) = manager.on_data_received(conn_id, &buf[..read_len]) {
                        info!(
                            "Failed to dispatch_irq received data: conn_id={conn_id:?}, \
                             error={e:?}",
                        );
                    }
                }
                Err(e) => {
                    info!("Failed to receive vsock data: conn_id={conn_id:?}, error={e:?}",);
                }
            }
        }

        VsockDriverEventType::Disconnected(conn_id) => {
            if let Err(e) = manager.on_disconnected(conn_id) {
                info!("Failed to dispatch_irq disconnection: {conn_id:?}, error={e:?}",);
            }
        }

        VsockDriverEventType::Connected(conn_id) => {
            if let Err(e) = manager.on_connected(conn_id) {
                info!("Failed to dispatch_irq connection established: {conn_id:?}, error={e:?}",);
            }
        }

        VsockDriverEventType::Unknown => warn!("Received unknown vsock event"),
    }
}

pub fn vsock_listen(addr: VsockAddr) -> KResult<()> {
    let mut guard = VSOCK_DEV.lock();
    let dev = guard.as_mut().ok_or(KError::NotFound)?;
    dev.listen(addr.port);
    Ok(())
}

fn map_dev_err(e: DriverError) -> KError {
    match e {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::Io => KError::Io,
        _ => KError::BadState,
    }
}

pub fn vsock_connect(conn_id: VsockConnId) -> KResult<()> {
    let mut guard = VSOCK_DEV.lock();
    let dev = guard.as_mut().ok_or(KError::NotFound)?;
    dev.connect(conn_id).map_err(map_dev_err)
}

pub fn vsock_send(conn_id: VsockConnId, buf: &[u8]) -> KResult<usize> {
    let mut guard = VSOCK_DEV.lock();
    let dev = guard.as_mut().ok_or(KError::NotFound)?;
    dev.send(conn_id, buf).map_err(map_dev_err)
}

pub fn vsock_disconnect(conn_id: VsockConnId) -> KResult<()> {
    let mut guard = VSOCK_DEV.lock();
    let dev = guard.as_mut().ok_or(KError::NotFound)?;
    dev.disconnect(conn_id).map_err(map_dev_err)
}

pub fn vsock_guest_cid() -> KResult<u64> {
    let mut guard = VSOCK_DEV.lock();
    let dev = guard.as_mut().ok_or(KError::NotFound)?;
    Ok(dev.guest_cid())
}
