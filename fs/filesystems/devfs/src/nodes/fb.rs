// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::slice;

use kerrno::KError;
use khal::mem::v2p;
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, MmapMapper, NodeFlags, SimpleFs, VfsError, VfsFile,
    VfsResult,
};
use memaddr::{PhysAddrRange, VirtAddr};
use osvm::VirtMutPtr;

use crate::{DeviceFile, add_device_entry};

// Types from https://github.com/Tangzh33/asterinas

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct FrameBufferBitfield {
    /// The beginning of bitfield.
    offset: u32,
    /// The length of bitfield.
    length: u32,
    /// Most significant bit is right(!= 0).
    msb_right: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VarScreenInfo {
    pub xres: u32, // Visible resolution
    pub yres: u32,
    pub xres_virtual: u32, // Virtual resolution
    pub yres_virtual: u32,
    pub xoffset: u32, // Offset from virtual to visible
    pub yoffset: u32,
    pub bits_per_pixel: u32, // Guess what
    pub grayscale: u32,      // 0 = color, 1 = grayscale, >1 = FOURCC
    // Add other fields as needed
    pub red: FrameBufferBitfield, // Bitfield in framebuffer memory if true color
    pub green: FrameBufferBitfield, // Else only length is significant
    pub blue: FrameBufferBitfield,
    pub transp: FrameBufferBitfield, // Transparency
    pub nonstd: u32,                 // Non-standard pixel format
    pub activate: u32,               // See FB_ACTIVATE_*
    pub height: u32,                 // Height of picture in mm
    pub width: u32,                  // Width of picture in mm
    pub accel_flags: u32,            // (OBSOLETE) see fb_info.flags
    pub pixclock: u32,               // Pixel clock in ps (pico seconds)
    pub left_margin: u32,            // Time from sync to picture
    pub right_margin: u32,           // Time from picture to sync
    pub upper_margin: u32,           // Time from sync to picture
    pub lower_margin: u32,
    pub hsync_len: u32,     // Length of horizontal sync
    pub vsync_len: u32,     // Length of vertical sync
    pub sync: u32,          // See FB_SYNC_*
    pub vmode: u32,         // See FB_VMODE_*
    pub rotate: u32,        // Angle we rotate counter-clockwise
    pub colorspace: u32,    // Colorspace for FOURCC-based modes
    pub reserved: [u32; 4], // Reserved for future compatibility
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FixScreenInfo {
    pub id: [u8; 16],       // Identification string, e.g., "TT Builtin"
    pub smem_start: u64,    // Start of framebuffer memory (physical address)
    pub smem_len: u32,      // Length of framebuffer memory
    pub type_: u32,         // See FB_TYPE_*
    pub type_aux: u32,      // Interleave for interleaved planes
    pub visual: u32,        // See FB_VISUAL_*
    pub xpanstep: u16,      // Zero if no hardware panning
    pub ypanstep: u16,      // Zero if no hardware panning
    pub ywrapstep: u16,     // Zero if no hardware ywrap
    pub line_length: u32,   // Length of a line in bytes
    pub mmio_start: u64,    // Start of Memory Mapped I/O (physical address)
    pub mmio_len: u32,      // Length of Memory Mapped I/O
    pub accel: u32,         // Indicate to driver which specific chip/card we have
    pub capabilities: u16,  // See FB_CAP_*
    pub reserved: [u16; 2], // Reserved for future compatibility
}

/// Framebuffer device for graphics output, backed by fbdev emulation's shadow
/// buffer (see [`fbdevice`]).
///
/// This node only exposes the shadow buffer for userspace read/write/mmap; it
/// deliberately does **not** run a background refresh task pushing the shadow
/// to the scanout. A continuous `present_scanout_resource` would race a DRM
/// compositor (e.g. Weston) for the single physical scanout and cause
/// flickering. Instead the shadow is pushed to the scanout on explicit
/// demand: `write()` to `/dev/fb0` and the `FBIOPAN_DISPLAY` ioctl both
/// trigger [`fbdevice::fb_present`], so writes to the node stay visible while
/// an active DRM master keeps the scanout to itself.
pub struct FrameBuffer {
    base: VirtAddr,
    size: usize,
}
impl FrameBuffer {
    pub fn new() -> Self {
        // fb_available() is checked by the caller (add_root_entries) before
        // constructing us, so the shadow is guaranteed present.
        let base = fbdevice::fb_shadow_vaddr().expect("fbdev shadow missing");
        let size = fbdevice::fb_shadow_size().expect("fbdev shadow missing");
        Self { base, size }
    }

    #[allow(clippy::mut_from_ref)]
    fn as_mut_slice(&self) -> &mut [u8] {
        // SAFETY: `base..base+size` is the fbdev emulation shadow buffer,
        // which stays mapped for the kernel lifetime; concurrent userspace
        // writes to a mapped page race here, matching legacy fbdev semantics.
        unsafe { slice::from_raw_parts_mut(self.base.as_mut_ptr(), self.size) }
    }
}
impl DeviceFileOps for FrameBuffer {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let slice = self.as_mut_slice();
        let len = buf
            .len()
            .min((slice.len() as u64).saturating_sub(offset) as usize);
        buf[..len].copy_from_slice(&slice[..len]);
        Ok(len)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let slice = self.as_mut_slice();
        if offset >= slice.len() as u64 {
            return Err(VfsError::StorageFull);
        }
        let len = buf.len().min(slice.len() - offset as usize);
        slice[..len].copy_from_slice(&buf[..len]);
        // Make the written pixels visible: write() is the fbdev "explicit
        // demand" trigger (see module docs). Failure is not fatal for the
        // write itself; the host may be busy presenting another resource.
        fbdevice::fb_present();
        Ok(len)
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            // FBIOGET_VSCREENINFO
            0x4600 => {
                let info = fbdevice::fb_info();
                // The shadow scanout buffer is always BGRA8888 (32bpp); the
                // line length reported in FSCREENINFO is width * 4 with no
                // row padding.
                let bpp = 32u32;
                (arg as *mut VarScreenInfo).write_vm(VarScreenInfo {
                    xres: info.width,
                    yres: info.height,
                    xres_virtual: info.width,
                    yres_virtual: info.height,
                    xoffset: 0,
                    yoffset: 0,
                    bits_per_pixel: bpp,
                    grayscale: 0,
                    red: FrameBufferBitfield {
                        offset: 16,
                        length: 8,
                        msb_right: 0,
                    },
                    green: FrameBufferBitfield {
                        offset: 8,
                        length: 8,
                        msb_right: 0,
                    },
                    blue: FrameBufferBitfield {
                        offset: 0,
                        length: 8,
                        msb_right: 0,
                    },
                    transp: FrameBufferBitfield {
                        offset: 24,
                        length: 8,
                        msb_right: 0,
                    },
                    nonstd: 0,
                    activate: 0,
                    height: 0,
                    width: 0,
                    accel_flags: 0,
                    pixclock: 10000000 / info.width * 1000 / info.height,
                    left_margin: (info.width / 8) & 0xf8,
                    right_margin: 32,
                    upper_margin: 16,
                    lower_margin: 4,
                    hsync_len: (info.width / 8) & 0xf8,
                    vsync_len: 4,
                    sync: 0,
                    vmode: 0,
                    rotate: 0,
                    colorspace: 0,
                    reserved: [0; 4],
                })?;
                Ok(0)
            }
            // FBIOPUT_VSCREENINFO
            0x4601 => Ok(0),
            // FBIOGET_FSCREENINFO
            0x4602 => {
                let info = fbdevice::fb_info();
                let line_length = info.width.checked_mul(4).unwrap_or(0);
                (arg as *mut FixScreenInfo).write_vm(FixScreenInfo {
                    id: *b"Virtio Framebuf\0",
                    // smem_start is the physical base of the shadow buffer, so
                    // userspace (and the kernel mmap path) describe the same
                    // backing memory the host scanout resource is bound to.
                    smem_start: v2p(self.base).as_usize() as u64,
                    smem_len: self.size as u32,
                    type_: 0,
                    type_aux: 0,
                    visual: 2, // FB_VISUAL_TRUECOLOR
                    xpanstep: 0,
                    ypanstep: 0,
                    ywrapstep: 0,
                    line_length,
                    mmio_start: 0,
                    mmio_len: 0,
                    accel: 0,
                    capabilities: 0,
                    reserved: [0; 2],
                })?;
                Ok(0)
            }
            // FBIOGETCMAP
            0x4604 => Ok(0),
            // FBIOPUTCMAP
            0x4605 => Ok(0),
            // FBIOPAN_DISPLAY: explicit "make the shadow visible" trigger for
            // apps that mmap the framebuffer and draw directly (writes via
            // write() present automatically). The framebuffer is a fixed
            // full-screen buffer, so panning is a no-op pan + refresh.
            0x4606 => {
                fbdevice::fb_present();
                Ok(0)
            }
            // FBIOBLANK
            0x4611 => Err(KError::InvalidInput),
            _ => Err(KError::NotATty),
        }
    }

    fn mmap(&self, _file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let paddr = fbdevice::fb_shadow_paddr().ok_or(VfsError::NoSuchDevice)?;
        mapper.map_physical(PhysAddrRange::from_start_size(paddr, self.size))
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    if !fbdevice::fb_available() {
        return;
    }
    let fb0 = DeviceFile::new_character(
        fs.clone(),
        DeviceId::new(29, 0),
        Arc::new(FrameBuffer::new()),
    );
    add_device_entry(root, "fb0", fb0);
}
