// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for the parts of the object model that are pure functions: the
//! permission matrix over the ioctl table and the mode-blob encoding.

use unittest::{assert, assert_eq, def_test};

use crate::{
    MinorKind,
    kms::{ObjectType, PROPS, PropKind, apply, atomic_flags_valid, mode_blob_bytes, mode_from_bytes},
    model::ioctl::{IOCTLS, Ioctl, IoctlFlags, check_permission, report_user_array},
    prelude::*,
};

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
fn render_nodes_only_accept_render_allowed_commands() {
    // `(command x node)` matrix: on a render node the answer is exactly the
    // `DRM_RENDER_ALLOW` bit, and the failure is EACCES (`PermissionDenied`).
    for desc in IOCTLS {
        let allowed = desc.flags.contains(IoctlFlags::RENDER_ALLOW);
        let result = check_permission(MinorKind::Render, desc.flags, true);
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
fn master_commands_require_ownership_on_the_primary_node() {
    // Pinned from Linux's `drm_ioctl.c` table, not derived from the flags under
    // test: a command that silently lost its MASTER bit must fail here.
    let expected = [
        "MODE_SETCRTC",
        "MODE_CURSOR",
        "MODE_CONNECTOR_SETPROPERTY",
        "MODE_PAGE_FLIP",
        "MODE_DIRTYFB",
        "MODE_OBJ_SETPROPERTY",
        "MODE_CURSOR2",
        "MODE_ATOMIC",
        "AUTH_MAGIC",
    ];
    for desc in IOCTLS {
        let expected_master = expected.contains(&desc.name);
        assert_eq!(
            desc.flags.contains(IoctlFlags::MASTER),
            expected_master,
            "{} disagrees with the Linux master flag",
            desc.name
        );
        assert_eq!(
            check_permission(MinorKind::Primary, desc.flags, false).is_ok(),
            !expected_master,
            "{} disagrees with the master permission",
            desc.name
        );
        assert!(
            check_permission(MinorKind::Primary, desc.flags, true).is_ok(),
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
    use crate::model::{backend::BackendCaps, protocol::capability};

    const DRM_CAP_PRIME: u64 = 0x5;
    let caps = BackendCaps { dumb: true };
    assert_eq!(capability(DRM_CAP_DUMB_BUFFER, caps), 1);
    // Xorg's modesetting driver only enables ShadowFB (and therefore DIRTYFB)
    // when this is 1; Linux virtio-gpu reports the same value.
    assert_eq!(capability(DRM_CAP_DUMB_PREFER_SHADOW, caps), 1);
    // No dma-buf implementation, so PRIME must stay unadvertised.
    assert_eq!(capability(DRM_CAP_PRIME, caps), 0);
    assert_eq!(capability(0xffff, caps), 0);
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
        DrmModeSetCrtc::handle(&dev, &file, &mut request),
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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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
    flushed: AtomicUsize,
    fail_next_present: AtomicBool,
    fail_next_create: AtomicBool,
}

impl FakeBackend {
    fn set_resolution(&self, width: u32, height: u32) {
        self.resolution
            .store((u64::from(width) << 32) | u64::from(height), Ordering::Relaxed);
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
        BackendCaps { dumb: true }
    }

    fn create_resource(
        &self,
        _id: ResourceId,
        _spec: ResourceSpec,
        _paddr: u64,
        _length: u32,
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

    fn present(&self, _id: ResourceId, _rect: Rect) -> DrmResult<()> {
        if self
            .fail_next_present
            .swap(false, core::sync::atomic::Ordering::Relaxed)
        {
            return Err(DrmError::Io);
        }
        self.presented.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn flush(&self, _id: ResourceId, _rect: Rect) -> DrmResult<()> {
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
    let fb = dev.add_framebuffer(&file, object.clone()).expect("fb");
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
    let next_fb = dev.add_framebuffer(&file, next).expect("fb");
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
    assert!(check_permission(first.kind, IoctlFlags::MASTER, dev.is_master(&first)).is_ok());

    dev.drop_master(&first).expect("drop master");
    assert!(!dev.is_master(&first), "DROP_MASTER must end ownership");
    assert_eq!(
        check_permission(first.kind, IoctlFlags::MASTER, dev.is_master(&first)),
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
    let fb = dev.add_framebuffer(&file, object).expect("fb");
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
    let first_fb = dev.add_framebuffer(&file, first).expect("fb");
    bind(&dev, first_fb).expect("present");
    dev.remove_framebuffer(&file, first_fb).expect("rmfb");
    assert_eq!(backend.destroyed(), 0);

    // A failed present means the host may have switched anyway: the attempted
    // resource must stay alive as well.
    let second = GemObject::create(&dev, 64, 64).expect("dumb");
    let second_fb = dev.add_framebuffer(&file, second).expect("fb");
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
    let third_fb = dev.add_framebuffer(&file, third).expect("fb");
    bind(&dev, third_fb).expect("present");
    assert_eq!(backend.destroyed(), 2);
}

#[def_test]
fn closefb_refuses_a_framebuffer_that_is_still_bound() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev.add_framebuffer(&file, object).expect("fb");
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
    let fb = dev.add_framebuffer(&owner, object).expect("fb");

    // Linux looks the id up in `file_priv->fbs`, so a foreign id is
    // indistinguishable from an unknown one.
    assert_eq!(
        dev.remove_framebuffer(&other, fb),
        Err(DrmError::NotFound)
    );
    assert_eq!(
        dev.close_framebuffer(&other, fb),
        Err(DrmError::NotFound)
    );
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
    dev.destroy_client_blob(None, id).expect("destroy legacy blob");
    assert!(dev.blob_bytes(id).is_none());
    assert_eq!(
        dev.legacy_mode_blob(),
        None,
        "destroying the blob must clear the device's legacy slot"
    );
    // A second destroy is ENOENT, and the slot no longer names it.
    assert_eq!(
        dev.destroy_client_blob(None, id),
        Err(DrmError::NotFound)
    );
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
    let fb = dev.add_framebuffer(&file, object).expect("fb");
    let mut request = DrmModeCrtcPageFlip {
        crtc_id: CRTC_ID,
        fb_id: fb.raw(),
        ..Default::default()
    };

    assert_eq!(
        DrmModeCrtcPageFlip::handle(&dev, &file, &mut request),
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
    for (id, name) in [
        (PROP_PLANE_CRTC_X, "CRTC_X"),
        (PROP_PLANE_CRTC_Y, "CRTC_Y"),
    ] {
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
        assert_eq!(write(&mut state, 0x8000_0000, &dev), Err(DrmError::InvalidInput));
        assert_eq!(write(&mut state, u32::MAX as u64, &dev), Err(DrmError::InvalidInput));
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
        DrmModeObjGetProperties::handle(&dev, &file, &mut request),
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
        DrmModeObjGetProperties::handle(&dev, &file, &mut wrong),
        Err(DrmError::NotFound)
    );

    let mut unknown = DrmModeObjGetProperties {
        obj_id: 0,
        obj_type: 0,
        ..Default::default()
    };
    assert_eq!(
        DrmModeObjGetProperties::handle(&dev, &file, &mut unknown),
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

    let result = <DrmModeGetBlob as Ioctl>::handle(&dev, &file, &mut request);
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
        <DrmModeAtomic as Ioctl>::handle(&dev, &file, &mut request),
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
        DrmModeGetConnector::handle(&dev, &file, &mut connector).expect("connector");
        assert_eq!((connector.mm_width, connector.mm_height), (0, 0));
        assert_eq!(connector.count_modes, 1);
        assert_eq!(connector.connection, DRM_MODE_CONNECTED);

        let mut resources = DrmModeCardRes::default();
        DrmModeCardRes::handle(&dev, &file, &mut resources).expect("resources");
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

    let mode = current_mode(&dev);
    assert_eq!((mode.hdisplay, mode.vdisplay), (1280, 800));
    assert_eq!(mode_name(&mode), "1280x800");

    // A resized host window must change both the pixels and the mode name, so
    // a client that re-reads the mode list can tell the new mode apart.
    backend.set_resolution(1024, 768);
    let mode = current_mode(&dev);
    assert_eq!((mode.hdisplay, mode.vdisplay), (1024, 768));
    assert_eq!(mode_name(&mode), "1024x768");

    // The query envelope must not shrink to the new mode, otherwise a client
    // that grew the window could never ask for the larger size again.
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let mut resources = DrmModeCardRes::default();
    DrmModeCardRes::handle(&dev, &file, &mut resources).expect("resources");
    assert_eq!((resources.max_width, resources.max_height), (8192, 8192));
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
            DrmModeGetConnector::handle(&dev, &file, &mut connector),
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
    let first = dev.add_framebuffer(&owner, object.clone()).expect("fb");
    let second = dev.add_framebuffer(&owner, object.clone()).expect("fb");
    let foreign = dev.add_framebuffer(&other, object).expect("foreign");

    let mut request = DrmModeCardRes::default();
    DrmModeCardRes::handle(&dev, &owner, &mut request).expect("owner query");
    assert_eq!(request.count_fbs, 2);
    request = DrmModeCardRes::default();
    DrmModeCardRes::handle(&dev, &other, &mut request).expect("other query");
    assert_eq!(request.count_fbs, 1);

    dev.remove_framebuffer(&owner, first).expect("rmfb");
    request = DrmModeCardRes::default();
    DrmModeCardRes::handle(&dev, &owner, &mut request).expect("after remove");
    assert_eq!(request.count_fbs, 1);
    dev.remove_framebuffer(&owner, second).expect("rmfb");
    dev.remove_framebuffer(&other, foreign).expect("rmfb");
}

#[def_test]
fn getresources_count_query_does_not_access_the_id_buffer() {
    let (dev, _backend) = fake_device();
    let file = dev.open_file(MinorKind::Primary).expect("open");
    let object = GemObject::create(&dev, 64, 64).expect("dumb");
    let fb = dev.add_framebuffer(&file, object).expect("fb");
    let mut request = DrmModeCardRes {
        fb_id_ptr: UserPtr::from(1_usize),
        ..Default::default()
    };

    DrmModeCardRes::handle(&dev, &file, &mut request).expect("length query");
    assert_eq!(request.count_fbs, 1);
    // Reset output counts so an unrelated null array cannot cause EFAULT.
    request = DrmModeCardRes {
        fb_id_ptr: UserPtr::default(),
        count_fbs: 1,
        ..Default::default()
    };
    assert_eq!(
        DrmModeCardRes::handle(&dev, &file, &mut request),
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

    DrmModeCardRes::handle(&dev, &file, &mut request).expect("empty query");
    assert_eq!(request.count_fbs, 0);
}

#[def_test(serial)]
fn gem_backing_holder_keeps_exact_pages_alive_after_the_object_is_destroyed() {
    let (dev, backend) = fake_device();
    // 65 * 33 * 4 bytes requires three pages and rounds raw inventory to four.
    let object = GemObject::create(&dev, 65, 33).unwrap();
    let mapped = object.pages();
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
    let minor = dev.minor(MinorKind::Render);
    // Three requested pages distinguish the mapping length from four raw pages.
    let object = GemObject::create(&dev, 65, 33).unwrap();
    let paddr = object.pages().phys_addr();
    let backing = Arc::downgrade(&object.pages());
    let handle = object.handle();
    let mut mapper = RecordingMmapMapper {
        offset_bytes: object.offset_bytes() as usize,
        range: None,
        requested_length: None,
    };
    state.handles.lock().insert(handle, object.clone());
    minor.mmap(&file, &mut mapper).unwrap();
    let range = mapper.range.unwrap();
    assert_eq!(range.start, paddr);
    assert_eq!(range.size(), 3 * PAGE_SIZE_4K);

    drop(state.take_handle(handle).unwrap());
    drop(object);
    assert_eq!(backend.destroyed(), 1);
    // No test-owned strong page reference survives the object. Only the mmap
    // path can retain this backing; removing retain_mapping makes upgrade fail.
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
    let (dev, backend) = fake_device();
    let state = dev.open_file(MinorKind::Render).unwrap();
    let file = mmap_test_file(state.clone());
    let minor = dev.minor(MinorKind::Render);
    let object = GemObject::create(&dev, 65, 33).unwrap();
    let backing = Arc::downgrade(&object.pages());
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
    assert_eq!(backend.destroyed(), 1);
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
    assert_eq!(retry.pages().page_count(), 3);
    drop(retry);
    assert_eq!(backend.destroyed(), 1);
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::Global),
        before
    );
}
