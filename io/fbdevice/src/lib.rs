// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framebuffer device emulation over scanout.
//!
//! This mirrors the Linux `drm_fbdev` model: `/dev/fb0` is not a separate
//! hardware path but a compatibility shim built on top of the display
//! device's scanout interface. At [`fb_init`] the primary display device is
//! asked for its resolution, a contiguous shadow buffer is allocated in guest
//! memory, and that buffer is bound to the host as a 2D scanout resource via
//! [`DisplayDevice::create_scanout_resource`]. Userspace reads and writes land
//! in the shadow; the shadow can be pushed to the visible scanout on demand
//! via [`fb_present`].
//!
//! Unlike Linux's fbcon-backed fbdev, this emulation deliberately does **not**
//! run a background refresh task. A continuous `present_scanout_resource`
//! (which can call `SET_SCANOUT` on virtio-gpu when the resource or scanout
//! rectangle changes) would race an active DRM
//! compositor for the single physical scanout and cause flicker; fbdev
//! emulation therefore defers to a DRM master and only presents the shadow when
//! explicitly asked.
//!
//! This works uniformly for any [`DisplayDevice`]: scanout-only devices (e.g.
//! virtio-gpu) and any future directly-mapped device both get a working
//! `/dev/fb0` without the driver having to expose a directly-mapped buffer.

#![no_std]

#[macro_use]
extern crate log;

extern crate alloc;

mod fb;

use alloc::sync::Arc;

use display::{
    DisplayDevice, DisplayInfo, ScanoutBacking, ScanoutFormat, ScanoutRect, ScanoutResource,
    ScanoutWindow,
};
pub use fb::{FB_PAN_STEPS, FrameBuffer};
use kalloc::{PageRef, UsageKind, alloc_zeroed_pages_exact_ref};
use kclass::{ClassDevice, DisplayDeviceImpl, display_devices as class_display_devices};
use khal::mem::p2v;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

/// Bytes per pixel for the shadow scanout buffer. Matches the BGRA8888 format
/// advertised to the display device.
const BYTES_PER_PIXEL: usize = 4;

/// Driver-local resource id for the framebuffer's scanout resource.
const FB_RESOURCE_ID: u32 = 1;

struct ShadowBacking<'a> {
    _pages: &'a PageRef,
    range: memaddr::PhysAddrRange,
}

// SAFETY: the shadow allocation is owned by the framebuffer emulation and is
// retained until the display resource is destroyed.
unsafe impl ScanoutBacking for ShadowBacking<'_> {
    fn range(&self) -> memaddr::PhysAddrRange {
        self.range
    }
}

/// Runtime state for the emulated framebuffer, established once during
/// [`fb_init`] against the primary display device.
struct FbEmulation {
    info: DisplayInfo,
    /// Primary display device the shadow is bound to, cached so a present pays
    /// one Arc clone instead of re-snapshotting (and allocating) the whole
    /// class registry on every [`fb_present`] call. The device outlives fbdev
    /// (no teardown), so the cached handle cannot dangle.
    device: ClassDevice<DisplayDeviceImpl>,
    /// Guest-mapped shadow buffer; userspace reads/writes land here. Kept alive
    /// for the framebuffer lifetime, so the host scanout resource's backing
    /// memory stays valid.
    shadow: PageRef,
    /// Requested page-aligned buffer length, excluding allocation rounding slack.
    shadow_len_bytes: usize,
}

static FB: LazyInit<SpinNoIrq<Option<Arc<FbEmulation>>>> = LazyInit::new();

/// Initialize the framebuffer emulation subsystem against the primary display
/// device.
///
/// Allocates a shadow buffer sized for the primary display's resolution at
/// BGRA8888 and binds it as a host-visible scanout resource. On any failure the
/// emulation is left unavailable (rather than panicking), so a misbehaving
/// display driver cannot take down boot; `/dev/fb0` simply will not appear.
pub fn fb_init() {
    info!("Initialize framebuffer subsystem...");

    FB.init_once(SpinNoIrq::new(None));

    // Display registration races this call in both directions: a PCI display
    // that probed earlier is in the class registry now, a later one arrives
    // through the availability callback, exactly like the input devices.
    kclass::subscribe_display_available(Arc::new(on_display_available));
    if class_display_devices().is_empty() {
        info!("fbdev: no display registered yet; waiting for one");
    }
    if let Err(e) = try_setup() {
        debug!("fbdev: initial setup did not run: {:?}", e);
    }
}

fn on_display_available(device: ClassDevice<DisplayDeviceImpl>) {
    if FB.lock().is_some() {
        return;
    }
    if let Err(e) = try_setup_with(device) {
        warn!("fbdev emulation unavailable: {:?}", e);
    }
}

#[derive(Debug)]
enum SetupError {
    NoDisplay,
    InvalidResolution,
    SizeOverflow,
    AllocationFailed,
    ResourceSetupFailed,
}

fn try_setup() -> Result<(), SetupError> {
    // Pick the primary display device, when one already probed.
    let device = class_display_devices()
        .into_iter()
        .next()
        .ok_or(SetupError::NoDisplay)?;
    try_setup_with(device)
}

fn try_setup_with(device: ClassDevice<DisplayDeviceImpl>) -> Result<(), SetupError> {
    let info = device.info();
    if info.width == 0 || info.height == 0 {
        warn!("primary display reported zero resolution; skipping fbdev");
        return Err(SetupError::InvalidResolution);
    }

    // Size the shadow for the full visible surface at BGRA8888, page-aligned so
    // it can be both mapped to userspace and described to the host as a single
    // contiguous backing region.
    let size = (info.width as usize)
        .checked_mul(info.height as usize)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or(SetupError::SizeOverflow)?;
    let pages = size.div_ceil(PAGE_SIZE_4K);

    let shadow =
        alloc_zeroed_pages_exact_ref(pages, PAGE_SIZE_4K, UsageKind::Global).map_err(|_| {
            warn!("failed to allocate {}-page fbdev shadow buffer", pages);
            SetupError::AllocationFailed
        })?;
    let shadow_len_bytes = pages * PAGE_SIZE_4K;

    // FbEmulation retains the page hold for the lifetime of the host resource.
    // Publish only the requested byte range, excluding allocator rounding slack.
    let range = memaddr::PhysAddrRange::try_from_start_size(shadow.phys_addr(), shadow_len_bytes)
        .ok_or(SetupError::SizeOverflow)?;
    let backing = ShadowBacking {
        _pages: &shadow,
        range,
    };
    let resource = ScanoutResource {
        id: FB_RESOURCE_ID,
        width: info.width,
        height: info.height,
        // The shadow is packed (no row padding), so pitch == width * bpp.
        pitch: (info.width as usize * BYTES_PER_PIXEL) as u32,
        format: ScanoutFormat::Bgra8888,
    };
    let created = device.create_scanout_resource(resource, &ScanoutWindow::new(&backing));
    if let Err(e) = created {
        warn!("create_scanout_resource failed on primary display: {:?}", e);
        return Err(SetupError::ResourceSetupFailed);
    }

    *FB.lock() = Some(Arc::new(FbEmulation {
        info,
        device,
        shadow,
        shadow_len_bytes,
    }));
    info!(
        "fbdev emulation ready: {}x{} ({} bytes shadow)",
        info.width, info.height, size
    );
    Ok(())
}

/// Returns whether fbdev emulation is ready (a primary display device was
/// probed and its shadow scanout resource was established successfully).
pub fn fb_available() -> bool {
    FB.is_inited() && FB.lock().is_some()
}

/// Display information for the primary framebuffer device.
///
/// # Panics
///
/// Panics if fbdev emulation is not available; callers must gate on
/// [`fb_available`].
pub fn fb_info() -> DisplayInfo {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: fb_info requires fb_available; the \
                  initialized framebuffer is retained for kernel lifetime"
    )]
    FB.lock()
        .as_ref()
        .expect("fb_info called without fb_available")
        .info
}

/// Virtual address of the emulated framebuffer's shadow buffer.
///
/// Returns `None` if emulation is unavailable. The returned address remains
/// valid for as long as fbdev emulation stays available (i.e. for kernel
/// lifetime, since teardown is not supported).
pub fn fb_shadow_vaddr() -> Option<VirtAddr> {
    FB.lock().as_ref().map(|fb| p2v(fb.shadow.phys_addr()))
}

/// Physical address of the emulated framebuffer's shadow buffer, for `mmap`.
pub fn fb_shadow_paddr() -> Option<PhysAddr> {
    FB.lock().as_ref().map(|fb| fb.shadow.phys_addr())
}

/// Size in bytes of the emulated framebuffer's shadow buffer.
pub fn fb_shadow_size() -> Option<usize> {
    FB.lock().as_ref().map(|fb| fb.shadow_len_bytes)
}

/// Push the shadow buffer to the visible scanout.
///
/// This is on-demand only (no background refresh task): a continuous
/// `present_scanout_resource` would race an active DRM compositor for the
/// single physical scanout and cause flicker. The devfs node triggers a
/// present on write to `/dev/fb0` and via the `FBIOPAN_DISPLAY` ioctl.
/// Returns `true` on success; transient host failures are not fatal.
pub fn fb_present() -> bool {
    let Some(fb) = FB.lock().as_ref().map(Arc::clone) else {
        return false;
    };
    let rect = ScanoutRect {
        x: 0,
        y: 0,
        width: fb.info.width,
        height: fb.info.height,
    };
    fb.device
        .present_scanout_resource(FB_RESOURCE_ID, rect, false)
        .is_ok()
}
