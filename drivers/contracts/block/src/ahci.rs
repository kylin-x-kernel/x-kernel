// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use device_res::CpuMemoryOp;
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
#[cfg(not(unittest))]
use simple_ahci::AhciDriver as CoreAhciDriver;
#[cfg(not(unittest))]
pub use simple_ahci::Hal as AhciControllerHal;

use crate::BlockDeviceOperations;
#[cfg(unittest)]
use crate::ahci_controller_fixture::AhciDriver as CoreAhciDriver;
#[cfg(unittest)]
pub use crate::ahci_controller_fixture::Hal as AhciControllerHal;

/// Platform admission for the legacy AHCI SDK's direct physical-address DMA.
///
/// The SDK has no data-buffer mapping or range-based cache-synchronization hook.
/// Its `flush_dcache` callback cannot be assumed to maintain data buffers.
///
/// # Safety
/// A true [`Self::DMA_COHERENT`] must mean that ordinary cached CPU buffers and
/// the SDK's command allocations are hardware coherent with this controller.
/// A HAL that cannot establish this must report false; initialization is then
/// rejected before the SDK can access MMIO or submit any DMA.
pub unsafe trait AhciHal: AhciControllerHal {
    /// Whether cached data and command allocations support coherent device DMA.
    const DMA_COHERENT: bool;
}

/// AHCI driver implementation based on `simple_ahci` crate.
///
/// The `SpinNoIrq` lock serializes hardware access so the device can be
/// shared behind a `&self` interface. `num_blocks` and `block_size` cache
/// the device geometry so queries do not need to take the lock.
/// Arbitrary CPU buffer addresses use the block layer's word-aligned adapter;
/// aligned buffers go through unchanged. Allocation precedes the device lock,
/// while the controller/HAL supplies the existing physical-address translation.
/// Only explicitly hardware-coherent HALs are admitted. Ordinary CPU staging
/// supplies alignment, not cache maintenance or DMA mappings.
pub struct AhciDriver<H: AhciHal> {
    inner: SpinNoIrq<CoreAhciDriver<H>>,
    num_blocks: u64,
    block_size: usize,
    buffer_provider: &'static dyn CpuMemoryOp,
}

impl<H: AhciHal> AhciDriver<H> {
    /// Attempts to create a new AHCI driver using the specified MMIO base address.
    ///
    /// Returns `None` without entering the SDK when the HAL cannot certify
    /// coherent cached DMA, or when SDK initialization fails. Non-coherent
    /// platforms need a separate SDK mapping/cache protocol before admission.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `base_addr` refers to a valid MMIO register block of the AHCI controller.
    /// - The memory region from `base_addr` onward is mapped and accessible.
    /// - No other part of the code is accessing the AHCI controller simultaneously.
    /// - The AHCI hardware is functioning at the provided address.
    pub unsafe fn new(base_addr: usize, buffer_provider: &'static dyn CpuMemoryOp) -> Option<Self> {
        if !H::DMA_COHERENT {
            return None;
        }
        // SAFETY: the caller upholds the `# Safety` contract of this function:
        // `base_addr` is the mapped MMIO register block of a present, working
        // AHCI controller that no other code accesses concurrently, which is
        // exactly the contract `simple_ahci::AhciDriver::try_new` requires.
        unsafe { CoreAhciDriver::<H>::try_new(base_addr) }.map(|driver| {
            let block_size = driver.block_size();
            let num_blocks = driver.capacity();
            AhciDriver {
                inner: SpinNoIrq::new(driver),
                num_blocks,
                block_size,
                buffer_provider,
            }
        })
    }
}

impl<H: AhciHal> Device for AhciDriver<H> {
    fn name(&self) -> &str {
        "ahci"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl<H: AhciHal> BlockDeviceOperations for AhciDriver<H> {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn num_blocks(&self) -> u64 {
        self.num_blocks
    }
}

impl<H: AhciHal> AhciDriver<H> {
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        crate::word_io::read_blocks_with_provider(
            self.buffer_provider,
            block_id,
            self.block_size,
            self.num_blocks,
            buf,
            |block, aligned| {
                let mut dev = self.inner.lock();
                if dev.read(block, aligned) {
                    Ok(())
                } else {
                    Err(DriverError::Io)
                }
            },
        )
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        crate::word_io::write_blocks_with_provider(
            self.buffer_provider,
            block_id,
            self.block_size,
            self.num_blocks,
            buf,
            |block, aligned| {
                let mut dev = self.inner.lock();
                if dev.write(block, aligned) {
                    Ok(())
                } else {
                    Err(DriverError::Io)
                }
            },
        )
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

impl<H: AhciHal> crate::BlkMqOperations for AhciDriver<H> {
    fn queue_rq(&self, mut request: crate::Request) {
        let block_id = request.sector() / (self.block_size() as u64 / 512);
        let result = match request.operation() {
            crate::BioOperation::Read => {
                request.read_contiguous(|bytes| self.read_block(block_id, bytes))
            }
            crate::BioOperation::Write => {
                request.write_contiguous(|bytes| self.write_block(block_id, bytes))
            }
            crate::BioOperation::Flush => self.flush(),
        };
        request.complete(result);
    }
}

unittest::path_mod_test!(pub mod tests_ahci, "../ut-tests/ahci.rs");
unittest::path_mod_test!(mod cached_buffer_tests, "../ut-tests/ahci_cached_buffers.rs");
