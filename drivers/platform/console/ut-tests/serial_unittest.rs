// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::mem::PhysAddr;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[cfg(feature = "pl011")]
#[def_test]
fn input_irq_binding_initial_pass_duplicate_and_stop() {
    use alloc::{sync::Weak, task::Wake};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{RawWaker, RawWakerVTable},
    };

    struct InputObserver {
        port: Weak<SerialPort>,
        passes: AtomicUsize,
    }
    impl Wake for InputObserver {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            // A consumer may access the port while delivering notification.
            core::assert!(self.port.upgrade().unwrap().io.try_lock().is_some());
            self.passes.fetch_add(1, Ordering::Release);
        }
    }

    /// # Safety
    /// `data` must come from `Arc::into_raw` for a live `InputObserver`.
    /// The caller retains its strong reference; the result owns a new one.
    /// Invoke outside the observed port lock; the callback checks reentry.
    unsafe fn clone_observer(data: *const ()) -> RawWaker {
        // SAFETY: each live RawWaker owns one strong InputObserver reference.
        let observer = unsafe { &*data.cast::<InputObserver>() };
        core::assert!(observer.port.upgrade().unwrap().io.try_lock().is_some());
        // SAFETY: retain a new strong reference for the returned RawWaker.
        unsafe { Arc::increment_strong_count(data.cast::<InputObserver>()) };
        RawWaker::new(data, &OBSERVER_VTABLE)
    }

    /// # Safety
    /// `data` must own one `Arc::into_raw` reference to `InputObserver`.
    /// This call consumes it exactly once. Invoke outside the port lock.
    unsafe fn wake_observer(data: *const ()) {
        // SAFETY: wake consumes the RawWaker's single owned strong reference.
        let observer = unsafe { Arc::from_raw(data.cast::<InputObserver>()) };
        observer.wake();
    }

    /// # Safety
    /// `data` must point to a live `InputObserver` from `Arc::into_raw`;
    /// its owned reference must remain live and is not consumed here.
    /// Invoke outside the observed port lock.
    unsafe fn wake_observer_by_ref(data: *const ()) {
        // SAFETY: borrow the RawWaker's reference without consuming it.
        let observer = core::mem::ManuallyDrop::new(unsafe {
            Arc::from_raw(data.cast::<InputObserver>())
        });
        observer.wake_by_ref();
    }

    /// # Safety
    /// `data` must own one `Arc::into_raw` reference to `InputObserver`.
    /// This call consumes it exactly once. Invoke outside the port lock.
    unsafe fn drop_observer(data: *const ()) {
        // SAFETY: drop consumes the RawWaker's single owned strong reference.
        let observer = unsafe { Arc::from_raw(data.cast::<InputObserver>()) };
        core::assert!(observer.port.upgrade().unwrap().io.try_lock().is_some());
        drop(observer);
    }

    static OBSERVER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        clone_observer,
        wake_observer,
        wake_observer_by_ref,
        drop_observer,
    );

    let mut registers = [0u32; 18];
    registers[6] = 1 << 4;
    // SAFETY: the aligned window covers PL011 registers and outlives the
    // port and waker. Only this test accesses registers; the waker
    // tests the lock but does not access hardware.
    let port = Arc::new(unsafe {
        SerialPort::new_mmio_pl011(
            VirtAddr::from_usize(registers.as_mut_ptr() as usize),
            PhysAddr::from_usize(0),
            size_of_val(&registers),
            SerialRole::Auxiliary,
        )
    });
    let observer = Arc::new(InputObserver {
        port: Arc::downgrade(&port),
        passes: AtomicUsize::new(0),
    });
    // SAFETY: the vtable balances Arc ownership; all observer operations
    // are thread-safe and the test keeps the port alive until Waker drop.
    let waker = unsafe {
        Waker::from_raw(RawWaker::new(
            Arc::into_raw(observer.clone()).cast(),
            &OBSERVER_VTABLE,
        ))
    };
    let passes = &observer.passes;
    assert_eq!(
        port.attach_input_waker(waker.clone()),
        Err(DriverError::BadState)
    );
    assert_eq!(port.handle_input_irq(), IrqEvent::NOT_HANDLED);
    port.prepare_irq_input().unwrap();
    assert_eq!(port.take_input_dropped_bytes(), 0);
    port.io.lock().input.as_mut().unwrap().dropped_bytes = 7;
    assert_eq!(port.take_input_dropped_bytes(), 7);
    assert_eq!(port.take_input_dropped_bytes(), 0);
    registers[0] = 7;
    registers[6] = 0;
    // The emulated register window does not implement FIFO popping.
    // One IRQ therefore collects precisely its maximum batch.
    assert_eq!(port.handle_input_irq(), IrqEvent::HANDLED);
    registers[6] = 1 << 4;
    assert_eq!(passes.load(Ordering::Acquire), 0);
    port.attach_input_waker(waker.clone()).unwrap();
    assert_eq!(passes.load(Ordering::Acquire), 1);
    assert_eq!(
        port.attach_input_waker(waker.clone()),
        Err(DriverError::AlreadyExists)
    );
    assert_eq!(port.handle_input_irq(), IrqEvent::NOT_HANDLED);
    assert_eq!(passes.load(Ordering::Acquire), 1);
    let mut bytes = [0; input::RECEIVE_BUDGET_BYTES];
    assert_eq!(port.read_data(&mut bytes), bytes.len());
    assert!(bytes.iter().all(|byte| *byte == 7));

    // A receive status can require ack even if task-side sampling emptied
    // the FIFO first. That is still this port's event on a shared IRQ.
    registers[16] = 1 << 4;
    assert_eq!(port.handle_input_irq(), IrqEvent::HANDLED);
    assert_eq!(passes.load(Ordering::Acquire), 2);
    let stale = port.detach_input_waker().unwrap();
    assert!(port.detach_input_waker().is_none());
    assert_eq!(port.handle_input_irq(), IrqEvent::HANDLED);
    assert_eq!(passes.load(Ordering::Acquire), 2);
    assert_eq!(registers[17], 0x7ff);
    // Detach stops future lookups, not notification clones already issued.
    stale.wake();
    assert_eq!(passes.load(Ordering::Acquire), 3);
}

#[def_test]
fn stdout_port_registered_at_boot() {
    // The platform `early_driver_init` registers the stdout UART before
    // unit tests run, so the registry must be populated.
    assert!(stdout_port().is_some());
}

#[cfg(feature = "pl011")]
#[def_test]
fn buffered_port_preparation_preserves_shared_input_and_hardware_tail() {
    let mut registers = [0u32; 18];
    registers[6] = 1 << 4;
    let base = VirtAddr::from_usize(registers.as_mut_ptr() as usize);
    // SAFETY: this aligned register window covers PL011 accesses and
    // outlives both local port handles; no other CPU accesses it.
    let boot = Arc::new(unsafe {
        SerialPort::new_mmio_pl011(
            base,
            PhysAddr::from_usize(0),
            size_of_val(&registers),
            SerialRole::Stdout,
        )
    });
    let runtime = boot.clone();
    assert_eq!(boot.collect_input(), 0);
    assert_eq!(boot.getchar(), None);
    boot.prepare_irq_input().unwrap();
    {
        let mut io = boot.io.lock();
        let mut bytes = [1, 2, 3].into_iter();
        io.input.as_mut().unwrap().collect(|| bytes.next());
    }
    runtime.prepare_irq_input().unwrap();
    assert_eq!(runtime.getchar(), Some(1));
    let mut output = [0; 2];
    assert_eq!(boot.read_data(&mut output), 2);
    assert_eq!(output, [2, 3]);

    registers[0] = 7;
    registers[6] = 0;
    assert_eq!(boot.collect_input(), input::RECEIVE_BUDGET_BYTES);
    registers[6] = 1 << 4;
    let mut output = [0; input::RECEIVE_BUDGET_BYTES + 1];
    assert_eq!(runtime.read_data(&mut output), input::RECEIVE_BUDGET_BYTES);
    assert_eq!(runtime.getchar(), None);
    assert!(
        output[..input::RECEIVE_BUDGET_BYTES]
            .iter()
            .all(|b| *b == 7)
    );
    let dropped = boot.io.lock().input.as_ref().unwrap().dropped_bytes;
    assert_eq!(dropped, 0);
    assert_eq!(registers[6], 1 << 4);
}

#[def_test]
fn atomic_console_write_does_not_take_normal_locks() {
    let port = stdout_port().expect("boot registered a stdout");
    let _io_guard = khal::console::IO_LOCK.lock();
    let _port_guard = port.io.lock();
    // Empty output exercises both adapters without depending on UART
    // timing or modifying the permanently registered boot console.
    khal::console::write_data_atomic(b"");
    khal::kprint_atomic!("");
}

#[cfg(feature = "pl011")]
#[def_test]
fn atomic_pl011_write_bypasses_lock_without_reinitializing() {
    let mut registers = [0u32; 18];
    let base = VirtAddr::from_usize(registers.as_mut_ptr() as usize);
    // SAFETY: this aligned, initialized test register window covers all
    // PL011 accesses and outlives the port; no other CPU accesses it.
    let port = unsafe {
        SerialPort::new_mmio_pl011(
            base,
            PhysAddr::from_usize(0),
            size_of_val(&registers),
            SerialRole::Auxiliary,
        )
    };
    // Distinguish reinitialization from transmission, including interrupt
    // and control registers that the normal constructor programmed.
    registers[12..].fill(0x1234_5678);
    let before = registers;
    {
        let _guard = port.io.lock();
        port.write_data_atomic(b"atomic");
    }
    assert_eq!(registers[0], u32::from(b'c'));
    assert_eq!(&registers[1..], &before[1..]);
}

#[cfg(feature = "pl011")]
#[def_test]
fn atomic_pl011_write_stops_when_fifo_stays_full() {
    let mut registers = [0u32; 18];
    registers[0] = 0x1234_5678;
    registers[6] = 1 << 5;
    let base = VirtAddr::from_usize(registers.as_mut_ptr() as usize);
    // SAFETY: this aligned, initialized test register window covers all
    // PL011 accesses and outlives the port; no other CPU accesses it.
    let port = unsafe {
        SerialPort::new_mmio_pl011(
            base,
            PhysAddr::from_usize(0),
            size_of_val(&registers),
            SerialRole::Auxiliary,
        )
    };
    let before = registers;
    port.write_data_atomic(b"not sent");
    assert_eq!(registers, before);
}

#[cfg(feature = "ns16550-mmio")]
#[def_test]
fn atomic_ns16550_write_preserves_layout_and_configuration() {
    for width in [SerialRegWidth::U8, SerialRegWidth::U16, SerialRegWidth::U32] {
        let mut registers = [0xA5A5_A5A5u32; 8];
        let base = VirtAddr::from_usize(registers.as_mut_ptr() as usize);
        // SAFETY: the initialized test window is aligned for all tested
        // widths, uses a four-byte stride, and outlives the local port.
        let port = unsafe {
            SerialPort::new_mmio_ns16550(
                base,
                PhysAddr::from_usize(0),
                size_of_val(&registers),
                SerialRole::Auxiliary,
                2,
                width,
            )
        };
        let before = registers;
        {
            let _guard = port.io.lock();
            port.write_data_atomic(b"atomic");
        }
        let preserved_bits = match width {
            SerialRegWidth::U8 => before[0] & !0xff,
            SerialRegWidth::U16 => before[0] & !0xffff,
            SerialRegWidth::U32 => 0,
        };
        assert_eq!(registers[0], preserved_bits | u32::from(b'c'));
        assert_eq!(&registers[1..], &before[1..]);
    }
}

#[cfg(feature = "ns16550-mmio")]
#[def_test]
fn atomic_ns16550_write_stops_when_transmitter_stays_busy() {
    let mut registers = [0u32; 8];
    registers[0] = 0x1234_5678;
    let base = VirtAddr::from_usize(registers.as_mut_ptr() as usize);
    // SAFETY: this aligned test window covers the 32-bit registers with
    // four-byte stride and outlives the port; no other CPU accesses it.
    let port = unsafe {
        SerialPort::new_mmio_ns16550(
            base,
            PhysAddr::from_usize(0),
            size_of_val(&registers),
            SerialRole::Auxiliary,
            2,
            SerialRegWidth::U32,
        )
    };
    let before = registers;
    port.write_data_atomic(b"not sent");
    assert_eq!(registers, before);
}

/// The adoption guard: `take_early_port` must match only the real stdout's
/// identity and reject any other. This is what prevents the runtime serial
/// driver from adopting the wrong UART (e.g. an auxiliary "safe serial").
#[def_test]
fn take_early_port_discriminates_by_ident() {
    let port = stdout_port().expect("boot registered a stdout");
    let ident = port.ident();
    // The stdout's own identity matches.
    assert!(take_early_port(&ident).is_some());
    // A different identity must not match.
    let wrong = match ident {
        SerialIdent::Mmio { .. } => SerialIdent::IoPort { port: 0xffff },
        SerialIdent::IoPort { .. } => SerialIdent::Mmio {
            paddr: PhysAddr::from_usize(0xdead_beef),
            size: 0x1000,
        },
    };
    assert!(take_early_port(&wrong).is_none());
}

#[def_test]
fn serial_ident_equality() {
    let a = SerialIdent::Mmio {
        paddr: PhysAddr::from_usize(0x9000000),
        size: 0x1000,
    };
    let eq = SerialIdent::Mmio {
        paddr: PhysAddr::from_usize(0x9000000),
        size: 0x1000,
    };
    assert_eq!(a, eq);
    let other_mmio = SerialIdent::Mmio {
        paddr: PhysAddr::from_usize(0x9040000),
        size: 0x1000,
    };
    assert!(a != other_mmio);
    assert!(a != SerialIdent::IoPort { port: 0x3f8 });
}
