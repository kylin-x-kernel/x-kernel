// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private-storage serial ports without hardware or global console registration.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use super::{Backend, EmergencyTx, PhysAddr, SerialIdent, SerialPort, SerialRole};

const CAPACITY: usize = 256;

pub(super) struct TestBackend {
    pub(super) receive_enable_count: usize,
    input: [u8; CAPACITY],
    input_len: usize,
    input_offset: usize,
    output: [u8; CAPACITY],
    output_len: usize,
}

impl TestBackend {
    pub(super) fn try_receive(&mut self) -> Option<u8> {
        if self.input_offset == self.input_len {
            return None;
        }
        let byte = self.input[self.input_offset];
        self.input_offset += 1;
        Some(byte)
    }

    pub(super) fn send_byte(&mut self, byte: u8) {
        assert!(self.output_len < CAPACITY, "test serial output exhausted");
        self.output[self.output_len] = byte;
        self.output_len += 1;
    }
}

/// An independent port with at most 256 input and 256 output bytes.
///
/// Normal I/O consumes and records bytes verbatim without allocation under the
/// port lock. No IRQ is installed. Emergency output is unsupported and panics.
pub struct SerialPortFixture {
    port: Arc<SerialPort>,
}

impl SerialPortFixture {
    /// Creates a port with the supplied input, without publishing it anywhere.
    ///
    /// Task context; allocates the port. Panics if input exceeds 256 bytes.
    pub fn new(input: &[u8]) -> Self {
        assert!(
            input.len() <= CAPACITY,
            "test serial input exceeds capacity"
        );
        let mut backend = TestBackend {
            receive_enable_count: 0,
            input: [0; CAPACITY],
            input_len: input.len(),
            input_offset: 0,
            output: [0; CAPACITY],
            output_len: 0,
        };
        backend.input[..input.len()].copy_from_slice(input);
        Self {
            port: Arc::new(SerialPort::new(
                Backend::Test(Box::new(backend)),
                EmergencyTx::Test,
                SerialIdent::Mmio {
                    paddr: PhysAddr::from_usize(0),
                    size: 0,
                },
                SerialRole::Auxiliary,
            )),
        }
    }

    /// Clones this fixture's port; the clone retains all byte storage.
    pub fn port(&self) -> Arc<SerialPort> {
        self.port.clone()
    }

    /// Number of RX activation requests, initially zero.
    pub fn receive_enable_count(&self) -> usize {
        let io = self.port.io.lock();
        let Backend::Test(backend) = &io.backend else {
            unreachable!("fixture owns a test serial port");
        };
        backend.receive_enable_count
    }

    /// Returns all bytes written so far without clearing them.
    ///
    /// Task context; snapshot allocation occurs after releasing the port lock.
    pub fn output(&self) -> Vec<u8> {
        let (bytes, len) = {
            let io = self.port.io.lock();
            let Backend::Test(backend) = &io.backend else {
                unreachable!("fixture owns a test serial port");
            };
            (backend.output, backend.output_len)
        };
        bytes[..len].to_vec()
    }
}
