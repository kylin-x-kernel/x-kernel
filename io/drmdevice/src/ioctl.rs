// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The ioctl table: one entry per command, carrying its number, its permission
//! bits and its handler.
//!
//! This table is the ABI inventory. `VERSION` through `GET_BLOB` are listed
//! here once, so:
//!
//! * adding a command is one line, and forgetting to give it a render
//!   permission is the safe default (denied on `/dev/dri/renderD*`);
//! * the dispatch match that used to duplicate every command number is gone;
//! * argument marshalling follows the direction the entry was built with —
//!   `rw` for `_IOWR`, `ro` for `_IOR`, `wo` for `_IOW`.
//!
//! Tracing one command: a row names its wire struct and number, and the
//! struct's `Ioctl` impl names the command again right above `handle`, with
//! its full Linux name (`DRM_IOCTL_*`). Either direction greps to the other:
//! from the table to the handler, or from an ABI name to the table. Rows are
//! sorted by command number.
//!
//! Linux returns `ENOTTY` for a number that is not in its table; this driver
//! reports the same class of failure (`EOPNOTSUPP`) so clients probe instead of
//! assuming support.

use alloc::sync::Arc;

use crate::{framebuffer::DrmModeGetFb, kms::DrmModeSetGamma, legacy::DrmSetUnique, prelude::*};

/// One ioctl request type: decodes nothing itself, it is handed a decoded
/// request and mutates it. Argument transfer lives in the dispatcher below, so
/// a handler cannot get the copy direction wrong.
pub(crate) trait Ioctl: Sized {
    /// Wire name of the command this request serves (`"MODE_SETCRTC"`), shown
    /// in diagnostics and tests.
    const NAME: &'static str;

    /// Handles one decoded request.
    ///
    /// `unit` is the number of the node the request arrived through (the
    /// card's `EVENT` unit); only the commands that need it read it.
    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize>;
}

/// Permission bits of one command, mirroring `DRM_RENDER_ALLOW` / `DRM_MASTER`.
#[derive(Clone, Copy)]
pub(crate) struct IoctlFlags(u32);

impl IoctlFlags {
    /// Requires an authenticated client (or a render client), `DRM_AUTH`.
    pub(crate) const AUTH: Self = Self(4);
    /// The legacy commands Linux guards with `DRM_AUTH | DRM_MASTER`
    /// (`SET_UNIQUE`, `BLOCK`, the drawables).
    pub(crate) const AUTH_MASTER: Self = Self(4 | 2);
    /// Requires the calling file to be the card master.
    pub(crate) const MASTER: Self = Self(2);
    /// No additional permission required.
    pub(crate) const NONE: Self = Self(0);
    /// Allowed on a render node.
    pub(crate) const RENDER_ALLOW: Self = Self(1);

    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// The type of one command's handler.
type Handler = fn(&DrmDevice, &Arc<DrmFile>, u32, usize) -> DrmResult<usize>;

/// Like [`desc_rw`], but the command runs a shared handler (`drm_noop`).
const fn desc_rw_handler<T: Ioctl + UserRead + UserWrite>(
    nr: u8,
    flags: IoctlFlags,
    handler: Handler,
) -> IoctlDesc {
    IoctlDesc {
        cmd: iowr::<T>(DRM_TYPE, nr),
        name: T::NAME,
        flags,
        handler,
    }
}

/// Like [`desc_wo`], but the command runs a shared handler.
const fn desc_wo_handler<T: Ioctl + UserRead>(
    nr: u8,
    flags: IoctlFlags,
    handler: Handler,
) -> IoctlDesc {
    IoctlDesc {
        cmd: iow::<T>(DRM_TYPE, nr),
        name: T::NAME,
        flags,
        handler,
    }
}

/// One command of the driver.
pub(crate) struct IoctlDesc {
    /// Wire command number.
    pub cmd: u32,
    /// Command name, for diagnostics and tests. Wire structs carry it as
    /// [`Ioctl::NAME`]; the two commands without a request struct pass it in
    /// [`desc_plain`].
    pub name: &'static str,
    /// Permission bits.
    pub flags: IoctlFlags,
    handler: fn(&DrmDevice, &Arc<DrmFile>, u32, usize) -> DrmResult<usize>,
}

impl IoctlDesc {
    /// One command whose number is computed elsewhere.
    ///
    /// The core table uses [`desc_rw`] and friends; the virtio-gpu range is
    /// offset by `DRM_COMMAND_BASE`, so its table builds entries here.
    pub(crate) const fn new<T: Ioctl + UserRead + UserWrite>(
        cmd: u32,
        name: &'static str,
        flags: IoctlFlags,
    ) -> Self {
        Self {
            cmd,
            name,
            flags,
            handler: rw::<T>,
        }
    }
}

const fn desc_rw<T: Ioctl + UserRead + UserWrite>(nr: u8, flags: IoctlFlags) -> IoctlDesc {
    IoctlDesc {
        cmd: iowr::<T>(DRM_TYPE, nr),
        name: T::NAME,
        flags,
        handler: rw::<T>,
    }
}

/// `drm_noop`: the request is parsed, nothing happens.
fn noop_entry(_dev: &DrmDevice, _file: &Arc<DrmFile>, _unit: u32, _arg: usize) -> DrmResult<usize> {
    Ok(0)
}

/// `drm_invalid_op`.
fn invalid_op_entry(
    _dev: &DrmDevice,
    _file: &Arc<DrmFile>,
    _unit: u32,
    _arg: usize,
) -> DrmResult<usize> {
    Err(DrmError::InvalidInput)
}

const fn desc_ro<T: Ioctl + Default + UserWrite>(nr: u8, flags: IoctlFlags) -> IoctlDesc {
    IoctlDesc {
        cmd: ior::<T>(DRM_TYPE, nr),
        name: T::NAME,
        flags,
        handler: ro::<T>,
    }
}

const fn desc_wo<T: Ioctl + UserRead>(nr: u8, flags: IoctlFlags) -> IoctlDesc {
    IoctlDesc {
        cmd: iow::<T>(DRM_TYPE, nr),
        name: T::NAME,
        flags,
        handler: wo::<T>,
    }
}

const fn desc_plain(
    nr: u8,
    name: &'static str,
    flags: IoctlFlags,
    handler: fn(&DrmDevice, &Arc<DrmFile>, u32, usize) -> DrmResult<usize>,
) -> IoctlDesc {
    IoctlDesc {
        cmd: io(DRM_TYPE, nr),
        name,
        flags,
        handler,
    }
}

/// The commands this driver implements, in command-number order.
pub(crate) static IOCTLS: &[IoctlDesc] = &[
    desc_rw::<DrmVersion>(0x00, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmUnique>(0x01, IoctlFlags::NONE),
    desc_ro::<DrmAuth>(0x02, IoctlFlags::NONE),
    // `DRM_IOCTL_SET_VERSION`, `GEM_FLINK` and `GEM_OPEN` carry `DRM_MASTER`
    // or `DRM_AUTH` in Linux's table, not `DRM_RENDER_ALLOW`.
    desc_rw::<DrmSetVersion>(0x07, IoctlFlags::MASTER),
    desc_wo::<DrmGemClose>(0x09, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmGemFlink>(0x0A, IoctlFlags::AUTH),
    desc_rw::<DrmGemOpen>(0x0B, IoctlFlags::AUTH),
    desc_rw::<DrmGetCap>(0x0C, IoctlFlags::RENDER_ALLOW),
    desc_wo::<DrmSetClientCap>(0x0D, IoctlFlags::NONE),
    // The legacy core commands, with Linux's flags and answers: `GET_CLIENT`
    // and `GET_STATS` are hollowed out, `SET_UNIQUE` is `drm_invalid_op`, and
    // `BLOCK` through `FINISH` are `drm_noop`.
    desc_rw::<DrmClient>(0x05, IoctlFlags::NONE),
    desc_ro::<DrmStats>(0x06, IoctlFlags::NONE),
    desc_wo_handler::<DrmSetUnique>(0x10, IoctlFlags::AUTH_MASTER, invalid_op_entry),
    desc_wo::<DrmAuthMagic>(0x11, IoctlFlags::MASTER),
    desc_rw_handler::<DrmBlock>(0x12, IoctlFlags::AUTH_MASTER, noop_entry),
    desc_rw_handler::<DrmBlock>(0x13, IoctlFlags::AUTH_MASTER, noop_entry),
    desc_rw_handler::<DrmDraw>(0x27, IoctlFlags::AUTH_MASTER, noop_entry),
    desc_rw_handler::<DrmDraw>(0x28, IoctlFlags::AUTH_MASTER, noop_entry),
    desc_wo_handler::<DrmLock>(0x2C, IoctlFlags::AUTH, noop_entry),
    desc_wo_handler::<DrmUpdateDraw>(0x3F, IoctlFlags::AUTH_MASTER, noop_entry),
    desc_plain(0x1E, "SET_MASTER", IoctlFlags::NONE, set_master_entry),
    desc_plain(0x1F, "DROP_MASTER", IoctlFlags::NONE, drop_master_entry),
    desc_rw::<DrmPrimeHandle>(0x2D, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmPrimeFdToHandle>(0x2E, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmWaitVblank>(0x3A, IoctlFlags::NONE),
    desc_rw::<DrmCrtcGetSequence>(0x3B, IoctlFlags::NONE),
    desc_rw::<DrmCrtcQueueSequence>(0x3C, IoctlFlags::NONE),
    desc_rw::<DrmModeCardRes>(0xA0, IoctlFlags::NONE),
    desc_rw::<DrmModeCrtc>(0xA1, IoctlFlags::NONE),
    desc_rw::<DrmModeSetCrtc>(0xA2, IoctlFlags::MASTER),
    desc_rw::<DrmModeCursor>(0xA3, IoctlFlags::MASTER),
    desc_rw::<DrmModeCrtcLut>(0xA4, IoctlFlags::NONE),
    desc_rw::<DrmModeSetGamma>(0xA5, IoctlFlags::MASTER),
    desc_rw::<DrmModeGetEncoder>(0xA6, IoctlFlags::NONE),
    desc_rw::<DrmModeGetConnector>(0xA7, IoctlFlags::NONE),
    desc_rw_handler::<DrmModeModeCmd>(0xA8, IoctlFlags::MASTER, noop_entry),
    desc_rw_handler::<DrmModeModeCmd>(0xA9, IoctlFlags::MASTER, noop_entry),
    desc_rw::<DrmModeGetProperty>(0xAA, IoctlFlags::NONE),
    desc_rw::<DrmModeConnectorSetProperty>(0xAB, IoctlFlags::MASTER),
    desc_rw::<DrmModeGetBlob>(0xAC, IoctlFlags::NONE),
    desc_rw::<DrmModeGetFb>(0xAD, IoctlFlags::NONE),
    desc_rw::<DrmModeFbCmd>(0xAE, IoctlFlags::NONE),
    desc_rw::<DrmModeRmFb>(0xAF, IoctlFlags::NONE),
    desc_rw::<DrmModeCrtcPageFlip>(0xB0, IoctlFlags::MASTER),
    desc_rw::<DrmModeDirtyFb>(0xB1, IoctlFlags::MASTER),
    desc_rw::<DrmModeCreateDumb>(0xB2, IoctlFlags::NONE),
    desc_rw::<DrmModeMapDumb>(0xB3, IoctlFlags::NONE),
    desc_rw::<DrmModeDestroyDumb>(0xB4, IoctlFlags::NONE),
    desc_rw::<DrmModeGetPlaneRes>(0xB5, IoctlFlags::NONE),
    desc_rw::<DrmModeGetPlane>(0xB6, IoctlFlags::NONE),
    desc_rw::<DrmModeSetPlane>(0xB7, IoctlFlags::MASTER),
    desc_rw::<DrmModeFbCmd2>(0xB8, IoctlFlags::NONE),
    // `DRM_IOCTL_MODE_GETFB2` is 0xCE in `drm.h` (after the lease commands),
    // not next to `ADDFB2`; the number is part of the ABI.
    desc_rw::<DrmModeGetFb2>(0xCE, IoctlFlags::NONE),
    desc_rw::<DrmModeObjGetProperties>(0xB9, IoctlFlags::NONE),
    desc_rw::<DrmModeObjSetProperty>(0xBA, IoctlFlags::MASTER),
    desc_rw::<DrmModeCursor2>(0xBB, IoctlFlags::MASTER),
    desc_rw::<DrmModeAtomic>(0xBC, IoctlFlags::MASTER),
    desc_rw::<DrmModeCreateBlob>(0xBD, IoctlFlags::NONE),
    desc_rw::<DrmModeDestroyBlob>(0xBE, IoctlFlags::NONE),
    desc_rw::<DrmModeCloseFb>(0xD0, IoctlFlags::NONE),
    desc_rw::<DrmSetClientName>(0xD1, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmGemChangeHandle>(0xD2, IoctlFlags::RENDER_ALLOW),
    // Sync objects. The numbers are the ones `include/uapi/drm/drm.h` assigns:
    // CREATE 0xBF, DESTROY 0xC0, HANDLE_TO_FD 0xC1, FD_TO_HANDLE 0xC2,
    // WAIT 0xC3, RESET 0xC4, SIGNAL 0xC5, then the lease commands (0xC6-0xC9)
    // this driver does not implement, then TIMELINE_WAIT 0xCA, QUERY 0xCB,
    // TRANSFER 0xCC, TIMELINE_SIGNAL 0xCD, GETFB2 0xCE and EVENTFD 0xCF.
    desc_rw::<DrmSyncobjCreate>(0xBF, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjDestroy>(0xC0, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjHandle>(0xC1, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjFdToHandle>(0xC2, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjWait>(0xC3, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjReset>(0xC4, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjSignal>(0xC5, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjTimelineWait>(0xCA, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjQuery>(0xCB, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjTransfer>(0xCC, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjTimelineArray>(0xCD, IoctlFlags::RENDER_ALLOW),
    desc_rw::<DrmSyncobjEventfd>(0xCF, IoctlFlags::RENDER_ALLOW),
];

/// Finds one command by number.
pub(crate) fn lookup(cmd: u32) -> Option<&'static IoctlDesc> {
    IOCTLS
        .iter()
        .chain(crate::virtgpu::table())
        .find(|desc| desc.cmd == cmd)
}

/// Checks the permission bits of one command against the calling file.
///
/// Mastership is asked of the device, never of the file: the per-file auth
/// state only remembers which master object the file is bound to.
pub(crate) fn permit(dev: &DrmDevice, flags: IoctlFlags, file: &Arc<DrmFile>) -> DrmResult<()> {
    check_permission(
        file.kind,
        flags,
        dev.is_master(file),
        file.is_authenticated(),
    )
}

/// The permission rule as a pure function.
///
/// Kept separate from `permit` so the matrix (`command x node x ownership`) can
/// be proven in a unit test without constructing a card.
pub(crate) fn check_permission(
    kind: MinorKind,
    flags: IoctlFlags,
    is_master: bool,
    is_authenticated: bool,
) -> DrmResult<()> {
    // Linux `drm_ioctl_permit()`, in its order: `DRM_AUTH` admits an
    // authenticated file or a render client, `DRM_MASTER` needs the card, and
    // a render client may only issue commands marked `DRM_RENDER_ALLOW`.
    if flags.contains(IoctlFlags::AUTH) && kind != MinorKind::Render && !is_authenticated {
        return Err(DrmError::PermissionDenied);
    }
    if flags.contains(IoctlFlags::MASTER) && !is_master {
        return Err(DrmError::PermissionDenied);
    }
    if kind == MinorKind::Render && !flags.contains(IoctlFlags::RENDER_ALLOW) {
        return Err(DrmError::PermissionDenied);
    }
    Ok(())
}

/// Validates the permission and runs one command.
///
/// `unit` is the number of the node the command arrived through: the card's
/// `EVENT` unit, which `SYNCOBJ_EVENTFD` accounting is keyed by.
pub(crate) fn dispatch(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    unit: u32,
    cmd: u32,
    arg: usize,
) -> DrmResult<usize> {
    let Some(desc) = lookup(cmd) else {
        log::debug!("drm: unknown ioctl cmd={cmd:#x}");
        return Err(DrmError::OpNotSupported);
    };
    if let Err(error) = permit(dev, desc.flags, file) {
        log::debug!("drm: {} denied: {error:?}", desc.name);
        return Err(error);
    }
    (desc.handler)(dev, file, unit, arg)
}

fn rw<T: Ioctl + UserRead + UserWrite>(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    unit: u32,
    arg: usize,
) -> DrmResult<usize> {
    let user = UserPtr::<T>::from(arg);
    let mut request: T = user.read_vm().map_err(|_| DrmError::BadAddress)?;
    let result = T::handle(dev, file, unit, &mut request);
    user.write_vm(request).map_err(|_| DrmError::BadAddress)?;
    result
}

fn ro<T: Ioctl + Default + UserWrite>(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    unit: u32,
    arg: usize,
) -> DrmResult<usize> {
    let user = UserPtr::<T>::from(arg);
    let mut request = T::default();
    let result = T::handle(dev, file, unit, &mut request);
    user.write_vm(request).map_err(|_| DrmError::BadAddress)?;
    result
}

fn wo<T: Ioctl + UserRead>(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    unit: u32,
    arg: usize,
) -> DrmResult<usize> {
    let user = UserPtr::<T>::from(arg);
    let mut request: T = user.read_vm().map_err(|_| DrmError::BadAddress)?;
    T::handle(dev, file, unit, &mut request)
}

fn set_master_entry(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    _unit: u32,
    _arg: usize,
) -> DrmResult<usize> {
    dev.set_master(file)?;
    Ok(0)
}

fn drop_master_entry(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    _unit: u32,
    _arg: usize,
) -> DrmResult<usize> {
    dev.drop_master(file)?;
    Ok(0)
}

/// Copies an id array, reporting the required length.
pub(crate) fn report_user_array<T: UserWrite + Copy>(
    user_ptr: UserPtr<T>,
    capacity: u32,
    values: &[T],
) -> DrmResult<u32> {
    let copy = (capacity as usize).min(values.len());
    // A null pointer is only a length query when there is nothing to copy.
    // With a non-zero destination length Linux faults with EFAULT instead of
    // silently reporting success and leaving the caller's buffer untouched.
    if copy != 0 {
        user_ptr
            .write_vm_slice(&values[..copy])
            .map_err(|_| DrmError::BadAddress)?;
    }
    Ok(values.len() as u32)
}
