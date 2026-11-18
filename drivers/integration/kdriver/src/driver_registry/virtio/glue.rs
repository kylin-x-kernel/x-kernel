// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO device probing and HAL integration.
#[cfg(any(
    feature = "virtio-net",
    feature = "virtio-socket",
    feature = "virtio-9p"
))]
use alloc::boxed::Box;
#[cfg(feature = "virtio-rng")]
use alloc::sync::Arc;
use core::ptr::NonNull;

use cfg_if::cfg_if;
#[cfg(any(
    feature = "virtio-net",
    feature = "virtio-socket",
    feature = "virtio-9p",
    feature = "virtio-rng"
))]
use driver_base::DriverResult;
use driver_os_api::{DmaAllocation, DmaDirection, DmaMapping, DmaOp, DmaSpec, MmioOp, MmioRegion};
use virtio::{BufferDirection, PhysAddr, VirtIoHal};

cfg_if! {
    if #[cfg(feature = "virtio-net")] {
        pub(super) struct VirtIoNet;

        impl VirtIoNet {
            pub(super) fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                irq: Option<driver_os_api::IrqResource>,
            ) -> DriverResult<kclass::prelude::NetDeviceImpl> {
                Ok(Box::new(virtio::VirtIoNetDev::<VirtIoHalImpl, T, 64>::try_new(
                    transport,
                    irq,
                    Some(crate::resource::resource_provider()),
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-socket")] {
        pub(super) struct VirtIoSocket;

        impl VirtIoSocket {
            pub(super) fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<driver_os_api::IrqResource>,
            ) -> DriverResult<kclass::prelude::VsockDeviceImpl> {
                Ok(Box::new(virtio::VirtIoVsockDev::<
                    VirtIoHalImpl,
                    T,
                >::try_new(transport)?))
            }

            /// Constructs a socket through the test counting HAL.
            ///
            /// Everything else, including the production HAL, the RX buffer
            /// size and the provider/bounce-pool paths, is identical to
            /// [`Self::try_new`]. Only the synthetic MMIO fixture dispatches
            /// here, so the caller attributes these counts to that scenario.
            #[cfg(unittest)]
            #[expect(
                clippy::allow_attributes,
                reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
            )]
            #[allow(
                unknown_lints,
                std_box_in_kernel_tests,
                reason = "CG-049-no-std-box-in-kernel-tests: the counting-HAL fixture returns the production boxed VsockDeviceImpl required by kclass publication"
            )]
            pub(super) fn try_new_counting<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<driver_os_api::IrqResource>,
            ) -> DriverResult<kclass::prelude::VsockDeviceImpl> {
                Ok(Box::new(virtio::VirtIoVsockDev::<
                    super::virtio_mmio_support::CountingVirtIoHal,
                    T,
                >::try_new(transport)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-9p")] {
        pub(super) struct VirtIo9p;

        impl VirtIo9p {
            pub(super) fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<driver_os_api::IrqResource>,
            ) -> DriverResult<kclass::prelude::Virtio9pDeviceImpl> {
                Ok(Box::new(virtio::VirtIo9pDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-rng")] {
        use driver_base::DriverError;
        use driver_os_api::{DeviceDma, DmaInfo, ResError};
        use driver_os_api_xkernel::XKernelDeviceDma;

        pub(super) struct VirtIoRng;

        impl VirtIoRng {
            /// Probe with the legacy static HAL. Only the PCI path, whose
            /// firmware DMA description is out of scope, uses this.
            pub(super) fn try_new<T: virtio::Transport + Send + 'static>(
                transport: T,
                irq: Option<driver_os_api::IrqResource>,
            ) -> DriverResult<kclass::prelude::HwrngDeviceImpl> {
                let irq = irq.ok_or(DriverError::InvalidInput)?;
                let device = virtio::VirtIoRngDev::<VirtIoHalImpl, T>::try_new_irq(
                    transport,
                    crate::rng_completion::new_rng_completion(),
                    crate::resource::resource_provider(),
                    irq,
                )?;
                Ok(device)
            }

            /// Probe an MMIO device through its resolved firmware DMA view.
            ///
            /// Every queue ring and request buffer goes through the
            /// device-scoped capability; there is no static-HAL fallback once a
            /// firmware view exists.
            pub(super) fn try_new_with_dma<T: virtio::Transport + Send + 'static>(
                transport: T,
                irq: Option<driver_os_api::IrqResource>,
                info: DmaInfo,
            ) -> DriverResult<kclass::prelude::HwrngDeviceImpl> {
                let irq = irq.ok_or(DriverError::InvalidInput)?;
                // `DeviceDmaBackend` requires a capability usable in both
                // negotiated DMA modes (`access_platform` false and true). This
                // wiring can neither pin the negotiated feature nor program an
                // IOMMU-aware translation, so it only admits an identity window:
                // then the address handed to the device is the same CPU physical
                // address whether the device bypasses or uses platform DMA.
                // This is a restriction of this consumer, not a claim that
                // non-identity windows are invalid; a consumer that controls the
                // negotiated mode or the IOMMU may use them.
                if !info.window().is_identity() {
                    return Err(DriverError::Unsupported);
                }
                let capability: Arc<dyn DeviceDma> = Arc::new(
                    XKernelDeviceDma::new(info).map_err(map_dma_config_error)?,
                );
                // SAFETY: `XKernelDeviceDma` documents allocation, mapping and
                // release as legal in ordinary IRQ context and never NMI. The
                // RNG completion runs in hard IRQ and only releases mappings;
                // queue/request allocation happens in this probe context. The
                // capability is exclusive to this device and stays alive through
                // every retained `DeviceDmaBackend` clone. The identity-window
                // check above satisfies the adapter's requirement that the
                // capability's addresses work in both `access_platform` modes,
                // and the capability contract guarantees zero-filled coherent
                // allocations for queue setup.
                let backend = unsafe { virtio::DeviceDmaBackend::new(capability) };
                let device =
                    virtio::VirtIoRngDev::<VirtIoHalImpl, T, virtio::DeviceDmaBackend>::try_new_irq_with_dma(
                        transport,
                        crate::rng_completion::new_rng_completion(),
                        crate::resource::resource_provider(),
                        irq,
                        backend,
                    )?;
                Ok(device)
            }
        }

        fn map_dma_config_error(error: ResError) -> DriverError {
            match error {
                ResError::InvalidResource => DriverError::InvalidInput,
                ResError::Unsupported => DriverError::Unsupported,
                ResError::NoMemory => DriverError::NoMemory,
                ResError::Busy | ResError::AlreadyExists => DriverError::ResourceBusy,
                ResError::BadState | ResError::NotFound | ResError::MappingFailed
                | ResError::Io | ResError::Interrupted => DriverError::Io,
            }
        }
    }
}

use memaddr::PAGE_SIZE_4K;
pub(super) struct VirtIoHalImpl;

// SAFETY: `VirtIoHal` requires that DMA buffers handed to the device are
// valid for the device's view of memory and stay alive until `dma_dealloc`
// is called, that MMIO physical-to-virtual translations produce valid kernel
// mappings of the MMIO window, and that `share` / `unshare` keep CPU and
// device views of a buffer coherent.
//
// This implementation upholds those invariants by routing every operation
// through the `driver_os_api` provider rather than x-kernel APIs directly:
// * `dma_alloc` / `dma_dealloc` use `alloc_coherent` / `free_coherent`, which
//   return a paired (`cpu_addr`, `bus_addr`) for the same region and free it
//   in one call;
// * `mmio_phys_to_virt` uses `map_mmio`, which installs (or reuses) a kernel
//   mapping covering the requested MMIO range before returning a virtual
//   pointer;
// * `share` / `unshare` use `map_streaming` / `unmap_streaming`, which perform
//   the cache maintenance and IOMMU bookkeeping required to keep CPU and
//   device views in sync for the requested `BufferDirection`.
const fn hal_direction(direction: BufferDirection) -> DmaDirection {
    match direction {
        BufferDirection::DriverToDevice => DmaDirection::DriverToDevice,
        BufferDirection::DeviceToDriver => DmaDirection::DeviceToDriver,
        BufferDirection::Both => DmaDirection::Bidirectional,
    }
}

// SAFETY: every method routes hardware access through the `driver_os_api`
// provider, so the HAL holds no x-kernel state; DMA / MMIO ownership
// invariants are delegated to the installed provider.
unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        let size = pages * PAGE_SIZE_4K;
        let spec = DmaSpec::new(size, PAGE_SIZE_4K);
        match crate::resource::resource_provider().alloc_coherent(spec) {
            Ok(alloc) => {
                let cpu_ptr = alloc.cpu_addr as *mut u8;
                // SAFETY: `alloc.cpu_addr` was just returned by the provider and
                // points to `size` bytes of valid, exclusively-owned DMA memory.
                // Zeroing prevents the device from reading stale kernel data.
                unsafe { core::ptr::write_bytes(cpu_ptr, 0, size) };
                // Ownership of the allocation is handed to the virtio transport
                // and returned via `dma_dealloc`. We deliberately do NOT wrap it
                // in `CoherentMem` (whose Drop would free it on return).
                let paddr = alloc.bus_addr as PhysAddr;
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: The resource-provider \
                              contract guarantees a non-null CPU address on successful DMA \
                              allocation or MMIO mapping"
                )]
                let ptr = NonNull::new(cpu_ptr).expect("dma allocation stored a null CPU address");
                (paddr, ptr)
            }
            Err(err) => {
                log::error!("dma_alloc failed: pages={}, error={:?}", pages, err);
                (0, NonNull::dangling())
            }
        }
    }

    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        _access_platform: bool,
    ) -> i32 {
        let spec = DmaSpec::new(pages * PAGE_SIZE_4K, PAGE_SIZE_4K);
        crate::resource::resource_provider().free_coherent(DmaAllocation {
            cpu_addr: vaddr.as_ptr() as usize,
            bus_addr: paddr,
            spec,
        });
        0
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        let region = MmioRegion {
            base: paddr as usize,
            size,
        };
        match crate::resource::resource_provider().map_mmio(region, "virtio-hal") {
            Ok(m) =>
            {
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: The resource-provider \
                              contract guarantees a non-null CPU address on successful DMA \
                              allocation or MMIO mapping"
                )]
                NonNull::new(m.vaddr as *mut u8)
                    .expect("virtio mmio mapping stored a null virtual address")
            }
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): map_mmio can fail with \
                          NoMemory, InvalidResource or MappingFailed during a fallible PCI probe, \
                          but this VirtIO HAL callback can only return a pointer; pre-map the \
                          MMIO regions before constructing the transport and fail the probe on \
                          mapping errors"
            )]
            Err(err) => {
                log::error!(
                    "virtio mmio_phys_to_virt failed: paddr={:#x}, size={:#x}, err={:?}",
                    paddr,
                    size,
                    err
                );
                panic!("virtio mmio_phys_to_virt failed")
            }
        }
    }

    #[inline]
    unsafe fn share(
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        _access_platform: bool,
    ) -> PhysAddr {
        match crate::resource::resource_provider().map_streaming(buffer, hal_direction(direction)) {
            Ok(m) => m.bus_addr as PhysAddr,
            Err(err) => {
                log::error!("virtio share failed: err={:?}", err);
                0
            }
        }
    }

    #[inline]
    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        _access_platform: bool,
    ) {
        if paddr == 0 {
            return;
        }

        let dir = hal_direction(direction);
        let p = crate::resource::resource_provider();
        // SAFETY: `buffer` is valid (caller contract); `paddr` / `buffer` come
        // from a prior `share` call, so reconstructing the mapping reverses it.
        let slice = unsafe { buffer.as_ref() };
        let cpu_addr = NonNull::from(slice).cast::<u8>();
        p.unmap_streaming(DmaMapping {
            cpu_addr: cpu_addr.as_ptr() as usize,
            bus_addr: paddr,
            len: slice.len(),
            direction: dir,
        });
    }
}

unittest::path_mod_test!(
    virtio_glue_unittest,
    "../../../ut-tests/virtio_glue_unittest.rs"
);
