// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use char_driver::{Device, DeviceKind, DriverError, DriverResult};
use console_driver::serial::test_support::SerialPortFixture;
use unittest::def_test;

use super::*;

struct PollerWakeProbe(AtomicUsize);

impl alloc::task::Wake for PollerWakeProbe {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn poller_owner_failure_does_not_publish_or_notify_and_releases_waker() {
    let connection = ConsoleConnection::new();
    let wake_probe = Arc::new(PollerWakeProbe(AtomicUsize::new(0)));
    let built = core::cell::Cell::new(false);
    let result = connection.poll_input_with(Waker::from(wake_probe.clone()), |waker| {
        ConsoleInputPoller::new_with_callback(
            |_| {
                built.set(true);
                move || waker.wake_by_ref()
            },
            |flags| {
                assert_eq!(flags, AllocFlags::NOWAIT);
                Err(kerrno::KError::NoMemory)
            },
        )
    });
    assert_eq!(result, Err(kerrno::KError::NoMemory));
    assert!(!built.get());
    assert!(connection.input_poller.lock().is_none());
    assert_eq!(wake_probe.0.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&wake_probe), 1);
}

#[def_test]
fn poller_reservation_failure_leaves_once_retryable_without_a_strong_cycle() {
    let once = Once::new();
    let failed = once.try_call_once(|| {
        ConsoleInputPoller::new_with_callback(
            |_| || {},
            |_| Err(kerrno::KError::NoMemory),
        )
    });
    assert!(matches!(failed, Err(kerrno::KError::NoMemory)));
    assert!(once.get().is_none());

    let weak_owner = core::cell::RefCell::new(Weak::new());
    let built = core::cell::Cell::new(0);
    let poller = once
        .try_call_once(|| {
            ConsoleInputPoller::new_with_callback(
                |weak| {
                    assert!(weak.upgrade().is_none());
                    *weak_owner.borrow_mut() = weak.clone();
                    built.set(built.get() + 1);
                    move || { drop(weak.upgrade()); }
                },
                |flags| {
                    assert_eq!(flags, AllocFlags::NOWAIT);
                    KBox::<_>::new_uninit_with_flags(flags)
                },
            )
        })
        .unwrap();
    assert_eq!(built.get(), 1);
    assert!(Arc::ptr_eq(&weak_owner.borrow().upgrade().unwrap(), poller));
    poller.work.shutdown().unwrap();
    drop(once);
    assert!(weak_owner.borrow().upgrade().is_none());
}

struct RuntimeIo {
    calls: Arc<AtomicUsize>,
    fails: bool,
}

impl Device for RuntimeIo {
    fn name(&self) -> &str {
        "tty-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn irq(&self) -> Option<usize> {
        None
    }
}

impl CharDevice for RuntimeIo {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fails {
            return Err(DriverError::BadState);
        }
        buf.fill(b'r');
        Ok(buf.len())
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fails {
            Err(DriverError::BadState)
        } else {
            Ok(buf.len())
        }
    }
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: CharDeviceImpl is still a standard Box in the driver runtime API"
)]
fn connection(
    parent: Arc<kdevice::DeviceObject>,
    fails: bool,
    boot_port: Option<Arc<SerialPort>>,
) -> (Arc<ConsoleConnection>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    // Reuse live identity metadata without publishing or changing global state.
    // Only this test connection can access the synthetic runtime implementation.
    let runtime: CharDeviceImpl = Box::new(RuntimeIo {
        calls: calls.clone(),
        fails,
    });
    let handle =
        ClassDevice::try_new_with_info(parent, runtime, "tty-test".into(), DeviceKind::Char, None)
            .unwrap();
    let connection = ConsoleConnection {
        boot_port,
        runtime: SpinNoPreempt::new(Some(handle)),
        input_poller: SpinNoPreempt::new(None),
    };
    (Arc::new(connection), calls)
}

#[def_test]
fn runtime_connection_is_shared_by_read_write_and_repeated_handoff() {
    // Boot discovery precedes all UTs; this immutable parent stays active for
    // the run. No test publishes, removes or mutates the shared device.
    let Some(stdout) = stdout_console() else {
        unittest::ktest_println!("SKIP: no adopted runtime stdout in this configuration");
        return unittest::TestResult::Ignored;
    };
    let (mut connection, calls) = connection(stdout.parent(), false, serial::stdout_port());
    connection.try_handoff();
    let mut bytes = [0; 3];
    assert_eq!(TtyRead::read(&mut connection, &mut bytes), 3);
    assert_eq!(bytes, [b'r'; 3]);
    assert_eq!(TtyWrite::write(&connection, b"test"), Ok(4));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[def_test]
fn runtime_errors_return_both_directions_to_the_boot_port() {
    // The boot-owned parent is read-only and persistent throughout this run.
    let Some(stdout) = stdout_console() else {
        unittest::ktest_println!("SKIP: no adopted runtime stdout in this configuration");
        return unittest::TestResult::Ignored;
    };
    for write_first in [false, true] {
        let fixture = SerialPortFixture::new(b"boot-input");
        let (mut connection, calls) = connection(stdout.parent(), true, Some(fixture.port()));
        let mut prefix = [0; 4];
        if write_first {
            assert_eq!(TtyWrite::write(&connection, b"first"), Ok(5));
            assert_eq!(TtyRead::read(&mut connection, &mut prefix), prefix.len());
        } else {
            assert_eq!(TtyRead::read(&mut connection, &mut prefix), prefix.len());
            assert_eq!(TtyWrite::write(&connection, b"first"), Ok(5));
        }
        assert_eq!(&prefix, b"boot");
        assert!(connection.runtime_handle().is_none());
        let mut suffix = [0; 16];
        let count = TtyRead::read(&mut connection, &mut suffix);
        assert_eq!(&suffix[..count], b"-input");
        assert_eq!(TtyRead::read(&mut connection, &mut suffix), 0);
        assert_eq!(TtyWrite::write(&connection, b"-second"), Ok(7));
        assert_eq!(fixture.output(), b"first-second");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}

#[def_test]
fn unavailable_runtime_clears_handle_and_uses_boot_port() {
    use kdevice::{DeviceId, DeviceObject};

    // This bound device is never published or activated, so the test
    // exercises availability without changing any global device lifecycle.
    let parent = Arc::new(DeviceObject::new_bound_for_test(
        DeviceId::new(1),
        DeviceKind::Char,
    ));
    let fixture = SerialPortFixture::new(b"boot");
    let (mut connection, calls) = connection(parent, false, Some(fixture.port()));
    assert!(connection.runtime_handle().is_none());
    assert!(connection.runtime.lock().is_none());
    let mut bytes = [0; 4];
    assert_eq!(TtyRead::read(&mut connection, &mut bytes), 4);
    assert_eq!(&bytes, b"boot");
    assert_eq!(TtyWrite::write(&connection, b"out"), Ok(3));
    assert_eq!(fixture.output(), b"out");
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[def_test]
fn console_output_keeps_backend_owned_newline_processing() {
    let fixture = SerialPortFixture::new(&[]);
    let connection = Arc::new(ConsoleConnection {
        boot_port: Some(fixture.port()),
        runtime: SpinNoPreempt::new(None),
        input_poller: SpinNoPreempt::new(None),
    });
    let tty = Tty::new(
        Arc::default(),
        TtyKind::Terminal,
        TtyConfig {
            reader: connection.clone(),
            writer: connection,
            process_mode: ProcessMode::Manual,
        },
    )
    .unwrap();
    assert_eq!(tty.output.lock().write(b"line\n"), Ok(5));
    // The test serial backend is raw; the TTY must pass LF through unchanged.
    assert_eq!(fixture.output(), b"line\n");
}

#[def_test]
fn direct_boot_connection_and_manual_input_remain_usable() {
    let connection = Arc::new(ConsoleConnection::new());
    assert!(connection.runtime_handle().is_none());
    assert_eq!(ConsoleConnection::read(&connection, &mut []), 0);
    assert_eq!(TtyWrite::write(&connection, &[]), Ok(0));
    let tty = Tty::new(
        Arc::default(),
        TtyKind::Terminal,
        TtyConfig {
            reader: connection.clone(),
            writer: connection,
            process_mode: ProcessMode::Manual,
        },
    )
    .unwrap();
    assert!(tty.input.lock().input_waker().is_none());
}
