// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GEM objects behind the dumb-buffer ABI (`drm_gem.c`).
//!
//! The object owns its backing pages, its scanout resource and the knowledge of
//! how to free that resource. Everything else — handles, framebuffers, the
//! scanout slot — holds an `Arc<GemObject>`, so "a resource must outlive every
//! handle that named it and the scanout that displays it" needs no bookkeeping:
//! it is the last reference that destroys the resource.

use crate::prelude::*;

/// One allocation exposed through the dumb-buffer ABI.
pub(crate) struct GemObject {
    backend: BackendHandle,
    spec: ResourceSpec,
    pages: Arc<PageRef>,
    /// Page-aligned mapping length, excluding allocation rounding slack.
    length_bytes: usize,
    /// Offset this object is mapped at, in bytes (page-aligned by construction).
    offset_bytes: u64,
    handle: DumbHandle,
    /// Resource id, taken exactly once by the destructor.
    resource: Mutex<Option<ResourceId>>,
}

impl GemObject {
    /// Allocates zeroed contiguous backing and creates the scanout resource.
    ///
    /// Geometry validation belongs to the ioctl layer; this function only
    /// assumes `width`/`height` are non-zero and `width * 4 * height` fits.
    pub(crate) fn create(dev: &DrmDevice, width: u32, height: u32) -> DrmResult<Arc<Self>> {
        let pitch = width.checked_mul(4).ok_or(DrmError::InvalidInput)?;
        let size_bytes = (pitch as u64)
            .checked_mul(height as u64)
            .ok_or(DrmError::InvalidInput)?;
        if size_bytes as usize > DUMB_BUFFER_MAX_SIZE {
            return Err(DrmError::NoMemory);
        }
        let length_bytes = (size_bytes as usize).next_multiple_of(PAGE_SIZE_4K);
        let pages = alloc_zeroed_pages_exact_ref(
            length_bytes / PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            UsageKind::Global,
        )
        .map_err(|_| DrmError::NoMemory)?;
        let paddr = pages.phys_addr().as_usize() as u64;
        let spec = ResourceSpec {
            width,
            height,
            pitch,
            format: ScanoutFormat::Bgra8888,
        };
        let resource = dev.next_resource_id();
        let backend = dev.backend()?.clone();
        backend.create_resource(resource, spec, paddr, length_bytes as u32)?;
        Ok(Arc::new(Self {
            backend,
            spec,
            pages: Arc::new(pages),
            length_bytes,
            offset_bytes: dev.next_mmap_offset_bytes(),
            handle: dev.next_dumb_handle(),
            resource: Mutex::new(Some(resource)),
        }))
    }

    /// Pixel width.
    pub(crate) const fn width(&self) -> u32 {
        self.spec.width
    }

    /// Pixel height.
    pub(crate) const fn height(&self) -> u32 {
        self.spec.height
    }

    /// Row stride in bytes.
    pub(crate) const fn pitch(&self) -> u32 {
        self.spec.pitch
    }

    /// Offset this object is mapped at, in bytes, as `MAP_DUMB` reports it.
    pub(crate) const fn offset_bytes(&self) -> u64 {
        self.offset_bytes
    }

    /// Handle the object was created under.
    pub(crate) const fn handle(&self) -> DumbHandle {
        self.handle
    }

    /// Backing pages, for the mapping path.
    pub(crate) fn pages(&self) -> Arc<PageRef> {
        self.pages.clone()
    }

    /// Page-aligned mapping length requested for this object's pixels.
    pub(crate) const fn length_bytes(&self) -> usize {
        self.length_bytes
    }

    /// The scanout resource id.
    pub(crate) fn resource(&self) -> DrmResult<ResourceId> {
        self.resource.lock().ok_or(DrmError::NotFound)
    }
}

impl Drop for GemObject {
    fn drop(&mut self) {
        // Freeing outside any caller's lock is the destructor's job; a backend
        // that already lost its device reports Io and there is nothing left to
        // do about it.
        if let Some(resource) = self.resource.lock().take()
            && let Err(error) = self.backend.destroy_resource(resource)
        {
            log::warn!("drm: cannot destroy resource {resource:?}: {error:?}");
        }
    }
}

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`: same wire structure as the forward call.
#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmPrimeFdToHandle(pub(crate) DrmPrimeHandle);

/// `DRM_IOCTL_MODE_CREATE_DUMB`.
impl Ioctl for DrmModeCreateDumb {
    const NAME: &'static str = "MODE_CREATE_DUMB";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, c: &mut Self) -> DrmResult<usize> {
        // The backing store is always BGRA8888 (32bpp), but 24bpp requests must
        // still be accepted: libdrm-based clients on the non-atomic path ask for
        // 24bpp and hand that value back to ADDFB.
        if c.width == 0 || c.height == 0 || c.flags != 0 || !matches!(c.bpp, 24 | 32) {
            return Err(DrmError::InvalidInput);
        }
        if c.width > 16384 || c.height > 16384 {
            return Err(DrmError::InvalidInput);
        }
        let object = GemObject::create(dev, c.width, c.height)?;
        c.pitch = object.pitch();
        c.size = object.pitch() as u64 * object.height() as u64;
        c.handle = object.handle().raw();
        file.handles.lock().insert(object.handle(), object);
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_MAP_DUMB`.
impl Ioctl for DrmModeMapDumb {
    const NAME: &'static str = "MODE_MAP_DUMB";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, m: &mut Self) -> DrmResult<usize> {
        // Only an offset this file may map is reported, which mirrors the
        // per-file handle table `mmap` checks again.
        let object = file
            .handle(DumbHandle::from_raw(m.handle))
            .ok_or(DrmError::InvalidInput)?;
        m.offset = object.offset_bytes();
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_DESTROY_DUMB`.
impl Ioctl for DrmModeDestroyDumb {
    const NAME: &'static str = "MODE_DESTROY_DUMB";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, d: &mut Self) -> DrmResult<usize> {
        // Legacy entry point: an unknown handle stays a silent success, because
        // callers free a dumb buffer without reading the result. The object is
        // released when the last reference (a framebuffer or the scanout) goes.
        let _ = file.take_handle(DumbHandle::from_raw(d.handle));
        Ok(0)
    }
}

/// `DRM_IOCTL_GEM_CLOSE`.
impl Ioctl for DrmGemClose {
    const NAME: &'static str = "GEM_CLOSE";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, args: &mut Self) -> DrmResult<usize> {
        // `drm_gem_close_ioctl()` rejects a non-zero `pad` and reports an
        // unknown handle as EINVAL (through `drm_gem_handle_delete()`).
        if args.pad != 0 {
            return Err(DrmError::InvalidInput);
        }
        file.take_handle(DumbHandle::from_raw(args.handle))
            .ok_or(DrmError::InvalidInput)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_PRIME_HANDLE_TO_FD`.
impl Ioctl for DrmPrimeHandle {
    const NAME: &'static str = "PRIME_HANDLE_TO_FD";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, prime: &mut Self) -> DrmResult<usize> {
        // Protocol stub, as documented in docs/render-node.md: without a dma-buf
        // implementation the "fd" is the handle and GET_CAP keeps
        // `DRM_CAP_PRIME == 0`. The handle still has to exist for this file,
        // which is what makes the render-node test meaningful: the command is
        // allowed there, an unknown handle is EINVAL.
        if file.handle(DumbHandle::from_raw(prime.handle)).is_none() {
            return Err(DrmError::InvalidInput);
        }
        prime.fd = prime.handle as i32;
        Ok(0)
    }
}

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`.
impl Ioctl for DrmPrimeFdToHandle {
    const NAME: &'static str = "PRIME_FD_TO_HANDLE";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, prime: &mut Self) -> DrmResult<usize> {
        let handle = DumbHandle::from_raw(prime.0.fd as u32);
        if file.handle(handle).is_none() {
            return Err(DrmError::InvalidInput);
        }
        prime.0.handle = handle.raw();
        Ok(0)
    }
}
