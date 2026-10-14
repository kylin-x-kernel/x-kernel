// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-instance UART port.
//!
//! Replaces the per-backend global singletons: one [`SerialPort`] per physical
//! UART. The stdout port is constructed during platform `early_driver_init` and
//! reused at runtime, so printk and the driver-model serial driver share the
//! same hardware instance. Auxiliary ports (additional UARTs) are created later
//! by the serial driver and exposed as standalone character devices.
//!
//! Emergency output uses immutable register addresses outside the normal port
//! lock, so an NMI can print even after interrupting a lock holder.

mod emergency;
mod input;

// Isolated byte-oriented ports for cross-crate unit tests.
unittest::path_mod_test!(pub mod test_support, "../ut-tests/serial_test_support.rs");

use alloc::{collections::TryReserveError, sync::Arc};
use core::task::Waker;

#[cfg(feature = "pl011")]
use arm_pl011::Pl011Uart;
use driver_base::DriverError;
use emergency::EmergencyTx;
use input::SerialInput;
use khal::mem::PhysAddr;
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
use khal::mem::VirtAddr;
use kirq::{IrqDesc, IrqEvent};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
#[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
use uart_16550::SerialPort as SerialPort16550;

#[cfg(feature = "ns16550-mmio")]
use crate::ns16550_mmio::{Port as Ns16550MmioPort, SerialRegWidth};

/// Physical identity of a UART.
///
/// Used to match an early-boot stdout instance against a device-model node so
/// the runtime serial driver can adopt the same hardware without remapping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialIdent {
    /// Memory-mapped register window.
    Mmio { paddr: PhysAddr, size: usize },
    /// I/O-port register window (e.g. an x86 ISA COM port).
    IoPort { port: u16 },
}

/// Functional role of a port within the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialRole {
    /// The kernel stdout console (early printk target).
    Stdout,
    /// An auxiliary serial port exposed as a standalone char device.
    Auxiliary,
}

/// Underlying UART hardware, fixed at construction time. More than one variant
/// may be compiled in so a single image can carry several UART families.
enum Backend {
    #[cfg(unittest)]
    Test(alloc::boxed::Box<test_support::TestBackend>),
    #[cfg(feature = "pl011")]
    Pl011(Pl011Uart),
    #[cfg(feature = "ns16550-mmio")]
    Ns16550Mmio(Ns16550MmioPort),
    #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
    Ns16550IoPort(SerialPort16550),
}

impl Backend {
    /// Send one byte, applying the per-backend line discipline. PL011 and the
    /// NS16550 MMIO backend translate `\n` to `\r\n`; the NS16550 ioport backend
    /// preserves its historical raw behavior.
    fn send_byte(&mut self, c: u8) {
        match self {
            #[cfg(unittest)]
            Backend::Test(port) => port.send_byte(c),
            #[cfg(feature = "pl011")]
            Backend::Pl011(uart) => pl011_putchar(uart, c),
            #[cfg(feature = "ns16550-mmio")]
            Backend::Ns16550Mmio(uart) => ns16550_mmio_putchar(uart, c),
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Backend::Ns16550IoPort(uart) => uart.send(c),
        }
    }

    fn try_receive(&mut self) -> Option<u8> {
        match self {
            #[cfg(unittest)]
            Backend::Test(port) => port.try_receive(),
            #[cfg(feature = "pl011")]
            Backend::Pl011(uart) => uart.getchar(),
            #[cfg(feature = "ns16550-mmio")]
            Backend::Ns16550Mmio(uart) => uart.try_receive().ok(),
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Backend::Ns16550IoPort(uart) => uart.try_receive().ok(),
        }
    }

    fn ack_interrupt(&mut self) -> bool {
        match self {
            #[cfg(unittest)]
            Backend::Test(_) => false,
            #[cfg(feature = "pl011")]
            Backend::Pl011(uart) => {
                if uart.is_receive_interrupt() {
                    uart.ack_interrupts();
                    return true;
                }
                false
            }
            // The NS16550 backends have no explicit ack in the existing code.
            #[cfg(feature = "ns16550-mmio")]
            Backend::Ns16550Mmio(_) => false,
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Backend::Ns16550IoPort(_) => false,
        }
    }
}

#[cfg(feature = "pl011")]
fn pl011_putchar(uart: &mut Pl011Uart, c: u8) {
    match c {
        b'\n' => {
            uart.putchar(b'\r');
            uart.putchar(b'\n');
        }
        c => uart.putchar(c),
    }
}

#[cfg(feature = "ns16550-mmio")]
fn ns16550_mmio_putchar(uart: &mut Ns16550MmioPort, c: u8) {
    match c {
        b'\n' => {
            uart.send(b'\r');
            uart.send(b'\n');
        }
        c => uart.send(c),
    }
}

/// Provides serialized normal I/O and best-effort emergency output for one UART.
///
/// The early stdout port is shared through `Arc<SerialPort>` by kernel logging
/// and the runtime console; auxiliary ports back standalone character devices.
/// Use [`Self::read_data`] and [`Self::write_data`] for normal I/O, or
/// [`Self::write_data_atomic`] for emergency output that bypasses the port lock.
pub struct SerialPort {
    io: SpinNoIrq<SerialIo>,
    emergency_tx: EmergencyTx,
    ident: SerialIdent,
    role: SerialRole,
}

struct SerialIo {
    backend: Backend,
    input: Option<SerialInput>,
}

impl SerialPort {
    /// Build a PL011 port over an already-mapped MMIO window.
    ///
    /// `role` records whether this port is the kernel stdout console or an
    /// auxiliary char device; it is later read back through [`Self::role`].
    ///
    /// # Safety
    ///
    /// `uart_base` must name a valid PL011 register window, aligned for 32-bit
    /// MMIO access and mapped for the lifetime of this port. Device setup must
    /// be exclusive until construction completes.
    #[cfg(feature = "pl011")]
    pub unsafe fn new_mmio_pl011(
        uart_base: VirtAddr,
        paddr: PhysAddr,
        size: usize,
        role: SerialRole,
    ) -> Self {
        let mut uart = Pl011Uart::new(uart_base.as_mut_ptr());
        uart.init();
        Self::new(
            Backend::Pl011(uart),
            EmergencyTx::Pl011 {
                base: uart_base.as_usize(),
            },
            SerialIdent::Mmio { paddr, size },
            role,
        )
    }

    /// Build an NS16550 MMIO port over an already-mapped window.
    ///
    /// `reg_shift` is the device-tree `reg-shift` value; each register is
    /// `1 << reg_shift` bytes apart (RK3588's DesignWare UART uses 2). The baud
    /// divisor is left untouched — firmware already programmed it.
    ///
    /// `reg_width` is the device-tree `reg-io-width` value decoded as a typed
    /// MMIO access width. Rockchip DesignWare UARTs use 32-bit accesses.
    ///
    /// # Safety
    ///
    /// `uart_base` must name a valid, exclusively-mapped NS16550 MMIO register
    /// window for the lifetime of this port.
    #[cfg(feature = "ns16550-mmio")]
    pub unsafe fn new_mmio_ns16550(
        uart_base: VirtAddr,
        paddr: PhysAddr,
        size: usize,
        role: SerialRole,
        reg_shift: u32,
        reg_width: SerialRegWidth,
    ) -> Self {
        let stride = 1usize << reg_shift;
        // SAFETY: caller guarantees `uart_base` is an exclusive NS16550 MMIO
        // map. The decoded DT layout supplies the register stride and access
        // width used for all volatile register operations.
        let uart = unsafe { Ns16550MmioPort::new(uart_base.as_usize(), stride, reg_width) };
        // SAFETY: the port was constructed from the same exclusive MMIO window;
        // line/FIFO/interrupt setup is programmed without touching the baud
        // divisor configured by firmware.
        unsafe { uart.init_preserve_baud() };
        Self::new(
            Backend::Ns16550Mmio(uart),
            EmergencyTx::Ns16550Mmio {
                base: uart_base.as_usize(),
                stride,
                reg_width,
            },
            SerialIdent::Mmio { paddr, size },
            role,
        )
    }

    /// Build an NS16550 I/O-port port.
    ///
    /// # Safety
    ///
    /// `port` must name a valid NS16550 I/O-port base.
    #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
    pub unsafe fn new_ioport_ns16550(port: u16, role: SerialRole) -> Self {
        // SAFETY: caller guarantees `port` is a valid NS16550 I/O-port base.
        let mut uart = unsafe { SerialPort16550::new(port) };
        uart.init();
        Self::new(
            Backend::Ns16550IoPort(uart),
            EmergencyTx::Ns16550IoPort { base: port },
            SerialIdent::IoPort { port },
            role,
        )
    }

    fn new(
        backend: Backend,
        emergency_tx: EmergencyTx,
        ident: SerialIdent,
        role: SerialRole,
    ) -> Self {
        Self {
            io: SpinNoIrq::new(SerialIo {
                backend,
                input: None,
            }),
            emergency_tx,
            ident,
            role,
        }
    }

    /// Write bytes while serializing access with normal port operations.
    ///
    /// Must not be called from NMI context; use [`Self::write_data_atomic`].
    pub fn write_data(&self, bytes: &[u8]) {
        let mut io = self.io.lock();
        for &c in bytes {
            io.backend.send_byte(c);
        }
    }

    /// Attempt emergency output without locking, allocation, or device setup.
    ///
    /// May run in NMI context, including while this port's normal lock is held.
    /// Each transmitted byte has a finite polling budget; on timeout the
    /// remainder is discarded. Concurrent writers may interleave or lose bytes.
    /// The device must remain mapped, powered, and configured for transmission.
    pub fn write_data_atomic(&self, bytes: &[u8]) {
        self.emergency_tx.write_data(bytes);
    }

    /// Reads one byte through the same input state as [`Self::read_data`].
    /// Not NMI-safe.
    pub fn getchar(&self) -> Option<u8> {
        let mut byte = [0];
        (self.read_data(&mut byte) != 0).then_some(byte[0])
    }

    /// Reads available input without waiting, preserving buffered byte order.
    ///
    /// Unprepared ports read hardware directly. Prepared ports consume stored
    /// bytes first, then sample at most one hardware receive budget. Not NMI-safe.
    pub fn read_data(&self, bytes: &mut [u8]) -> usize {
        let mut io = self.io.lock();
        let SerialIo { backend, input } = &mut *io;
        if let Some(input) = input {
            return input.read(bytes, || backend.try_receive());
        }
        let mut read_len = 0;
        while read_len < bytes.len() {
            if let Some(c) = backend.try_receive() {
                bytes[read_len] = c;
                read_len += 1;
            } else {
                break;
            }
        }
        read_len
    }

    /// Returns and clears the number of buffered-input bytes lost since the
    /// previous report. The count saturates at `u64::MAX` between reports.
    ///
    /// The input consumer reports this in task context after releasing the port lock;
    /// collection itself never logs from IRQ context. Not NMI-safe.
    pub fn take_input_dropped_bytes(&self) -> u64 {
        self.io
            .lock()
            .input
            .as_mut()
            .map_or(0, |input| core::mem::take(&mut input.dropped_bytes))
    }

    /// Prepares receive storage before enabling buffered IRQ input.
    ///
    /// Startup/task context only; allocation and discarded preparations stay
    /// outside the port lock. Repeated calls preserve existing input.
    /// The console continues using direct input until this is explicitly called.
    pub(crate) fn prepare_irq_input(&self) -> Result<(), TryReserveError> {
        if self.io.lock().input.is_some() {
            return Ok(());
        }
        let mut prepared = Some(SerialInput::try_new()?);
        {
            let mut io = self.io.lock();
            if io.input.is_none() {
                io.input = prepared.take();
            }
        }
        Ok(())
    }

    /// Collects a bounded hardware batch without allocating or waking tasks.
    ///
    /// Returns the number read from hardware, including overflow losses.
    /// Does nothing until receive storage is prepared. Not NMI-safe.
    #[cfg(all(unittest, feature = "pl011"))]
    pub(crate) fn collect_input(&self) -> usize {
        let mut io = self.io.lock();
        let SerialIo { backend, input } = &mut *io;
        input
            .as_mut()
            .map_or(0, |input| input.collect(|| backend.try_receive()))
    }

    /// Collects this port's receive event and notifies its input consumer.
    ///
    /// IRQ-safe, not NMI-safe. Hardware sampling is bounded and serialized with
    /// task reads; notification happens after releasing the port lock.
    pub(crate) fn handle_input_irq(&self) -> IrqEvent {
        let (has_input_event, waker) = {
            let mut io = self.io.lock();
            let SerialIo { backend, input } = &mut *io;
            let Some(input) = input else {
                return IrqEvent::NOT_HANDLED;
            };
            let is_receive_pending = backend.ack_interrupt();
            let received = input.collect(|| backend.try_receive());
            (is_receive_pending || received > 0, input.waker.clone())
        };
        if !has_input_event {
            return IrqEvent::NOT_HANDLED;
        }
        if let Some(waker) = waker {
            waker.wake_by_ref();
        }
        IrqEvent::HANDLED
    }

    /// Connects the sole input consumer and sends an initial notification.
    ///
    /// Task context only, serialized with detach by the owner. Receive storage
    /// must already be prepared. The waker must be allocation-free and
    /// non-sleeping in hardirq and task context, and must not strongly own this
    /// port. Duplicate binding is an
    /// error, not replacement; input collected before binding is preserved.
    /// Successful attachment records the binding, not completion of processing.
    /// Notification means input should be checked, not that a line is ready.
    ///
    /// # Errors
    /// Returns `BadState` without prepared storage, or `AlreadyExists` when
    /// another processor is attached.
    pub fn attach_input_waker(&self, waker: Waker) -> Result<(), DriverError> {
        // IRQ snapshots must not invoke a user-defined Waker clone under io.
        let waker = Arc::new(waker);
        {
            let mut io = self.io.lock();
            let input = io.input.as_mut().ok_or(DriverError::BadState)?;
            if input.waker.is_some() {
                return Err(DriverError::AlreadyExists);
            }
            input.waker = Some(waker.clone());
        }
        // Also check bytes received before the connection and sub-threshold
        // FIFO tails; neither necessarily produces another interrupt.
        waker.wake_by_ref();
        Ok(())
    }

    /// Removes the input notification without waiting or discarding bytes.
    ///
    /// Task context only, serialized with attach by the owner. Earlier IRQ
    /// clones may still notify; the consumer must stop and drain its processing
    /// outside port locks before releasing resources used by that processing.
    /// Destroying the port additionally requires synchronizing its IRQ action.
    pub fn detach_input_waker(&self) -> Option<Waker> {
        let waker = self.io.lock().input.as_mut()?.waker.take()?;
        // Clone and release the potentially last Waker owner outside io.
        Some(waker.as_ref().clone())
    }

    /// Physical identity, for matching against a device-model node.
    pub fn ident(&self) -> SerialIdent {
        self.ident
    }

    /// Functional role of this port.
    pub fn role(&self) -> SerialRole {
        self.role
    }
}

struct EarlyStdout {
    port: Arc<SerialPort>,
    irq: Option<IrqDesc>,
}

static STDOUT: LazyInit<EarlyStdout> = LazyInit::new();

/// Record the early-boot stdout UART (and its interrupt descriptor) so printk
/// and the runtime serial driver share the same instance. Called once from
/// platform `early_driver_init`.
pub fn register_early_stdout(port: Arc<SerialPort>, irq: Option<IrqDesc>) {
    STDOUT.init_once(EarlyStdout { port, irq });
}

/// The early-boot stdout port, once [`register_early_stdout`] has run.
pub fn stdout_port() -> Option<Arc<SerialPort>> {
    STDOUT.get().map(|stdout| stdout.port.clone())
}

/// The interrupt descriptor of the stdout port, if registered.
pub fn stdout_irq() -> Option<IrqDesc> {
    STDOUT.get().and_then(|stdout| stdout.irq)
}

/// Hand the early stdout port to the runtime serial driver if its identity
/// matches `ident`.
///
/// The port stays registered (printk still reaches it through [`stdout_port`]);
/// this returns a shared clone so the serial driver can publish the same
/// hardware as a character device without remapping it.
pub fn take_early_port(ident: &SerialIdent) -> Option<Arc<SerialPort>> {
    let stdout = STDOUT.get()?;
    (stdout.port.ident() == *ident).then(|| stdout.port.clone())
}

unittest::path_mod_test!(serial_unittest, "../ut-tests/serial_unittest.rs");
