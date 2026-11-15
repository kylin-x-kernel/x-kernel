// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for the parts of the object model that are pure functions: the
//! permission matrix over the ioctl table and the mode-blob encoding.

use unittest::{assert, assert_eq, def_test};

use crate::{
    MinorKind,
    ioctl::{IOCTLS, Ioctl, IoctlFlags, check_permission, report_user_array},
    kms::{
        ObjectType, PROPS, PropKind, apply, atomic_flags_valid, mode_blob_bytes, mode_from_bytes,
        wait_vblank_crtc_valid, wait_vblank_flags_valid,
    },
    model::{
        syncobj::{Syncobj, SyncobjHandle, SyncobjTable},
        vblank::{
            SequenceTracker, TickHandler, VblankClockState, VblankRequest, resolve_absolute,
            resolve_relative,
        },
    },
    prelude::*,
    sync::{
        dma_fence::{DmaFence, SyncContext, Wait, new_anonymous_fence},
        dma_resv::{DmaResv, ResvUsage},
        fence_access::fence_array,
        fence_array::FenceArray,
        fence_chain,
        fence_list::{FenceList, first_error, is_signaled as list_is_signaled},
    },
};

/// The resource layout a test framebuffer names: its object's whole memory.
///
/// A framebuffer's geometry is independent of its object's, so every test that
/// drives the commit path by hand states it.
fn object_spec(object: &Arc<GemObject>) -> crate::model::backend::ResourceSpec {
    object.spec()
}

/// A framebuffer geometry that is not its object's whole memory.
///
/// Tests that need the framebuffer to own a scanout resource of its own use
/// this: half the object's height at the same stride.
fn window_spec(object: &Arc<GemObject>) -> crate::model::backend::ResourceSpec {
    crate::model::backend::ResourceSpec {
        height: object.height() / 2,
        ..object.spec()
    }
}

#[def_test]
fn command_numbers_are_unique() {
    for (index, desc) in IOCTLS.iter().enumerate() {
        for other in &IOCTLS[index + 1..] {
            assert!(
                desc.cmd != other.cmd,
                "duplicate command number for {} and {}",
                desc.name,
                other.name
            );
        }
    }
}

#[def_test]
fn resource_array_rejects_a_null_destination() {
    assert_eq!(
        report_user_array(UserPtr::default(), 1, &[CRTC_ID]),
        Err(DrmError::BadAddress),
        "a non-empty destination must not accept a null pointer"
    );
    assert_eq!(
        report_user_array(UserPtr::default(), 0, &[CRTC_ID]),
        Ok(1),
        "a null pointer with zero capacity is the length query"
    );
}

#[def_test]
fn command_numbers_match_the_uapi_header() {
    // Pinned from `include/uapi/drm/drm.h`, not derived from the table under
    // test: a command that drifts to another number is invisible to a
    // libdrm-built client (the ioctl either lands on another command or on
    // nothing at all), so the numbers are asserted literally.
    // (name, number, request size): the number encodes the size, so a struct
    // that changes shape must fail here rather than at runtime in a client.
    let expected: &[(&str, u8, u16)] = &[
        ("VERSION", 0x00, 64),
        ("GEM_CLOSE", 0x09, 8),
        ("GEM_FLINK", 0x0A, 8),
        ("GEM_OPEN", 0x0B, 16),
        ("GET_CAP", 0x0C, 16),
        ("PRIME_HANDLE_TO_FD", 0x2D, 12),
        ("PRIME_FD_TO_HANDLE", 0x2E, 12),
        ("MODE_ADDFB2", 0xB8, 104),
        ("MODE_OBJ_GETPROPERTIES", 0xB9, 32),
        ("MODE_CURSOR2", 0xBB, 36),
        ("MODE_ATOMIC", 0xBC, 56),
        ("SYNCOBJ_CREATE", 0xBF, 8),
        ("SYNCOBJ_DESTROY", 0xC0, 8),
        ("SYNCOBJ_HANDLE_TO_FD", 0xC1, 16),
        ("SYNCOBJ_FD_TO_HANDLE", 0xC2, 16),
        ("SYNCOBJ_WAIT", 0xC3, 40),
        ("SYNCOBJ_RESET", 0xC4, 16),
        ("SYNCOBJ_SIGNAL", 0xC5, 16),
        ("SYNCOBJ_TIMELINE_WAIT", 0xCA, 48),
        ("SYNCOBJ_QUERY", 0xCB, 24),
        ("SYNCOBJ_TRANSFER", 0xCC, 32),
        ("SYNCOBJ_TIMELINE_SIGNAL", 0xCD, 24),
        ("MODE_GETFB2", 0xCE, 104),
        ("SYNCOBJ_EVENTFD", 0xCF, 24),
        ("MODE_CLOSEFB", 0xD0, 8),
    ];
    for (name, number, size) in expected {
        let desc = IOCTLS
            .iter()
            .find(|desc| desc.name == *name)
            .unwrap_or_else(|| panic!("{name} must be in the table"));
        let encoded_size = ((desc.cmd >> 16) & 0x3fff) as u16;
        assert_eq!(encoded_size, *size, "{name} has the wrong request size");
        assert_eq!(
            (desc.cmd & 0xff) as u8,
            *number,
            "{name} has the wrong command number"
        );
    }
}

#[def_test]
fn render_nodes_only_accept_render_allowed_commands() {
    // `(command x node)` matrix: on a render node the answer is exactly the
    // `DRM_RENDER_ALLOW` bit, and the failure is EACCES (`PermissionDenied`).
    for desc in IOCTLS {
        let allowed = desc.flags.contains(IoctlFlags::RENDER_ALLOW);
        let result = check_permission(MinorKind::Render, desc.flags, true, false);
        assert_eq!(
            result.is_ok(),
            allowed,
            "{} disagrees with its render permission",
            desc.name
        );
        if !allowed {
            assert_eq!(
                result.err(),
                Some(DrmError::PermissionDenied),
                "{} must fail with EACCES on a render node",
                desc.name
            );
        }
    }
}

#[def_test]
fn auth_commands_need_an_authenticated_client() {
    // From Linux's table: `GEM_FLINK` and `GEM_OPEN` are `DRM_AUTH`, not
    // `DRM_RENDER_ALLOW`, so an anonymous primary client cannot issue them and
    // a render client (which is exempt from `DRM_AUTH`) still cannot either.
    let auth = ["GEM_FLINK", "GEM_OPEN"];
    for desc in IOCTLS {
        if !auth.contains(&desc.name) {
            continue;
        }
        assert!(
            desc.flags.contains(IoctlFlags::AUTH),
            "{} must carry DRM_AUTH",
            desc.name
        );
        assert_eq!(
            check_permission(MinorKind::Primary, desc.flags, false, false),
            Err(DrmError::PermissionDenied),
            "{} needs an authenticated client",
            desc.name
        );
        assert!(
            check_permission(MinorKind::Primary, desc.flags, false, true).is_ok(),
            "{} must be allowed once authenticated",
            desc.name
        );
        // `DRM_AUTH` exempts render clients, but the command is not
        // `DRM_RENDER_ALLOW`, so a render node still refuses it.
        assert_eq!(
            check_permission(MinorKind::Render, desc.flags, false, false),
            Err(DrmError::PermissionDenied),
            "{} is not render-allowed",
            desc.name
        );
    }
}

#[def_test]
fn master_commands_require_ownership_on_the_primary_node() {
    // Pinned from Linux's `drm_ioctl.c` table, not derived from the flags under
    // test: a command that silently lost its MASTER bit must fail here.
    let expected = [
        "SET_VERSION",
        "MODE_SETCRTC",
        "MODE_CURSOR",
        "MODE_CONNECTOR_SETPROPERTY",
        "MODE_PAGE_FLIP",
        "MODE_DIRTYFB",
        "MODE_OBJ_SETPROPERTY",
        "MODE_CURSOR2",
        "MODE_ATOMIC",
        "AUTH_MAGIC",
        "MODE_SETPLANE",
        "MODE_SETGAMMA",
        "SET_UNIQUE",
        "ATTACHMODE",
        "DETACHMODE",
        "BLOCK",
        "UNBLOCK",
        "ADD_DRAW",
        "RM_DRAW",
        "UPDATE_DRAW",
    ];
    for desc in IOCTLS {
        let expected_master = expected.contains(&desc.name);
        assert_eq!(
            desc.flags.contains(IoctlFlags::MASTER),
            expected_master,
            "{} disagrees with the Linux master flag",
            desc.name
        );
        // Master answers `MASTER` on its own; every other command is checked
        // as an authenticated client, since `DRM_AUTH` commands need that.
        assert_eq!(
            check_permission(MinorKind::Primary, desc.flags, false, true).is_ok(),
            !expected_master,
            "{} disagrees with the master permission",
            desc.name
        );
        assert!(
            check_permission(MinorKind::Primary, desc.flags, true, true).is_ok(),
            "{} must be allowed to the master",
            desc.name
        );
    }
}

#[def_test]
fn the_render_allowlist_is_the_linux_one() {
    // The commands this driver implements that Linux marks DRM_RENDER_ALLOW.
    let expected = [
        "VERSION",
        "GET_CAP",
        "GEM_CLOSE",
        "PRIME_HANDLE_TO_FD",
        "PRIME_FD_TO_HANDLE",
        "SYNCOBJ_CREATE",
        "SYNCOBJ_DESTROY",
        "SYNCOBJ_SIGNAL",
        "SYNCOBJ_RESET",
        "SYNCOBJ_WAIT",
        "SYNCOBJ_TIMELINE_SIGNAL",
        "SYNCOBJ_TIMELINE_WAIT",
        "SYNCOBJ_HANDLE_TO_FD",
        "SYNCOBJ_FD_TO_HANDLE",
        "SYNCOBJ_TRANSFER",
        "SYNCOBJ_QUERY",
        "SYNCOBJ_EVENTFD",
        "SET_CLIENT_NAME",
        "GEM_CHANGE_HANDLE",
    ];
    let mut actual: alloc::vec::Vec<&str> = IOCTLS
        .iter()
        .filter(|desc| desc.flags.contains(IoctlFlags::RENDER_ALLOW))
        .map(|desc| desc.name)
        .collect();
    actual.sort_unstable();
    let mut expected = expected;
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

#[def_test]
fn capabilities_only_claim_what_is_implemented() {
    use crate::{model::backend::BackendCaps, protocol::capability};

    const DRM_CAP_PRIME: u64 = 0x5;
    let caps = BackendCaps {
        dumb: true,
        ..Default::default()
    };
    assert_eq!(capability(DRM_CAP_DUMB_BUFFER, caps), 1);
    // Xorg's modesetting driver only enables ShadowFB (and therefore DIRTYFB)
    // when this is 1; Linux virtio-gpu reports the same value.
    assert_eq!(capability(DRM_CAP_DUMB_PREFER_SHADOW, caps), 1);
    // Import and export are one dma-buf implementation behind
    // `PRIME_HANDLE_TO_FD` / `PRIME_FD_TO_HANDLE`.
    assert_eq!(capability(DRM_CAP_PRIME, caps), 3);
    // The whole `SYNCOBJ_*` family is in the ioctl table.
    assert_eq!(capability(DRM_CAP_SYNCOBJ, caps), 1);
    assert_eq!(capability(DRM_CAP_SYNCOBJ_TIMELINE, caps), 1);
    // Linux answers 1 unconditionally: the event always carries the CRTC id,
    // whether the timeline is hardware- or timer-driven.
    assert_eq!(capability(DRM_CAP_CRTC_IN_VBLANK_EVENT, caps), 1);
    // An unknown capability is refused, not answered with 0.
    assert_eq!(capability(0xffff, caps), u64::MAX);
}

#[def_test]
fn mode_blob_round_trips() {
    let mode = DrmModeModeInfo {
        clock: 65_000,
        hdisplay: 1280,
        hsync_start: 1328,
        hsync_end: 1360,
        htotal: 1400,
        hskew: 0,
        vdisplay: 800,
        vsync_start: 803,
        vsync_end: 806,
        vtotal: 823,
        vscan: 0,
        vrefresh: 60,
        flags: 0,
        kind: 0,
        name: {
            let mut name = [0u8; DRM_MODE_NAME_LEN];
            name[..4].copy_from_slice(b"test");
            name
        },
    };
    let bytes = mode_blob_bytes(&mode);
    assert_eq!(bytes.len(), core::mem::size_of::<DrmModeModeInfo>());
    let decoded = mode_from_bytes(&bytes).expect("encoded mode must decode");
    assert_eq!(decoded.clock, mode.clock);
    assert_eq!(decoded.hdisplay, mode.hdisplay);
    assert_eq!(decoded.vtotal, mode.vtotal);
    assert_eq!(decoded.vrefresh, mode.vrefresh);
    assert_eq!(decoded.name, mode.name);
    assert!(mode_from_bytes(&bytes[..bytes.len() - 1]).is_none());
}

#[def_test]
fn atomic_rejects_contradictory_test_only_flags() {
    use crate::abi::{
        DRM_MODE_ATOMIC_ALLOW_MODESET, DRM_MODE_ATOMIC_TEST_ONLY, DRM_MODE_PAGE_FLIP_EVENT,
    };

    assert!(!atomic_flags_valid(
        DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_PAGE_FLIP_EVENT,
        0
    ));
    assert!(atomic_flags_valid(
        DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_ATOMIC_ALLOW_MODESET,
        0
    ));
    assert!(!atomic_flags_valid(0, 1));
    assert!(!atomic_flags_valid(0x8000, 0));
}

#[def_test]
fn setcrtc_with_mode_and_no_framebuffer_is_enoent() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let mut request = DrmModeSetCrtc(DrmModeCrtc {
        crtc_id: CRTC_ID,
        mode_valid: 1,
        ..Default::default()
    });

    // This is a mode request with a missing framebuffer, not a disable.
    assert_eq!(
        DrmModeSetCrtc::handle(&dev, &file, 0, &mut request),
        Err(DrmError::NotFound)
    );
}

// ===========================================================================
// Behaviour tests over a fake backend.
//
// The object model is reachable without a display device, so the invariants
// the review found broken (scanout retention, mastership, dirty-vs-present,
// blob ownership) are proven here instead of only on target.
// ===========================================================================

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::{
    gem::GemObject,
    kms::KmsState,
    model::{
        backend::{Backend, BackendCaps, Rect, ResourceId, ResourceSpec},
        device::DrmDevice,
    },
};

/// Backend that records what the driver asked it to do.
#[derive(Default)]
struct FakeBackend {
    /// Visible size packed as `width << 32 | height`; zero selects the default.
    resolution: AtomicU64,
    /// Whether the next `refresh_info` must report the stored size as changed.
    pending_change: AtomicBool,
    created: AtomicUsize,
    destroyed: AtomicUsize,
    presented: AtomicUsize,
    /// Last transfer rectangle reported by `present`, packed as
    /// `((width as u64) << 32) | height as u64`.
    last_rect: AtomicU64,
    /// Resource id the last `flush` targeted, or 0 when none ran.
    last_flush_resource: AtomicU32,
    /// Resource id the last `present` put on screen, or 0 when none ran.
    last_present_resource: AtomicU32,
    flushed: AtomicUsize,
    fail_next_present: AtomicBool,
    fail_next_create: AtomicBool,
}

impl FakeBackend {
    fn set_resolution(&self, width: u32, height: u32) {
        self.resolution.store(
            (u64::from(width) << 32) | u64::from(height),
            Ordering::Relaxed,
        );
        self.pending_change.store(true, Ordering::Relaxed);
    }

    fn destroyed(&self) -> usize {
        self.destroyed.load(Ordering::Relaxed)
    }

    fn presented(&self) -> usize {
        self.presented.load(Ordering::Relaxed)
    }

    fn flushed(&self) -> usize {
        self.flushed.load(Ordering::Relaxed)
    }
}

impl Backend for FakeBackend {
    fn is_available(&self) -> bool {
        true
    }

    fn refresh_info(&self) -> Option<DisplayInfo> {
        if !self.pending_change.swap(false, Ordering::Relaxed) {
            return None;
        }
        Some(self.info())
    }

    fn info(&self) -> DisplayInfo {
        let packed = self.resolution.load(Ordering::Relaxed);
        if packed == 0 {
            return DisplayInfo {
                width: 1280,
                height: 800,
            };
        }
        DisplayInfo {
            width: (packed >> 32) as u32,
            height: packed as u32,
        }
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps {
            dumb: true,
            ..Default::default()
        }
    }

    fn create_resource(
        &self,
        _id: ResourceId,
        _spec: ResourceSpec,
        _window: &display::ScanoutWindow<'_>,
    ) -> DrmResult<()> {
        self.created.fetch_add(1, Ordering::Relaxed);
        if self.fail_next_create.swap(false, Ordering::Relaxed) {
            return Err(DrmError::Io);
        }
        Ok(())
    }

    fn destroy_resource(&self, _id: ResourceId) -> DrmResult<()> {
        self.destroyed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn present(&self, id: ResourceId, rect: Rect, _host_rendered: bool) -> DrmResult<()> {
        self.last_present_resource.store(id.raw(), Ordering::Relaxed);
        if self
            .fail_next_present
            .swap(false, core::sync::atomic::Ordering::Relaxed)
        {
            return Err(DrmError::Io);
        }
        self.last_rect.store(
            (u64::from(rect.width) << 32) | u64::from(rect.height),
            Ordering::Relaxed,
        );
        self.presented.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn flush(&self, id: ResourceId, _rect: Rect, _host_rendered: bool) -> DrmResult<()> {
        self.last_flush_resource.store(id.raw(), Ordering::Relaxed);
        self.flushed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn fake_device() -> (Arc<DrmDevice>, Arc<FakeBackend>) {
    let backend = Arc::new(FakeBackend::default());
    let device = DrmDevice::new_with_backend(backend.clone());
    (device, backend)
}

fn bind(dev: &Arc<DrmDevice>, fb: crate::prelude::FbId) -> DrmResult<()> {
    dev.commit(KmsState {
        crtc_active: 1,
        conn_crtc_id: CrtcId::from_raw(CONNECTOR_ID),
        plane_fb_id: fb,
        plane_crtc_id: CrtcId::from_raw(CRTC_ID),
        ..Default::default()
    })
}

#[def_test]
fn removed_framebuffer_stays_alive_while_it_is_scanned() {
    let (dev, backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev
        .add_framebuffer(&file, object.clone(), object_spec(&object), 0)
        .expect("fb");
    bind(&dev, fb).expect("present");
    assert_eq!(backend.presented(), 1);

    // The test drops its own reference, so only the scanout slot keeps the
    // resource alive from here on.
    drop(object);
    dev.remove_framebuffer(&file, fb).expect("rmfb");
    assert!(!dev.is_on_scanout(fb), "the plane must be unbound");
    assert_eq!(
        backend.destroyed(),
        0,
        "the host may still scan the removed framebuffer's resource"
    );

    // The next successful present is the moment the host stops scanning it.
    let next = GemObject::create(&dev, 64, 64).expect("dumb");
    let next_fb = dev
        .add_framebuffer(&file, next.clone(), object_spec(&next), 0)
        .expect("fb");
    bind(&dev, next_fb).expect("present");
    assert_eq!(
        backend.destroyed(),
        1,
        "the retained resource is retired by the next present"
    );
}

#[def_test]
fn dropping_master_revokes_master_only_commands() {
    let (dev, _backend) = fake_device();
    let first = dev.open_file(MinorKind::Primary).expect("open");
    assert!(dev.is_master(&first), "the first open owns the card");
    assert!(check_permission(first.kind, IoctlFlags::MASTER, dev.is_master(&first), false).is_ok());

    dev.drop_master(&first).expect("drop master");
    assert!(!dev.is_master(&first), "DROP_MASTER must end ownership");
    assert_eq!(
        check_permission(first.kind, IoctlFlags::MASTER, dev.is_master(&first), false),
        Err(DrmError::PermissionDenied),
        "a former master must not issue master-only commands"
    );

    // A second file that acquires the card is the only master; the first one
    // cannot take it back while the second holds it.
    let second = dev.open_file(MinorKind::Primary).expect("open");
    dev.set_master(&second).expect("second acquires");
    assert!(dev.is_master(&second));
    assert!(!dev.is_master(&first));
    assert_eq!(dev.set_master(&first), Err(DrmError::Busy));
}

#[def_test]
fn dirty_flush_never_rebinds_the_scanout() {
    let (dev, backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev
        .add_framebuffer(&file, object.clone(), object_spec(&object), 0)
        .expect("fb");
    bind(&dev, fb).expect("present");
    let presented = backend.presented();

    dev.flush(FbId::from_raw(0)).expect("flush the bound fb");
    assert_eq!(backend.flushed(), 1);
    assert_eq!(backend.presented(), presented, "a flush must not present");
    assert_eq!(dev.kms_state().plane_fb_id, fb);
    assert!(dev.is_on_scanout(fb));
}

#[def_test]
fn legacy_modeset_retires_its_previous_mode_blob() {
    let (dev, _backend) = fake_device();
    // Two modesets' worth of blobs, as SETCRTC would create them.
    let (first, first_blob) = dev
        .create_blob(None, Arc::new(alloc::vec![0u8; 68]), false)
        .expect("blob");
    dev.replace_legacy_mode_blob(first, first_blob.clone());
    assert!(dev.blob_bytes(first).is_some());

    let (second, second_blob) = dev
        .create_blob(None, Arc::new(alloc::vec![0u8; 68]), false)
        .expect("blob");
    dev.replace_legacy_mode_blob(second, second_blob.clone());
    assert!(
        dev.blob_bytes(first).is_none(),
        "the previous legacy mode blob must be freed"
    );
    assert!(dev.blob_bytes(second).is_some());

    dev.clear_legacy_mode_blob();
    assert!(
        dev.blob_bytes(second).is_none(),
        "disabling the CRTC frees the last legacy mode blob"
    );
}

#[def_test]
fn client_blobs_are_not_freed_by_the_legacy_slot() {
    let (dev, _backend) = fake_device();
    let (client, _client_blob) = dev
        .create_blob(None, Arc::new(alloc::vec![1u8; 4]), false)
        .expect("blob");
    let (legacy, legacy_blob) = dev
        .create_blob(None, Arc::new(alloc::vec![0u8; 68]), false)
        .expect("blob");
    dev.replace_legacy_mode_blob(legacy, legacy_blob.clone());
    assert!(dev.blob_bytes(client).is_some(), "client blob stays owned");
    dev.destroy_blob(client).expect("destroy");
    assert!(dev.blob_bytes(client).is_none());
}

#[def_test]
fn failed_present_keeps_both_resources_alive_until_the_next_present() {
    let (dev, backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");

    // First target presented, then removed from the registry: only the scanout
    // slot keeps its resource alive.
    let first = GemObject::create(&dev, 64, 64).expect("dumb");
    let first_fb = dev
        .add_framebuffer(&file, first.clone(), object_spec(&first), 0)
        .expect("fb");
    bind(&dev, first_fb).expect("present");
    dev.remove_framebuffer(&file, first_fb).expect("rmfb");
    assert_eq!(backend.destroyed(), 0);

    // A failed present means the host may have switched anyway: the attempted
    // resource must stay alive as well.
    let second = GemObject::create(&dev, 64, 64).expect("dumb");
    let second_fb = dev
        .add_framebuffer(&file, second.clone(), window_spec(&second), 0)
        .expect("fb");
    backend
        .fail_next_present
        .store(true, core::sync::atomic::Ordering::Relaxed);
    assert!(bind(&dev, second_fb).is_err());
    dev.remove_framebuffer(&file, second_fb).expect("rmfb");
    assert_eq!(
        backend.destroyed(),
        0,
        "neither the previous nor the attempted resource may be freed"
    );

    // The next successful present settles both.
    let third = GemObject::create(&dev, 64, 64).expect("dumb");
    let third_fb = dev
        .add_framebuffer(&file, third.clone(), object_spec(&third), 0)
        .expect("fb");
    // The third present retired the first target's object resource; the
    // failed window resource is still uncertain until a present succeeds for
    // the framebuffer that owns it.
    bind(&dev, third_fb).expect("present");
    assert_eq!(
        backend.destroyed(),
        1,
        "a failed present's resource stays until its own framebuffer presents"
    );
    // Presenting the window framebuffer again settles the uncertain resource:
    // the host now scans the very resource the failed attempt left behind,
    // then a successful present releases the previous one.
    dev.remove_framebuffer(&file, third_fb).expect("rmfb");
    let fourth_fb = dev
        .add_framebuffer(&file, second.clone(), window_spec(&second), 0)
        .expect("fb");
    // The failed attempt's window resource stays uncertain until a present
    // succeeds: the host may have switched to it, so it can only be released
    // once another present is known to have taken over the scanout.
    bind(&dev, fourth_fb).expect("present");
    let after_success = backend.destroyed();
    // It is released by the *next* successful present, which is the point at
    // which the host is known to have moved on.
    let fifth = GemObject::create(&dev, 64, 64).expect("dumb");
    let fifth_fb = dev
        .add_framebuffer(&file, fifth.clone(), object_spec(&fifth), 0)
        .expect("fb");
    bind(&dev, fifth_fb).expect("present");
    assert_eq!(
        backend.destroyed(),
        after_success + 1,
        "the uncertain window resource is settled by a later present"
    );
}

/// A dma-buf another exporter produced, as this crate's importer sees it.
///
/// The test's exporter is a separate type from `DrmBuf`, which is the point:
/// the importer reaches it only through the kernel-wide contract, the way a
/// second display engine in another driver would.
struct ForeignExport {
    start: usize,
    len: usize,
    /// Attachments taken and not yet detached, so the test can check pairing.
    attachments: core::sync::atomic::AtomicUsize,
    poll: kpoll::PollSet,
}

impl display::ExportedDmaBuf for ForeignExport {
    fn identity(&self) -> usize {
        self as *const Self as usize
    }

    fn exporter(&self) -> &str {
        "foreign-exporter"
    }

    fn size(&self) -> u64 {
        self.len as u64
    }

    fn attach(&self, _importer: u64) -> Result<display::DmaBufAttachment, display::DriverError> {
        self.attachments
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Ok(display::DmaBufAttachment {
            export: self.identity(),
        })
    }

    fn detach(&self, _attachment: &display::DmaBufAttachment) {
        self.attachments
            .fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    }

    fn map(
        &self,
        _attachment: &display::DmaBufAttachment,
    ) -> Result<display::MappedMemory, display::DriverError> {
        Ok(display::MappedMemory {
            start: self.start as u64,
            len: self.len as u64,
        })
    }

    fn fences(&self, _write: bool) -> Vec<Arc<dyn core::any::Any + Send + Sync>> {
        Vec::new()
    }

    fn poll(&self) -> &kpoll::PollSet {
        &self.poll
    }

    fn fence_or_signaled(&self) -> Arc<dyn core::any::Any + Send + Sync> {
        crate::sync::dma_fence::new_signaled_fence() as Arc<dyn core::any::Any + Send + Sync>
    }
}

impl display::DmaBufObject for ForeignExport {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[def_test(serial)]
fn a_foreign_dmabuf_is_imported_through_the_kernel_contract() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    // Memory this card never allocated: a window the host exporter reports.
    let pages = alloc_zeroed_pages_exact_ref(1, PAGE_SIZE_4K, UsageKind::Global).expect("pages");
    let export = Arc::new(ForeignExport {
        start: pages.phys_addr().as_usize(),
        len: PAGE_SIZE_4K,
        attachments: core::sync::atomic::AtomicUsize::new(0),
        poll: kpoll::PollSet::new(),
    });

    // The importer sees only the exported object, as a second driver would.
    let as_object: Arc<dyn display::DmaBufObject> = export.clone();
    let attachment = as_object.attach(dev.card_id()).expect("attach");
    let window = as_object.map(&attachment).expect("map");
    assert_eq!(window.len as usize, PAGE_SIZE_4K);

    // The import never attached: it is handed an attachment the importer
    // already took, exactly like `drm_gem_prime_import_dev` after
    // `dma_buf_attach`.
    let object = GemObject::import_window(
        &dev,
        window,
        crate::model::backend::ResourceSpec {
            width: (PAGE_SIZE_4K / 4) as u32,
            height: 1,
            pitch: PAGE_SIZE_4K as u32,
            format: ScanoutFormat::Bgra8888,
        },
        crate::sync::dma_resv::DmaResv::new(),
        crate::gem::ForeignAttachment::new(&as_object, attachment),
    )
    .expect("import");

    // The import reads the window, not pages of its own.
    assert_eq!(object.memory_range().0, export.start as u64);
    assert!(object.pages().is_none(), "a foreign import owns no pages");
    let handle = object.import_handle();
    file.handles.lock().insert(handle, object.clone());
    assert_eq!(file.handles.lock().len(), 1);

    // Dropping the object detaches from the exporter exactly once.
    assert_eq!(
        export
            .attachments
            .load(core::sync::atomic::Ordering::Relaxed),
        1
    );
    file.handles.lock().clear();
    drop(object);
    assert_eq!(
        export
            .attachments
            .load(core::sync::atomic::Ordering::Relaxed),
        0,
        "the object's lifetime is the attachment's lifetime"
    );
}

#[def_test]
fn flink_names_an_object_once_and_open_reaches_it_from_another_file() {
    let (dev, _backend) = fake_device();
    let first = dev.open_file(MinorKind::Primary).expect("open");
    let second = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 32, 16).expect("dumb");
    let handle = object.handle();
    first.handles.lock().insert(handle, object.clone());

    let mut flink = DrmGemFlink {
        handle: handle.raw(),
        ..Default::default()
    };
    DrmGemFlink::handle(&dev, &first, 0, &mut flink).expect("flink");
    assert_ne!(flink.name, 0, "an object gets a global name");
    let mut again = DrmGemFlink {
        handle: handle.raw(),
        ..Default::default()
    };
    DrmGemFlink::handle(&dev, &first, 0, &mut again).expect("flink");
    assert_eq!(again.name, flink.name, "flink is idempotent per object");

    // Another file reaches the same object through the name.
    let mut open = DrmGemOpen {
        name: flink.name,
        ..Default::default()
    };
    DrmGemOpen::handle(&dev, &second, 0, &mut open).expect("open");
    assert_eq!(open.size, object.size_bytes());
    let opened = second
        .handles
        .lock()
        .get(&DumbHandle::from_raw(open.handle))
        .cloned()
        .expect("handle in the second file");
    assert!(Arc::ptr_eq(&opened, &object), "one object, two handles");

    // A name holds no reference: with every handle gone it stops resolving.
    first.handles.lock().clear();
    second.handles.lock().clear();
    drop(opened);
    drop(object);
    let mut stale = DrmGemOpen {
        name: flink.name,
        ..Default::default()
    };
    assert_eq!(
        DrmGemOpen::handle(&dev, &second, 0, &mut stale),
        Err(DrmError::NotFound),
        "a dead object's name does not resolve"
    );
}

/// A PRIME export accepts `DRM_CLOEXEC | DRM_RDWR` and nothing else.
///
/// `drm_prime_handle_to_fd_ioctl` admits exactly those two flags, and
/// `DRM_RDWR` is `O_RDWR`. Mesa's `gbm_bo_get_fd_for_plane` exports a buffer it
/// renders into with both set, so rejecting `DRM_RDWR` failed every swapchain
/// allocation with `EINVAL` and the compositor fell back to no buffer at all.
#[def_test]
fn a_prime_export_accepts_the_read_write_flag() {
    const DRM_CLOEXEC: u32 = 0o2000000;
    const DRM_RDWR: u32 = 2;

    // The word Mesa passes: close-on-exec plus read/write.
    assert!(crate::gem::prime_export_flags_valid(
        DRM_CLOEXEC | DRM_RDWR
    ));
    assert!(crate::gem::prime_export_flags_valid(DRM_CLOEXEC));
    assert!(crate::gem::prime_export_flags_valid(DRM_RDWR));
    assert!(crate::gem::prime_export_flags_valid(0));
    // Any other bit is `EINVAL`, as the ioctl's own check reports it.
    assert!(!crate::gem::prime_export_flags_valid(0x4));
    assert!(!crate::gem::prime_export_flags_valid(DRM_CLOEXEC | 0x4));

    // The ABI constants match the header, so the admitted set is the real one:
    // `DRM_RDWR` is `O_RDWR`, which is 2 on every Linux target.
    assert_eq!(crate::abi::DRM_RDWR, DRM_RDWR, "DRM_RDWR is O_RDWR");
    assert_eq!(crate::abi::DRM_CLOEXEC, DRM_CLOEXEC);

    // A handle this file does not hold is `ENOENT`, the failed lookup, not the
    // flag check's `EINVAL`; the lookup runs before any descriptor is made, so
    // it is reachable without a current task.
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let mut unknown = DrmPrimeHandle {
        handle: 0xdead_beef,
        flags: DRM_CLOEXEC | DRM_RDWR,
        ..Default::default()
    };
    assert_eq!(
        DrmPrimeHandle::handle(&dev, &file, 0, &mut unknown),
        Err(DrmError::NotFound),
        "an unknown handle is ENOENT, not EINVAL"
    );
}

#[def_test]
fn the_mmap_offset_names_the_object_inside_the_card_namespace() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Render).expect("open");
    let object = GemObject::create(&dev, 8, 8).expect("dumb");
    let offset = object.offset_bytes();
    assert_eq!(
        offset % PAGE_SIZE_4K as u64,
        0,
        "an offset is page aligned, as drm_vma_offset_add requires"
    );

    // The namespace resolves the offset before any handle exists, but `mmap`
    // additionally requires the file to hold a handle for it.
    let range = dev.vma.lookup(offset).expect("the offset is registered");
    assert_eq!(range.offset, offset);
    assert!(file.object_by_offset(offset).is_none(), "no handle yet");

    file.handles.lock().insert(object.handle(), object.clone());
    let found = file
        .object_by_offset(offset)
        .expect("the handle makes the offset mappable");
    assert!(Arc::ptr_eq(&found, &object));

    // A mapping that starts inside the object resolves to it; a mapping that
    // would run past the object's end does not (`drm_vma_offset_exact_lookup`),
    // and neither does an offset outside the namespace.
    let big = GemObject::create(&dev, 64, 64).expect("dumb");
    file.handles.lock().insert(big.handle(), big.clone());
    assert!(
        file.object_by_offset(big.offset_bytes() + PAGE_SIZE_4K as u64)
            .is_some(),
        "a tail of the allocation is still the object"
    );
    assert!(
        file.object_by_offset(big.offset_bytes() + big.length_bytes() as u64)
            .is_none(),
        "past the allocation is nobody's offset"
    );
    assert!(
        file.object_by_offset_range(
            big.offset_bytes() + big.length_bytes() as u64 - PAGE_SIZE_4K as u64,
            PAGE_SIZE_4K as u64 * 2,
        )
        .is_none(),
        "a mapping may not run past the object"
    );
    assert!(
        file.object_by_offset_range(
            big.offset_bytes() + big.length_bytes() as u64 - PAGE_SIZE_4K as u64,
            PAGE_SIZE_4K as u64,
        )
        .is_some(),
        "the last page alone is inside the object"
    );
    file.handles.lock().remove(&big.handle());
    drop(big);
    assert!(
        file.object_by_offset(offset + (1 << 40)).is_none(),
        "an offset outside the namespace maps nothing"
    );

    // Releasing the object frees the binding, so a stale offset maps nothing.
    file.handles.lock().clear();
    drop(found);
    drop(object);
    assert!(dev.vma.lookup(offset).is_none());
    assert!(file.object_by_offset(offset).is_none());
}

#[def_test]
fn virtgpu_commands_need_a_device_that_negotiated_them() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Render).expect("open");

    // The fake backend has no 3D channel: the commands that need one refuse
    // instead of pretending, and they do so before touching any user memory.
    let mut context = crate::virtgpu::DrmVirtgpuContextInit {
        num_params: 0,
        abi_pad: 0,
        ctx_set_params: 0,
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut context),
        Err(DrmError::OpNotSupported)
    );
    let mut exec = crate::virtgpu::DrmVirtgpuExecbuffer {
        flags: 0,
        size: 4,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut exec),
        Err(DrmError::OpNotSupported)
    );
    let mut blob = crate::virtgpu::DrmVirtgpuResourceCreateBlob::default();
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut blob),
        Err(DrmError::OpNotSupported)
    );

    // `GETPARAM` and the other object commands do not need the channel; an
    // unknown parameter is EINVAL, as Linux answers it, whatever the device.
    let mut unknown = crate::virtgpu::DrmVirtgpuGetparam {
        param: 0xffff,
        value: 0,
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut unknown),
        Err(DrmError::InvalidInput)
    );
    let mut map = crate::virtgpu::DrmVirtgpuMap {
        offset: 0,
        handle: 0,
        abi_pad: 0,
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut map),
        Err(DrmError::InvalidInput),
        "an unknown handle maps nothing"
    );
}

#[def_test]
fn the_standard_property_set_is_present_and_consistent() {
    // Every property a client probes must exist once, and a property that
    // reports a writable value must have a writer that accepts it.
    let mut ids = alloc::vec::Vec::new();
    for prop in PROPS {
        assert!(!ids.contains(&prop.id), "{} has a duplicate id", prop.name);
        ids.push(prop.id);
        if prop.write.is_some() {
            assert!(
                prop.flags & DRM_MODE_PROP_IMMUTABLE == 0,
                "{} is immutable but writable",
                prop.name
            );
        } else {
            assert!(
                prop.flags & DRM_MODE_PROP_IMMUTABLE != 0,
                "{} has no writer and is not immutable",
                prop.name
            );
        }
    }
    for name in [
        "type",
        "SRC_X",
        "SRC_Y",
        "SRC_W",
        "SRC_H",
        "CRTC_X",
        "CRTC_Y",
        "CRTC_W",
        "CRTC_H",
        "FB_ID",
        "IN_FENCE_FD",
        "OUT_FENCE_PTR",
        "CRTC_ID",
        "FB_DAMAGE_CLIPS",
        "ACTIVE",
        "MODE_ID",
        "IN_FORMATS",
    ] {
        assert!(
            PROPS.iter().any(|prop| prop.name == name),
            "the standard property {name} must exist"
        );
    }
}

#[def_test]
fn a_connector_without_edid_reports_unknown_size_and_no_blob() {
    // The fake backend has no descriptor: the connector reports no physical
    // size (zero is "unknown", not an invented DPI) and no `EDID` blob.
    let (dev, _backend) = fake_device();
    assert!(dev.edid().is_none());
    assert!(dev.connector_edid_blob().is_none());
    assert!(dev.is_connected(), "the fake display always has a mode");
}

#[def_test]
fn atomic_requests_follow_linux_flag_and_capability_rules() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");

    // An atomic request without the capability is EINVAL (Linux), not
    // "unsupported".
    let mut request = DrmModeAtomic {
        flags: 0,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut request),
        Err(DrmError::InvalidInput),
        "a client without DRM_CLIENT_CAP_ATOMIC gets EINVAL"
    );
    file.grant_cap(crate::model::file::CLIENT_CAP_ATOMIC);

    // A flag word carrying `DRM_MODE_PAGE_FLIP_ASYNC` is refused: this card
    // does not flip asynchronously.
    let mut async_flip = DrmModeAtomic {
        flags: DRM_MODE_PAGE_FLIP_ASYNC,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut async_flip),
        Err(DrmError::InvalidInput),
        "async flips are not supported"
    );

    // Test-only plus an event is contradictory in Linux too.
    let mut contradictory = DrmModeAtomic {
        flags: DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_PAGE_FLIP_EVENT,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut contradictory),
        Err(DrmError::InvalidInput)
    );
}

#[def_test]
fn atomic_completion_follows_the_nonblock_flag() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    file.grant_cap(crate::model::file::CLIENT_CAP_ATOMIC);

    // Without `NONBLOCK` the ioctl waits for the flip's completion point
    // before returning; an inactive CRTC has no blank to wait for, so the
    // commit itself completes the flip and the wait returns at once.
    let mut blocking = DrmModeAtomic {
        flags: 0,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut blocking),
        Ok(0),
        "a blocking atomic commit completes before returning"
    );

    // `NONBLOCK` returns once the flip is queued instead, without changing
    // the completion point the client reads through the event/out-fence.
    let mut nonblocking = DrmModeAtomic {
        flags: DRM_MODE_ATOMIC_NONBLOCK,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut nonblocking),
        Ok(0),
        "a nonblocking atomic commit is accepted"
    );

    // Accepting NONBLOCK must not make the card claim async-flip support.
    let mut async_flip = DrmModeAtomic {
        flags: DRM_MODE_PAGE_FLIP_ASYNC,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut async_flip),
        Err(DrmError::InvalidInput),
        "async flips stay refused"
    );
}

#[def_test]
fn page_flip_refuses_flags_linux_refuses() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 48).expect("dumb");
    let fb = dev
        .add_framebuffer(&file, object.clone(), object_spec(&object), 0)
        .expect("fb");

    // A target flip needs `PAGE_FLIP_TARGET`; this card has no
    // `page_flip_target`, so Linux refuses the flag outright.
    let mut target = DrmModeCrtcPageFlip {
        crtc_id: CRTC_ID,
        fb_id: fb.raw(),
        flags: DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut target),
        Err(DrmError::InvalidInput)
    );

    // The legacy structure's fourth word is reserved and must be zero.
    let mut reserved = DrmModeCrtcPageFlip {
        crtc_id: CRTC_ID,
        fb_id: fb.raw(),
        flags: 0,
        abi_pad: 5,
        ..Default::default()
    };
    assert_eq!(
        Ioctl::handle(&dev, &file, 0, &mut reserved),
        Err(DrmError::InvalidInput)
    );
}

#[def_test]
fn closefb_refuses_a_framebuffer_that_is_still_bound() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev
        .add_framebuffer(&file, object.clone(), object_spec(&object), 0)
        .expect("fb");
    bind(&dev, fb).expect("present");

    assert_eq!(
        dev.close_framebuffer(&file, fb),
        Err(DrmError::Busy),
        "an on-screen framebuffer must not be closable"
    );
    dev.remove_framebuffer(&file, fb).expect("rmfb");
    assert_eq!(dev.close_framebuffer(&file, fb), Err(DrmError::NotFound));
}

#[def_test]
fn another_file_cannot_remove_a_framebuffer() {
    let (dev, _backend) = fake_device();
    let owner = dev.open_file(MinorKind::Primary).expect("open");
    let other = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev
        .add_framebuffer(&owner, object.clone(), object_spec(&object), 0)
        .expect("fb");

    // Linux looks the id up in `file_priv->fbs`, so a foreign id is
    // indistinguishable from an unknown one.
    assert_eq!(dev.remove_framebuffer(&other, fb), Err(DrmError::NotFound));
    assert_eq!(dev.close_framebuffer(&other, fb), Err(DrmError::NotFound));
    assert!(dev.has_framebuffer(fb), "a foreign file must not remove it");

    dev.remove_framebuffer(&owner, fb).expect("owner rmfb");
}

#[def_test]
fn destroying_the_legacy_blob_succeeds_and_clears_the_slot() {
    let (dev, _backend) = fake_device();
    let (id, id_blob) = dev
        .create_blob(None, Arc::new(alloc::vec![0u8; 68]), false)
        .expect("blob");
    dev.replace_legacy_mode_blob(id, id_blob);
    assert!(dev.blob_bytes(id).is_some());
    // The ioctl path destroys it exactly once and reports success, and the
    // legacy slot no longer names it (a later modeset must not free it twice).
    dev.destroy_client_blob(None, id)
        .expect("destroy legacy blob");
    assert!(dev.blob_bytes(id).is_none());
    assert_eq!(
        dev.legacy_mode_blob(),
        None,
        "destroying the blob must clear the device's legacy slot"
    );
    // A second destroy is ENOENT, and the slot no longer names it.
    assert_eq!(dev.destroy_client_blob(None, id), Err(DrmError::NotFound));
}

#[def_test]
fn destroy_client_blob_removes_the_creating_file_registration() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let (id, _blob) = dev
        .create_blob(Some(&file), Arc::new(alloc::vec![0u8; 68]), false)
        .expect("blob");
    assert!(
        file.blobs.lock().contains_key(&id),
        "CREATE_BLOB records the blob on the creating file"
    );

    // Registry removal and the file's bookkeeping are one operation: after it
    // returns, neither may still name the id.
    dev.destroy_client_blob(Some(&file), id)
        .expect("destroy client blob");
    assert!(dev.blob_bytes(id).is_none());
    assert!(
        !file.blobs.lock().contains_key(&id),
        "DESTROY_BLOB must remove the per-file registration in the same section"
    );
}

#[def_test]
fn destroy_client_blob_requires_ownership_and_refuses_system_blobs() {
    let (dev, _backend) = fake_device();
    let owner = dev.open_file(MinorKind::Primary).expect("open");
    let stranger = dev.open_file(MinorKind::Primary).expect("open");
    let (id, _blob) = dev
        .create_blob(Some(&owner), Arc::new(alloc::vec![0u8; 68]), false)
        .expect("blob");

    // Another file on the same card cannot destroy an id it does not own, and
    // the refusal must leave both the registry and the owner's set intact.
    assert_eq!(
        dev.destroy_client_blob(Some(&stranger), id),
        Err(DrmError::NotPermitted),
        "DESTROY_BLOB must fail with EPERM for a non-owner"
    );
    assert!(dev.blob_bytes(id).is_some());
    assert!(
        owner.blobs.lock().contains_key(&id),
        "the refused destroy must not disturb the owner's registration"
    );

    dev.destroy_client_blob(Some(&owner), id)
        .expect("the owner may destroy its blob");

    // Driver-owned blobs are not in any file's set, so a client naming one gets
    // the same EPERM and the system blob stays registered.
    let system = dev.ensure_in_formats_blob().expect("in-formats blob");
    assert_eq!(
        dev.destroy_client_blob(Some(&owner), system),
        Err(DrmError::NotPermitted),
        "DESTROY_BLOB must refuse a driver-owned blob"
    );
    assert!(dev.blob_bytes(system).is_some());
}

#[def_test]
fn release_destroys_client_blobs_and_clears_the_legacy_slot() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let (id, blob) = dev
        .create_blob(Some(&file), Arc::new(alloc::vec![0u8; 68]), false)
        .expect("blob");
    // A legacy modeset can name any client blob; release must clear that slot
    // as well as the device registry and the file set.
    dev.replace_legacy_mode_blob(id, blob.clone());

    dev.destroy_file_blobs(&file);

    assert!(dev.blob_bytes(id).is_none());
    assert_eq!(dev.legacy_mode_blob(), None);
    assert!(
        file.blobs.lock().is_empty(),
        "release must empty the file's blob set, not just the device registry"
    );
    assert_eq!(
        Arc::strong_count(&blob),
        1,
        "every release-owned reference must be dropped"
    );
}

#[def_test]
fn page_flip_on_a_disabled_crtc_is_rejected_without_binding() {
    let (dev, backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev
        .add_framebuffer(&file, object.clone(), object_spec(&object), 0)
        .expect("fb");
    let mut request = DrmModeCrtcPageFlip {
        crtc_id: CRTC_ID,
        fb_id: fb.raw(),
        ..Default::default()
    };

    assert_eq!(
        DrmModeCrtcPageFlip::handle(&dev, &file, 0, &mut request),
        Err(DrmError::InvalidInput),
        "the ioctl must reject a flip on a disabled CRTC"
    );
    let state = dev.kms_state();
    assert_eq!(state.plane_fb_id, FbId::default());
    assert_eq!(state.plane_crtc_id, CrtcId::default());
    assert_eq!(state.crtc_active, 0, "a flip must not enable the CRTC");
    assert_eq!(
        backend.presented(),
        0,
        "nothing is presented on a dead pipe"
    );
}

#[def_test]
fn plane_coordinate_metadata_matches_the_signed_writer() {
    let (dev, _backend) = fake_device();
    for (id, name) in [(PROP_PLANE_CRTC_X, "CRTC_X"), (PROP_PLANE_CRTC_Y, "CRTC_Y")] {
        let desc = PROPS
            .iter()
            .find(|desc| desc.id == PropertyId::from_raw(id))
            .expect("coordinate property must exist");
        assert_eq!(desc.name, name);
        assert_eq!(
            desc.flags & DRM_MODE_PROP_SIGNED_RANGE,
            DRM_MODE_PROP_SIGNED_RANGE,
            "{name} must advertise that its range is signed"
        );
        let PropKind::Range { min, max } = &desc.kind else {
            panic!("{name} must be a range");
        };
        assert_eq!(*min, i32::MIN as i64 as u64);
        assert_eq!(*max, i32::MAX as i64 as u64);

        let write = desc.write.expect("coordinate property must be writable");
        let mut state = KmsState::default();
        write(&mut state, *min, &dev).expect("advertised minimum must be accepted");
        write(&mut state, *max, &dev).expect("advertised maximum must be accepted");
        assert_eq!(
            write(&mut state, 0x8000_0000, &dev),
            Err(DrmError::InvalidInput)
        );
        assert_eq!(
            write(&mut state, u32::MAX as u64, &dev),
            Err(DrmError::InvalidInput)
        );
    }
}

#[def_test]
fn immutable_property_write_is_einval_not_enoent() {
    let (dev, _backend) = fake_device();
    let mut state = KmsState::default();

    for prop in [PROP_PLANE_TYPE, PROP_PLANE_IN_FORMATS] {
        assert_eq!(
            apply(
                &mut state,
                ObjectType::Plane,
                PropertyId::from_raw(prop),
                0,
                &dev,
            ),
            Err(DrmError::InvalidInput),
            "an attached immutable property is a rejected write, not a missing one"
        );
    }
    assert_eq!(
        apply(
            &mut state,
            ObjectType::Crtc,
            PropertyId::from_raw(PROP_PLANE_TYPE),
            0,
            &dev,
        ),
        Ok(false),
        "a property of another object class remains an ENOENT lookup failure"
    );
}

#[def_test]
fn object_properties_accept_the_wildcard_type() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");

    // libdrm's drmModeObjectGetProperties() passes DRM_MODE_OBJECT_ANY (zero)
    // and leaves the type to the id. wlroots reads connector properties this
    // way while probing a DRM backend.
    let mut request = DrmModeObjGetProperties {
        obj_id: CONNECTOR_ID,
        obj_type: 0,
        ..Default::default()
    };
    assert_eq!(
        DrmModeObjGetProperties::handle(&dev, &file, 0, &mut request),
        Ok(0)
    );
    assert!(
        request.count_props > 0,
        "the connector property list must be reported"
    );

    // A non-zero type still has to agree with the object the id names.
    let mut wrong = DrmModeObjGetProperties {
        obj_id: CONNECTOR_ID,
        obj_type: DRM_MODE_OBJECT_PLANE,
        ..Default::default()
    };
    assert_eq!(
        DrmModeObjGetProperties::handle(&dev, &file, 0, &mut wrong),
        Err(DrmError::NotFound)
    );

    let mut unknown = DrmModeObjGetProperties {
        obj_id: 0,
        obj_type: 0,
        ..Default::default()
    };
    assert_eq!(
        DrmModeObjGetProperties::handle(&dev, &file, 0, &mut unknown),
        Err(DrmError::NotFound),
        "the wildcard type must not invent an object for an unknown id"
    );
}

#[def_test]
fn get_blob_short_buffer_is_a_size_probe() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let bytes = Arc::new(alloc::vec![1u8, 2, 3, 4]);
    let (id, _blob) = dev
        .create_blob(Some(&file), bytes.clone(), false)
        .expect("blob");
    let mut request = DrmModeGetBlob {
        blob_id: id.raw(),
        length: 2,
        data: UserPtr::default(),
    };

    let result = <DrmModeGetBlob as Ioctl>::handle(&dev, &file, 0, &mut request);
    assert_eq!(
        result,
        Ok(0),
        "Linux succeeds a short size probe without copying"
    );
    assert_eq!(request.length, 4, "the caller must learn the required size");
    let stored = dev.blob_bytes(id).expect("blob stays registered");
    assert_eq!(stored.as_slice(), bytes.as_slice());
}

#[def_test]
fn flip_event_reservations_respect_the_queue_bound() {
    let (dev, _backend) = fake_device();
    let mut held = alloc::vec::Vec::new();
    for _ in 0..crate::abi::MAX_EVENTS {
        held.push(dev.reserve_flip_event().expect("a free event slot"));
    }
    assert!(
        dev.reserve_flip_event().is_none(),
        "a full queue must refuse another reservation"
    );
    held.clear();
    assert!(
        dev.reserve_flip_event().is_some(),
        "dropping uncommitted reservations must release their slots"
    );
}

#[def_test]
fn oversized_atomic_object_count_is_refused_before_loading() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    file.grant_cap(crate::model::file::CLIENT_CAP_ATOMIC);
    let mut request = DrmModeAtomic {
        // The driver names far fewer objects than this, and the pointers are
        // null: the count check must fire before any user memory is touched.
        count_objs: 2048,
        ..Default::default()
    };
    assert_eq!(
        <DrmModeAtomic as Ioctl>::handle(&dev, &file, 0, &mut request),
        Err(DrmError::NoMemory)
    );
}

#[def_test]
fn connector_without_physical_dimensions_reports_unknown_size() {
    for (width, height) in [(640, 480), (1280, 800), (3840, 2160)] {
        let backend = Arc::new(FakeBackend::default());
        backend.set_resolution(width, height);
        let dev = DrmDevice::new_with_backend(backend);
        let file = dev.open_file(MinorKind::Primary).expect("open");
        let mut connector = DrmModeGetConnector {
            connector_id: CONNECTOR_ID,
            mm_width: u32::MAX,
            mm_height: u32::MAX,
            ..Default::default()
        };
        DrmModeGetConnector::handle(&dev, &file, 0, &mut connector).expect("connector");
        assert_eq!((connector.mm_width, connector.mm_height), (0, 0));
        assert_eq!(connector.count_modes, 1);
        assert_eq!(connector.connection, DRM_MODE_CONNECTED);

        let mut resources = DrmModeCardRes::default();
        DrmModeCardRes::handle(&dev, &file, 0, &mut resources).expect("resources");
        // The reported range is the capability envelope, not this mode's size:
        // a client pins its screen bounds to it at startup, so it has to stay
        // wider than any size the host may resize to later.
        assert_eq!(
            (resources.min_width, resources.min_height),
            (MIN_SCANOUT_WIDTH, MIN_SCANOUT_HEIGHT)
        );
        assert_eq!(
            (resources.max_width, resources.max_height),
            (MAX_SCANOUT_WIDTH, MAX_SCANOUT_HEIGHT)
        );
    }
}

#[def_test]
fn connector_mode_tracks_a_host_resize() {
    let backend = Arc::new(FakeBackend::default());
    let dev = DrmDevice::new_with_backend(backend.clone());

    let mode = current_mode(&dev).expect("mode for initial geometry");
    assert_eq!((mode.hdisplay, mode.vdisplay), (1280, 800));
    assert_eq!(mode_name(&mode), "1280x800");

    // A resized host window must change both the pixels and the mode name, so
    // a client that re-reads the mode list can tell the new mode apart.
    backend.set_resolution(1024, 768);
    let mode = current_mode(&dev).expect("mode for resized geometry");
    assert_eq!((mode.hdisplay, mode.vdisplay), (1024, 768));
    assert_eq!(mode_name(&mode), "1024x768");
    let total_pixels = u64::from(mode.htotal) * u64::from(mode.vtotal);
    let refresh_hz = (u64::from(mode.clock) * 1000 + total_pixels / 2) / total_pixels;
    assert_eq!(refresh_hz, u64::from(mode.vrefresh));

    // The query envelope must not shrink to the new mode, otherwise a client
    // that grew the window could never ask for the larger size again.
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let mut resources = DrmModeCardRes::default();
    DrmModeCardRes::handle(&dev, &file, 0, &mut resources).expect("resources");
    assert_eq!((resources.max_width, resources.max_height), (8192, 8192));
}

#[def_test]
fn virtual_mode_checks_timing_fields_and_uses_wide_clock_arithmetic() {
    let (dev, backend) = fake_device();
    backend.set_resolution(16_384, 16_384);
    let mode = current_mode(&dev).expect("geometry fits the timing fields");
    assert_eq!((mode.htotal, mode.vtotal), (16_544, 16_414));
    assert_eq!(mode.clock, 16_293_192);

    backend.set_resolution(u32::from(u16::MAX), 1024);
    assert!(
        current_mode(&dev).is_none(),
        "horizontal blanking must fit u16"
    );
}

#[def_test]
fn refresh_notifies_the_hotplug_sink_once() {
    let (dev, backend) = fake_device();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    dev.set_hotplug_sink(Arc::new(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    }));

    // A query with nothing to report must not publish a hotplug.
    assert_eq!(dev.refresh_info(), None);
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    backend.set_resolution(1024, 768);
    assert_eq!(dev.refresh_info().map(|info| info.width), Some(1024));
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // The change was acknowledged, so the next refresh notifies nobody.
    assert_eq!(dev.refresh_info(), None);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[def_test]
fn refresh_reports_a_pending_host_change_once() {
    let (dev, backend) = fake_device();
    assert_eq!(dev.refresh_info(), None);

    backend.set_resolution(800, 600);
    let changed = dev.refresh_info().expect("change reported");
    assert_eq!((changed.width, changed.height), (800, 600));
    assert_eq!(dev.refresh_info(), None);
    assert_eq!(dev.info().expect("info").width, 800);
}

fn mode_name(mode: &DrmModeModeInfo) -> &str {
    let end = mode
        .name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(mode.name.len());
    core::str::from_utf8(&mode.name[..end]).expect("mode name is ASCII")
}

#[def_test]
fn connector_invalid_id_does_not_publish_physical_dimensions() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    for connector_id in [0, u32::MAX] {
        let mut connector = DrmModeGetConnector {
            connector_id,
            mm_width: 123,
            mm_height: 456,
            ..Default::default()
        };
        assert_eq!(
            DrmModeGetConnector::handle(&dev, &file, 0, &mut connector),
            Err(DrmError::NotFound)
        );
        assert_eq!((connector.mm_width, connector.mm_height), (123, 456));
    }
}

#[def_test]
fn getresources_framebuffer_count_is_per_open_file() {
    let (dev, _backend) = fake_device();
    let owner = dev.open_file(MinorKind::Primary).expect("owner");
    let other = dev.open_file(MinorKind::Primary).expect("other");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let first = dev
        .add_framebuffer(&owner, object.clone(), object_spec(&object), 0)
        .expect("fb");
    let second = dev
        .add_framebuffer(&owner, object.clone(), object_spec(&object), 0)
        .expect("fb");
    let foreign = dev
        .add_framebuffer(&other, object.clone(), object_spec(&object), 0)
        .expect("foreign");

    let mut request = DrmModeCardRes::default();
    DrmModeCardRes::handle(&dev, &owner, 0, &mut request).expect("owner query");
    assert_eq!(request.count_fbs, 2);
    request = DrmModeCardRes::default();
    DrmModeCardRes::handle(&dev, &other, 0, &mut request).expect("other query");
    assert_eq!(request.count_fbs, 1);

    dev.remove_framebuffer(&owner, first).expect("rmfb");
    request = DrmModeCardRes::default();
    DrmModeCardRes::handle(&dev, &owner, 0, &mut request).expect("after remove");
    assert_eq!(request.count_fbs, 1);
    dev.remove_framebuffer(&owner, second).expect("rmfb");
    dev.remove_framebuffer(&other, foreign).expect("rmfb");
}

#[def_test]
fn getresources_count_query_does_not_access_the_id_buffer() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev
        .add_framebuffer(&file, object.clone(), object_spec(&object), 0)
        .expect("fb");
    let mut request = DrmModeCardRes {
        fb_id_ptr: UserPtr::from(1_usize),
        ..Default::default()
    };

    DrmModeCardRes::handle(&dev, &file, 0, &mut request).expect("length query");
    assert_eq!(request.count_fbs, 1);
    // Reset output counts so an unrelated null array cannot cause EFAULT.
    request = DrmModeCardRes {
        fb_id_ptr: UserPtr::default(),
        count_fbs: 1,
        ..Default::default()
    };
    assert_eq!(
        DrmModeCardRes::handle(&dev, &file, 0, &mut request),
        Err(DrmError::BadAddress),
        "a nonzero capacity must access the nonempty output"
    );
    dev.remove_framebuffer(&file, fb).expect("rmfb");
}

#[def_test]
fn getresources_empty_list_ignores_even_a_max_capacity_bad_pointer() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let mut request = DrmModeCardRes {
        fb_id_ptr: UserPtr::from(1_usize),
        count_fbs: u32::MAX,
        ..Default::default()
    };

    DrmModeCardRes::handle(&dev, &file, 0, &mut request).expect("empty query");
    assert_eq!(request.count_fbs, 0);
}

#[def_test(serial)]
fn gem_backing_holder_keeps_exact_pages_alive_after_the_object_is_destroyed() {
    let (dev, backend) = fake_device();
    // 65 * 33 * 4 bytes requires three pages and rounds raw inventory to four.
    let object = GemObject::create(&dev, 65, 33).unwrap();
    let mapped = object.pages().expect("an owned object hands out its pages");
    assert_eq!(mapped.page_count(), 3);
    assert_eq!(object.length_bytes(), 3 * PAGE_SIZE_4K);
    assert_eq!(mapped.retained_range().size(), 3 * PAGE_SIZE_4K);
    drop(object);
    assert_eq!(backend.destroyed(), 1);
    let retained = mapped.try_clone_page(2).unwrap();
    let address: *const u8 = khal::mem::p2v(retained.phys_addr()).as_ptr();
    // SAFETY: the mapping owner and retained child keep this initialized page
    // live; the fake backend and this fixture never write to its contents.
    let bytes = unsafe { core::slice::from_raw_parts(address, PAGE_SIZE_4K) };
    assert!(bytes.iter().all(|byte| *byte == 0));
    drop(mapped);
    assert_eq!(retained.page_count(), 1);
    assert_eq!(retained.retained_range().size(), PAGE_SIZE_4K);
    drop(retained);
    assert_eq!(backend.destroyed(), 1);
}

struct MmapTestFileOperations;

impl kvfs::FileOperations for MmapTestFileOperations {}

fn mmap_test_file(state: Arc<DrmFile>) -> Arc<VfsFile> {
    static FS_TYPE: kvfs::FileSystemType = kvfs::FileSystemType::internal("drm-mmap-test");
    let super_block = kvfs::SimpleFs::new_with(&FS_TYPE, 0, |fs| {
        kvfs::SimpleDir::new_maker(fs, Arc::new(kvfs::DirMapping::new()))
    })
    .unwrap();
    let mount = kvfs::Mount::new_root(&super_block);
    let operations = Arc::new(MmapTestFileOperations);
    let inode = VfsInode::new_file_with_operations(
        Arc::new(()),
        kvfs::empty_inode_operations(),
        operations.clone(),
        NodeFlags::PRIVATE,
        kvfs::VfsInodeInit::new(
            2,
            0,
            kvfs::Umode::new(kvfs::NodeType::RegularFile, kvfs::NodePermission::default()),
        ),
    );
    let file = mount
        .alloc_file_pseudo(
            inode,
            "drm-mmap",
            kvfs::FMode::READ | kvfs::FMode::WRITE,
            kvfs::OpenFlags::READ_WRITE,
            operations,
            kcred::initial_cred(),
        )
        .unwrap();
    file.set_private_data(state);
    file
}

struct RecordingMmapMapper {
    offset_bytes: usize,
    range: Option<PhysAddrRange>,
    requested_length: Option<usize>,
}

impl MmapMapper for RecordingMmapMapper {
    fn map_physical(&mut self, range: PhysAddrRange) -> VfsResult<()> {
        if self
            .requested_length
            .is_some_and(|length| range.size() < length)
        {
            return Err(VfsError::InvalidInput);
        }
        self.range = Some(range);
        Ok(())
    }

    fn map_file_backed(&mut self) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn map_anonymous_shared(&mut self) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn offset(&self) -> usize {
        self.offset_bytes
    }
}

#[def_test(serial)]
fn drm_mmap_retains_exact_backing_after_handle_and_object_destruction() {
    let before = kalloc::global_allocator().usages().get(UsageKind::Global);
    let (dev, backend) = fake_device();
    let state = dev.open_file(MinorKind::Render).unwrap();
    let file = mmap_test_file(state.clone());
    let minor = dev.minor(MinorKind::Render, 0);
    // Three requested pages distinguish the mapping length from four raw pages.
    let object = GemObject::create(&dev, 65, 33).unwrap();
    let pages = object.pages().expect("owned pages");
    let backing = Arc::downgrade(&pages);
    drop(pages);
    let offset_bytes = object.offset_bytes();
    let handle = object.handle();
    let mut mapper = RecordingMmapMapper {
        offset_bytes: offset_bytes as usize,
        range: None,
        requested_length: None,
    };
    state.handles.lock().insert(handle, object.clone());
    minor.mmap(&file, &mut mapper).unwrap();
    assert_eq!(mapper.range.unwrap().size(), 3 * PAGE_SIZE_4K);

    // The mapping, not the handle, is what the card keeps: the retention table
    // names the mapped object by the mapping's offset.
    let retained = dev
        .retained_mapping(offset_bytes)
        .expect("mmap must retain the object");
    assert!(Arc::ptr_eq(&retained, &object));
    drop(retained);
    assert_eq!(backend.destroyed(), 0);

    // The handle and the test's own reference go away. Whatever else still
    // names the object, the mapping's retention must keep the pages alive.
    drop(state.take_handle(handle).unwrap());
    drop(object);
    assert!(
        dev.retained_mapping(offset_bytes).is_some(),
        "the mapping keeps the object after every other owner is gone"
    );
    let retained = backing.upgrade().expect("mmap must retain GEM backing");
    assert_eq!(retained.page_count(), 3);
    assert_eq!(retained.retained_range().size(), 3 * PAGE_SIZE_4K);
    drop(retained);

    drop(minor);
    drop(file);
    drop(state);
    drop(dev);
    assert!(backing.upgrade().is_none());
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::Global),
        before
    );
}

#[def_test(serial)]
fn oversized_drm_mmap_does_not_retain_gem_backing() {
    let before = kalloc::global_allocator().usages().get(UsageKind::Global);
    let (dev, _backend) = fake_device();
    let state = dev.open_file(MinorKind::Render).unwrap();
    let file = mmap_test_file(state.clone());
    let minor = dev.minor(MinorKind::Render, 0);
    let object = GemObject::create(&dev, 65, 33).unwrap();
    let backing = Arc::downgrade(&object.pages().expect("owned pages"));
    let handle = object.handle();
    let mut mapper = RecordingMmapMapper {
        offset_bytes: object.offset_bytes() as usize,
        range: None,
        // The GEM object backs three pages; ask the mapper for four.
        requested_length: Some(4 * PAGE_SIZE_4K),
    };
    state.handles.lock().insert(handle, object.clone());
    assert!(matches!(
        minor.mmap(&file, &mut mapper),
        Err(VfsError::InvalidInput)
    ));
    assert!(mapper.range.is_none());

    drop(state.take_handle(handle).unwrap());
    drop(object);
    assert!(
        backing.upgrade().is_none(),
        "a failed mmap must not retain GEM backing for the device lifetime"
    );
    drop(minor);
    drop(file);
    drop(state);
    drop(dev);
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::Global),
        before
    );
}

#[def_test(serial)]
fn failed_scanout_creation_reclaims_all_exact_backing_pages() {
    let (dev, backend) = fake_device();
    backend.fail_next_create.store(true, Ordering::Relaxed);
    let before = kalloc::global_allocator().usages().get(UsageKind::Global);
    let result = GemObject::create(&dev, 65, 33);
    assert!(matches!(result, Err(DrmError::Io)));
    assert_eq!(backend.created.load(Ordering::Relaxed), 1);
    assert_eq!(backend.destroyed(), 0);
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::Global),
        before
    );
    let retry = GemObject::create(&dev, 65, 33).unwrap();
    assert_eq!(retry.pages().expect("owned pages").page_count(), 3);
    drop(retry);
    assert_eq!(backend.destroyed(), 1);
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::Global),
        before
    );
}

// ---------------------------------------------------------------------------
// Vertical blanking: request resolution and completion on the timeline.
// ---------------------------------------------------------------------------

/// A tick handler for tests that inspect a clock without a card.
struct NoTicks;

impl TickHandler for NoTicks {
    fn on_tick(&self) {}
}

/// What a display reports its blanks to.
type BlankCallback = Arc<dyn Fn(display::VblankTick) + Send + Sync>;

/// A display that reports its own blanks.
struct ReportingBackend {
    inner: FakeBackend,
    emitted: Mutex<core::cell::RefCell<Option<BlankCallback>>>,
}

impl ReportingBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: FakeBackend::default(),
            emitted: Mutex::new(core::cell::RefCell::new(None)),
        })
    }

    /// Reports one blank, as the display's completion would.
    fn emit(&self, sequence: u32) {
        let callback = self
            .emitted
            .lock()
            .borrow()
            .clone()
            .expect("the card subscribed before the display reported a blank");
        callback(display::VblankTick {
            sequence,
            timestamp: ktime_types::MonotonicInstant::from_span_since_origin(
                TimeSpan::from_millis(16),
            ),
        });
    }
}

impl Backend for ReportingBackend {
    fn is_available(&self) -> bool {
        true
    }

    fn info(&self) -> DisplayInfo {
        self.inner.info()
    }

    fn caps(&self) -> BackendCaps {
        self.inner.caps()
    }

    fn vblank_source(&self) -> Option<&dyn display::VblankSource> {
        Some(self)
    }

    fn create_resource(
        &self,
        id: ResourceId,
        spec: ResourceSpec,
        window: &display::ScanoutWindow<'_>,
    ) -> DrmResult<()> {
        self.inner.create_resource(id, spec, window)
    }

    fn destroy_resource(&self, id: ResourceId) -> DrmResult<()> {
        self.inner.destroy_resource(id)
    }

    fn present(&self, id: ResourceId, rect: Rect, host_rendered: bool) -> DrmResult<()> {
        self.inner.present(id, rect, host_rendered)
    }

    fn flush(&self, id: ResourceId, rect: Rect, host_rendered: bool) -> DrmResult<()> {
        self.inner.flush(id, rect, host_rendered)
    }
}

impl display::VblankSource for ReportingBackend {
    fn origin(&self) -> display::VblankOrigin {
        display::VblankOrigin::Scanout
    }

    fn subscribe(&self, callback: BlankCallback) -> display::DriverResult<()> {
        *self.emitted.lock().borrow_mut() = Some(callback);
        Ok(())
    }
}

#[def_test]
fn a_card_without_a_source_drives_the_timeline_from_its_timer() {
    // Linux virtio-gpu has no blank interrupt and uses
    // `DRM_CRTC_VBLANK_TIMER_FUNCS`; a card whose display reports no blanking
    // still has a timeline, so `WAIT_VBLANK` is servable.
    let (dev, _backend) = fake_device();
    assert!(dev.vblank_supported());
}

#[def_test]
fn a_display_that_reports_blanks_drives_the_timeline() {
    let backend = ReportingBackend::new();
    let dev = DrmDevice::new_with_backend(backend.clone());
    assert!(dev.vblank_supported());
    backend.emit(7);
    assert_eq!(dev.vblank_clock().tick().sequence, 7);
}

#[def_test]
fn relative_wait_counts_from_the_files_cursor() {
    let mut tracker = SequenceTracker::default();
    assert_eq!(resolve_relative(&mut tracker, 10, 0), VblankRequest::AtNext);
    assert_eq!(resolve_relative(&mut tracker, 10, 1), VblankRequest::AtNext);
    assert_eq!(
        resolve_relative(&mut tracker, 12, 4),
        VblankRequest::Ahead(3)
    );
}

#[def_test]
fn absolute_targets_resolve_in_the_clients_wrap_domain() {
    let mut tracker = SequenceTracker::default();
    assert_eq!(
        resolve_absolute(&mut tracker, 100, 100),
        VblankRequest::Past
    );
    assert_eq!(resolve_absolute(&mut tracker, 100, 99), VblankRequest::Past);
    assert_eq!(
        resolve_absolute(&mut tracker, 100, 101),
        VblankRequest::AtNext
    );
    assert_eq!(
        resolve_absolute(&mut tracker, 100, 104),
        VblankRequest::Ahead(4)
    );
}

#[def_test]
fn the_flag_word_is_checked_before_anything_else() {
    assert!(wait_vblank_flags_valid(
        DRM_VBLANK_RELATIVE | DRM_VBLANK_NONBLOCK
    ));
    assert!(!wait_vblank_flags_valid(0x8000));
    assert!(!wait_vblank_flags_valid(DRM_VBLANK_RELATIVE | 0x20));
    assert!(wait_vblank_crtc_valid(DRM_VBLANK_HIGH_CRTC, 0));
    assert!(!wait_vblank_crtc_valid(DRM_VBLANK_HIGH_CRTC, 0x0100));
}

#[def_test]
fn a_clock_without_ticks_reports_its_state() {
    let clock = VblankClockState::new();
    let handler = NoTicks;
    clock.report(
        41,
        ktime_types::MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1)),
        &handler,
    );
    assert_eq!(clock.tick().sequence, 41);
    assert_eq!(VblankRequest::AtNext.target(41), 42);
    assert!(!clock.reached(42));
    assert!(
        !clock.reached(43),
        "a target two blanks ahead is still ahead"
    );
    assert!(clock.reached(40));
}

// ---------------------------------------------------------------------------
// Synchronization: fences, reservations, fence arrays and syncobjs.
//
// The layer below the ioctls is reachable without a display: fences, their
// chains and their waiters are plain objects, so the rules the ioctls rely on
// (once-signaled, error propagation, replaced values, dependency order) are
// proven here instead of only on target.
// ---------------------------------------------------------------------------

/// Counts wakes delivered to a fence waiter, without a task to park.
struct WakeCounter(AtomicUsize);

impl alloc::task::Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A waker that only counts how often a waiter was woken.
fn counting_waker() -> (Arc<WakeCounter>, core::task::Waker) {
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = core::task::Waker::from(counter.clone());
    (counter, waker)
}

fn fence_list(fences: &[Arc<DmaFence>]) -> Arc<FenceList> {
    FenceList::new(fences.to_vec())
}

/// The one-element set of `fence`.
fn one(fence: &Arc<DmaFence>) -> Arc<FenceList> {
    FenceList::new(alloc::vec![fence.clone()])
}

#[def_test(serial)]
fn a_fence_signals_once_and_keeps_its_first_outcome() {
    let (fence, signal) = new_anonymous_fence();
    assert!(!fence.is_signaled());
    assert_eq!(fence.error(), 0);

    assert_eq!(signal.signal(), 0, "no callbacks is still the first signal");
    assert!(fence.is_signaled());
    assert_eq!(signal.signal(), 0, "a second signal runs nothing");
    assert_eq!(
        signal.signal_error(22),
        0,
        "an error cannot override success"
    );
    assert_eq!(fence.error(), 0, "the fence reports its first outcome");
    assert!(fence.timestamp().is_some(), "completion is timestamped");
}

#[def_test(serial)]
fn an_error_fence_reports_the_error() {
    let (fence, signal) = new_anonymous_fence();
    signal.signal_error(16);
    assert!(fence.is_signaled());
    assert_eq!(fence.error(), 16);
    assert_eq!(signal.signal(), 0, "success cannot clear an error");
    assert_eq!(fence.error(), 16);
}

#[def_test(serial)]
fn releasing_an_uncompleted_fence_does_not_complete_it() {
    // A lock-free capability cannot tell "lost" from "the fence itself is gone"
    // on drop, so it never completes the fence: the owner that loses the work
    // reports it explicitly (`Flip::fail` on the commit path).
    let (fence, signal) = new_anonymous_fence();
    let second = fence.clone();
    drop(signal);
    assert!(!fence.is_signaled());
    assert!(!second.is_signaled());
}

#[def_test(serial)]
fn a_callback_runs_once_at_the_terminal_state() {
    let (first, first_signal) = new_anonymous_fence();
    let (second, second_signal) = new_anonymous_fence();
    let (counter, waker) = counting_waker();
    let waiter = Wait::new();

    // Registering on a pending fence defers the wake until it completes.
    assert_eq!(waiter.outcome(&one(&first), &waker), None);
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);
    first_signal.signal();
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    first_signal.signal();
    assert_eq!(
        counter.0.load(Ordering::Relaxed),
        1,
        "one wake per completion"
    );
    assert_eq!(
        waiter.outcome(&fence_list(&[first]), &waker),
        Some(0),
        "a completed fence decides the wait"
    );

    // Registering on a completed fence runs the callback immediately.
    second_signal.signal();
    assert_eq!(waiter.outcome(&fence_list(&[second]), &waker), Some(0));
}

#[def_test(serial)]
fn a_failed_fence_reports_its_errno_to_the_wait() {
    let (fence, signal) = new_anonymous_fence();
    let (_counter, waker) = counting_waker();
    let waiter = Wait::new();
    assert_eq!(waiter.outcome(&one(&fence), &waker), None);
    signal.signal_error(16);
    assert_eq!(
        waiter.outcome(&fence_list(&[fence]), &waker),
        Some(16),
        "the producer's errno is the wait's answer"
    );
    assert_eq!(DrmError::from_fence(16), DrmError::Busy);
    assert_eq!(DrmError::from_fence(999), DrmError::Io);
}

#[def_test(serial)]
fn a_chain_node_completes_after_its_premise_and_its_work() {
    // `dma_fence_chain`: the node is done only once both the work it contains
    // and the timeline node before it completed.
    let (premise, premise_signal) = new_anonymous_fence();
    let (work, work_signal) = new_anonymous_fence();
    let node = fence_chain::node(Some(premise.clone()), work.clone());
    fence_chain::link(&node, node.chain_prev().as_ref());
    assert!(!node.is_signaled(), "a chain waits for both dependencies");

    work_signal.signal();
    assert!(!node.is_signaled(), "the premise is still pending");
    premise_signal.signal();
    assert!(node.is_signaled(), "both dependencies completed the node");
    assert_eq!(node.error(), 0);
}

#[def_test(serial)]
fn a_chain_node_reports_the_premise_failure_first() {
    let (premise, premise_signal) = new_anonymous_fence();
    let (work, work_signal) = new_anonymous_fence();
    let node = fence_chain::node(Some(premise), work);
    fence_chain::link(&node, node.chain_prev().as_ref());
    // The work fails first, but the node still waits for its premise.
    work_signal.signal_error(11);
    assert!(!node.is_signaled());
    // The premise fails too: the node reports the premise's error first.
    premise_signal.signal_error(16);
    assert!(node.is_signaled());
    assert_eq!(node.error(), 16);
}

#[def_test(serial)]
fn a_chain_walks_past_completed_premises_to_the_pending_work() {
    let (first, first_signal) = new_anonymous_fence();
    let (work, work_signal) = new_anonymous_fence();
    let inner = fence_chain::node(Some(first.clone()), work.clone());
    fence_chain::link(&inner, Some(&first));
    // Chain the inner node: its premise is a chain node again, which is what a
    // second replacement in a timeline builds.
    let outer = fence_chain::node(None, work.clone());
    fence_chain::link(&outer, Some(&inner));
    outer.set_chain_prev(Some(inner.clone()));

    first_signal.signal();
    // The inner node is still pending on its work, so the walk stops there.
    let pending = fence_chain::walk(&outer).expect("the inner node is still pending");
    assert!(Arc::ptr_eq(&pending, &inner));
    work_signal.signal();
    // Both chains completed: the walk passes every finished link.
    assert!(inner.is_signaled());
    assert!(outer.is_signaled());
    assert!(fence_chain::walk(&outer).is_none());
}

#[def_test(serial)]
fn a_fence_array_completes_when_every_member_does() {
    let ((first, first_signal), (second, second_signal)) =
        (new_anonymous_fence(), new_anonymous_fence());
    let list = fence_list(&[first.clone(), second.clone()]);
    assert!(
        !list_is_signaled(&list),
        "one pending member keeps the set pending"
    );
    first_signal.signal();
    assert!(!list_is_signaled(&list));
    second_signal.signal();
    assert!(list_is_signaled(&list));

    // The merge of an already complete set completes as soon as its members
    // are registered, and its head reports that state to whoever observes it.
    assert!(
        fence_array(&list).is_signaled(),
        "the merge of completed fences is done"
    );
    let single = fence_array(&one(&first));
    assert!(
        Arc::ptr_eq(&single, &first),
        "a one-fence merge is the fence itself"
    );
    let empty = fence_array(&FenceList::empty());
    assert!(empty.is_signaled(), "an empty merge is already done");
}

// The head fence of a set is only reachable through its array: the array owns
// the head strongly and the head refers back weakly. A unit test that builds
// the set by dropping its array would therefore observe a released fence (whose
// memory `Arc` may have recycled), so the array form is exercised through the
// array itself and the set semantics through its members.
#[def_test(serial)]
fn a_fence_array_member_set_is_the_completion_condition() {
    let ((first, first_signal), (second, second_signal)) =
        (new_anonymous_fence(), new_anonymous_fence());
    let array = FenceArray::new(
        alloc::vec![first.clone(), second.clone()],
        SyncContext::new(3),
        0,
    );
    let merged = array.head().clone();
    assert!(!array.is_complete(), "two pending members are not complete");
    assert!(!merged.is_signaled(), "the head waits for the whole set");

    second_signal.signal_error(11);
    assert!(!array.is_complete());
    assert!(
        !merged.is_signaled(),
        "a failed member does not complete the merge early"
    );
    first_signal.signal();
    assert!(array.is_complete());
    assert!(merged.is_signaled());
    assert_eq!(merged.error(), 11, "the merge reports the member failure");
    assert!(
        Arc::ptr_eq(
            merged.array().as_ref().expect("the head knows its array"),
            &array
        ),
        "the head names the array it completes"
    );
}

#[def_test(serial)]
fn a_pending_fence_with_a_recorded_error_stays_pending() {
    // `first_error` only reports completed failures; a pending premise must
    // not end a wait on a fence that may still succeed.
    let (fence, signal) = new_anonymous_fence();
    let list = one(&fence);
    assert_eq!(first_error(&list), 0);
    signal.signal_error(19);
    assert_eq!(first_error(&list), 19);
}

#[def_test(serial)]
fn a_reservation_orders_writers_and_readers() {
    let resv = DmaResv::new();
    let (write, write_signal) = new_anonymous_fence();
    let _previous = resv.set_write_fence(write.clone());
    let (read, read_signal) = new_anonymous_fence();
    let _previous = resv.add_read_fence(read.clone());

    let writers = resv.snapshot(ResvUsage::Read);
    assert_eq!(writers.fences().len(), 1, "readers wait only for writers");
    assert!(Arc::ptr_eq(&writers.fences()[0], &write));

    let everything = resv.snapshot(ResvUsage::Write);
    assert_eq!(everything.fences().len(), 2, "writers wait for everything");

    write_signal.signal();
    read_signal.signal();
    assert!(list_is_signaled(&everything));
}

#[def_test(serial)]
fn a_replaced_write_fence_stops_being_reported() {
    let resv = DmaResv::new();
    let (first, first_signal) = new_anonymous_fence();
    let (second, second_signal) = new_anonymous_fence();
    let _first_replaced = resv.set_write_fence(first);
    let _second_replaced = resv.set_write_fence(second.clone());
    let current = resv.snapshot(ResvUsage::Write);
    assert_eq!(current.fences().len(), 1);
    assert!(Arc::ptr_eq(&current.fences()[0], &second));
    drop(first_signal);
    second_signal.signal();
    assert!(list_is_signaled(&current));
}

#[def_test(serial)]
fn a_syncobj_value_is_chained_onto_the_one_it_replaces() {
    let object = Syncobj::new_binary();
    assert!(object.binary_fence().is_none(), "a new object has no value");

    let (first, first_signal) = new_anonymous_fence();
    object.replace_binary(first.clone());
    assert!(Arc::ptr_eq(&object.binary_fence().unwrap(), &first));

    // A replacement does not lose the first dependency: the installed value
    // is a chain node over it, so it only completes after the premise did.
    let (second, second_signal) = new_anonymous_fence();
    // A binary replacement is `drm_syncobj_replace_fence`: the new fence is
    // the whole value, and the replaced one is no longer part of it.
    object.replace_binary(second.clone());
    let value = object.binary_fence().unwrap();
    assert!(Arc::ptr_eq(&value, &second), "the value is the replacement");
    first_signal.signal_error(22);
    assert!(!value.is_signaled(), "the replaced fence does not gate it");
    second_signal.signal();
    assert!(value.is_signaled());
}

#[def_test(serial)]
fn a_timeline_answers_every_earlier_point() {
    let object = Syncobj::new_binary();
    let (fence, signal) = new_anonymous_fence();
    object.set_timeline(7, fence.clone());
    // Completing point 7 completes every earlier point of the same timeline,
    // and a later request waits for the next point to be signaled.
    assert!(
        Arc::ptr_eq(&object.timeline_fence(3).unwrap(), &fence),
        "a later point covers an earlier one"
    );
    assert!(Arc::ptr_eq(&object.timeline_fence(7).unwrap(), &fence));
    assert!(
        object.timeline_fence(9).is_none(),
        "a point past the furthest signal is not available yet"
    );
    assert_eq!(object.available_point(), 0);
    signal.signal();
    assert_eq!(object.available_point(), 7);

    let (first, _first_signal) = new_anonymous_fence();
    object.replace_binary(first);
    assert!(
        object.timeline_fence(7).is_none(),
        "a binary value has no points"
    );
}

#[def_test(serial)]
fn resetting_a_syncobj_clears_its_value() {
    let object = Syncobj::new_binary();
    let (fence, _signal) = new_anonymous_fence();
    object.set_timeline(4, fence);
    object.reset();
    assert!(
        object.timeline_fence(4).is_none(),
        "a reset timeline has no points"
    );
    assert_eq!(object.available_point(), 0);

    let (fence, _signal) = new_anonymous_fence();
    object.replace_binary(fence);
    object.reset();
    assert!(object.binary_fence().is_none());
}

#[def_test(serial)]
fn a_syncobj_replacement_pokes_a_waiting_task() {
    let object = Syncobj::new_binary();
    let (counter, waker) = counting_waker();
    let waiter = Wait::new();
    let (fence, _signal) = new_anonymous_fence();
    object.replace_binary(fence.clone());
    assert_eq!(waiter.outcome(&fence_list(&[fence]), &waker), None);
    object.watch(waiter.waiter());
    let (replacement, replacement_signal) = new_anonymous_fence();
    object.replace_binary(replacement.clone());
    // The old value was replaced and may never complete; the waiter was poked
    // so that its next evaluation observes the new value.
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    replacement_signal.signal();
    let value = object.binary_fence().unwrap();
    assert!(
        Arc::ptr_eq(&value, &replacement),
        "the replacement is the value"
    );
    assert!(value.is_signaled());
}

#[def_test(serial)]
fn a_syncobj_table_names_one_object_per_file() {
    let mut table = SyncobjTable::new();
    let object = Syncobj::new_binary();
    let handle = table.insert(object.clone()).expect("insert");
    assert!(Arc::ptr_eq(&table.get(handle).unwrap(), &object));
    assert_eq!(
        table.get(SyncobjHandle::from_raw(1000)).err(),
        Some(DrmError::NotFound)
    );
    assert!(table.remove(handle).is_ok());
    assert_eq!(table.get(handle).err(), Some(DrmError::NotFound));
}

#[def_test(serial)]
fn an_eventfd_is_signaled_when_its_point_becomes_available() {
    let object = Syncobj::new_binary();
    let eventfd = kfd_objects::eventfd::EventFd::new(0, false);
    let event = crate::model::syncobj_signal::EventFdSignal::new(eventfd.clone(), 128, 1, 0);
    object.add_event(&event);
    // Nothing is installed yet, so there is nothing to arm.
    assert!(object.timeline_fence(1).is_none());

    let (fence, signal) = new_anonymous_fence();
    object.set_timeline(1, fence);
    let value = object.timeline_fence(1).expect("the point is installed");
    assert!(!value.is_signaled());
    // Completion of the installed value increments the eventfd.
    signal.signal();
    assert!(value.is_signaled());
    assert!(
        kpoll::Pollable::poll(eventfd.as_ref()).contains(IoEvents::IN),
        "the eventfd became readable"
    );
}

/// One EDID detailed timing descriptor: 1024x768@60, the classic layout.
fn dtd_1024x768_60() -> [u8; 18] {
    // Pixel clock 65.0 MHz, htotal 1344, vtotal 806, hsync at 1048 wide 136,
    // vsync at 771 wide 6.
    let mut dtd = [0u8; 18];
    let clock = 6_500;
    dtd[0] = (clock & 0xff) as u8;
    dtd[1] = (clock >> 8) as u8;
    dtd[2] = 0x00; // hactive low
    dtd[3] = 0x40; // hblank low (320)
    dtd[4] = 0x41; // hactive 1024 high nibble, hblank high nibble
    dtd[5] = 0x00; // vactive low
    dtd[6] = 0x26; // vblank low (38)
    dtd[7] = 0x30; // vactive 768 high nibble, vblank high nibble
    dtd[8] = 0x18; // hsync offset 24
    dtd[9] = 0x88; // hsync width 136
    dtd[10] = 0x36; // vsync offset 3, vsync width 6
    dtd[11] = 0x00; // all sync high bits fit in one byte
    dtd
}

#[def_test]
fn edid_detailed_timings_decode_a_descriptor() {
    let mut edid = [0u8; 128];
    edid[54..72].copy_from_slice(&dtd_1024x768_60());
    let timings = crate::edid::detailed_timings(&edid);
    assert!(timings.len() == 1, "one descriptor slot is filled");
    let timing = timings[0];
    assert!(timing.clock == 65_000, "pixel clock in kHz");
    assert!(timing.hdisplay == 1024 && timing.htotal == 1344);
    assert!(timing.vdisplay == 768 && timing.vtotal == 806);
    assert!(timing.hsync_start == 1048 && timing.hsync_end == 1184);
    assert!(timing.vsync_start == 771 && timing.vsync_end == 777);
    assert!(!timing.interlaced, "the flag byte is zero");
}

#[def_test]
fn edid_descriptor_slots_stop_at_the_first_monitor_descriptor() {
    let mut edid = [0u8; 128];
    edid[54..72].copy_from_slice(&dtd_1024x768_60());
    // A name descriptor starts with a zero pixel clock.
    edid[72] = 0;
    edid[75] = b'A';
    assert!(crate::edid::detailed_timings(&edid).len() == 1);
}

#[def_test]
fn edid_mode_names_its_geometry_and_refresh() {
    let mut edid = [0u8; 128];
    edid[54..72].copy_from_slice(&dtd_1024x768_60());
    let modes = crate::edid::modes(&edid);
    assert!(modes.len() == 1);
    let mode = modes[0];
    let name = mode.name.iter().position(|byte| *byte == 0).unwrap_or(0);
    let name = core::str::from_utf8(&mode.name[..name]).unwrap_or("");
    assert!(name == "1024x768", "name is {name}");
    assert!(mode.vrefresh == 60, "refresh is {}", mode.vrefresh);
    assert!(mode.kind == DRM_MODE_TYPE_PREFERRED as u32);
    assert!(mode.clock == 65_000);
}

#[def_test]
fn a_dma_buf_descriptor_answers_seek_end_with_its_size() {
    use crate::model::dma_buf::{DmaBuf, DmaBufBacking};

    // Mesa sizes an imported buffer with `lseek(SEEK_END)` before mapping it.
    // `VfsFile::llseek` rejects the syscall with ESPIPE unless the open mode
    // carries `FMode::LSEEK`, so the flag has to be part of the descriptor's
    // open mode and not only of its `FileOperations::llseek` op.
    let buf = DmaBuf::new(
        DmaBufBacking {
            start: 0,
            length: 3 * crate::prelude::PAGE_SIZE_4K,
            owner: None,
        },
        crate::sync::dma_resv::DmaResv::new(),
        alloc::string::String::from("test-buffer"),
        1,
        1,
        None,
    );
    let file = crate::model::dma_buf_file::new_dma_buf_file(&buf, &kcred::initial_cred())
        .expect("a dma-buf descriptor must be created");
    assert!(
        file.mode().contains(kvfs::FMode::LSEEK),
        "a dma-buf descriptor must allow seeking"
    );
    assert_eq!(
        file.llseek(0, 2).expect("SEEK_END must answer the size"),
        (3 * crate::prelude::PAGE_SIZE_4K) as u64
    );
    // `dma_buf_llseek` also accepts SEEK_SET(0), which is how a client that
    // wants the size without moving the offset asks for it.
    assert_eq!(
        file.llseek(0, 0).expect("SEEK_SET(0) must answer the base"),
        0
    );
    // Any other whence, or a non-zero offset, is EINVAL as in Linux.
    assert!(file.llseek(0, 1).is_err(), "SEEK_CUR is not supported");
    assert!(
        file.llseek(1, 2).is_err(),
        "a non-zero offset is rejected"
    );
}

/// A present must transfer the framebuffer's geometry, not its object's size.
///
/// A framebuffer smaller than its object is displayed through a view resource
/// created from the framebuffer geometry. Naming the object's rows in the
/// transfer rectangle would ask the host for rows that view does not contain,
/// and the display rejects the command (`EIO`) — exactly what glmark2 hit with
/// a 1280x800 framebuffer inside a 1280x832 dumb object.
#[def_test]
fn a_windowed_present_transfers_the_framebuffer_geometry() {
    let (dev, backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");

    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let geometry = window_spec(&object);
    let fb = dev
        .add_framebuffer(&file, object.clone(), geometry, 0)
        .expect("fb");
    bind(&dev, fb).expect("present");

    let recorded = backend.last_rect.load(Ordering::Relaxed);
    assert_eq!(
        (recorded >> 32) as u32,
        geometry.width,
        "the transfer width must be the framebuffer's, not the object's"
    );
    assert_eq!(
        recorded as u32,
        geometry.height,
        "the transfer height must be the framebuffer's, not the object's"
    );
    assert!(
        geometry.height < object.height(),
        "the fixture must exercise a framebuffer smaller than its object"
    );
}

/// `DRM_IOCTL_MODE_DIRTYFB` must update the resource the host scans.
///
/// A framebuffer smaller than its object is displayed through a view resource
/// created from the framebuffer geometry. Flushing the object's own resource
/// would push pixels into memory the display is not reading, so the visible
/// contents stay stale even though the call succeeded.
#[def_test]
fn a_windowed_dirtyfb_flushes_the_view_resource() {
    let (dev, backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");

    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev
        .add_framebuffer(&file, object.clone(), window_spec(&object), 0)
        .expect("fb");
    bind(&dev, fb).expect("present");

    let object_resource = object.resource().expect("object resource");

    dev.flush(fb).expect("dirtyfb");
    // `present` records the resource it put on screen; a flush of the same
    // framebuffer must name that same resource.
    assert_eq!(
        backend.last_flush_resource.load(Ordering::Relaxed),
        backend.last_present_resource.load(Ordering::Relaxed),
        "flush must target the resource present put on screen"
    );
    assert_ne!(
        backend.last_flush_resource.load(Ordering::Relaxed),
        object_resource.raw(),
        "a windowed present scans a view resource, not the object's own"
    );
}
