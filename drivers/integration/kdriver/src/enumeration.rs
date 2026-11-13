// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Enumeration context for bus backends.
//!
//! Bus backends use [`EnumerationContext`] while emitting discovery-stage
//! descriptors. Matching/probing is run after enumeration through the current
//! `kdevice` compatibility bridge.

use alloc::vec::Vec;

use driver_base::DriverResult;
use kdevice::{DeviceDesc, DeviceDescId, DeviceDescSpec, ProbeOutcome};

/// Context used while bus backends enumerate device descriptors.
///
/// The backend-facing API records descriptors only. Probing remains a separate
/// step so bus/firmware discovery does not directly manufacture runtime device
/// objects.
pub struct EnumerationContext {
    pending: Vec<DeviceDesc>,
    unclaimed: Vec<DeviceDesc>,
    registered_count: usize,
}

impl EnumerationContext {
    /// Create an empty enumeration context.
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            unclaimed: Vec::new(),
            registered_count: 0,
        }
    }

    /// Record an enumerated descriptor that remains unclaimed after probing.
    pub fn add_unclaimed(&mut self, desc: DeviceDesc) {
        log::debug!("unclaimed device descriptor: {:?}", desc);
        self.unclaimed.push(desc);
    }

    /// Add one finalized discovery result to the shared device core.
    ///
    /// The specification is consumed as-is. Registration stores the descriptor
    /// copy that later probe attempts rebuild from, so a retried probe keeps
    /// the same firmware-node association.
    pub fn register_device(&mut self, spec: DeviceDescSpec) -> DriverResult<DeviceDescId> {
        let desc = kdevice::device_desc_add(spec)?;
        let id = desc.id();
        self.registered_count += 1;
        self.pending.push(desc);
        Ok(id)
    }

    /// Probe every descriptor registered during this enumeration pass.
    pub fn probe_pending(&mut self) -> usize {
        let pending = core::mem::take(&mut self.pending);
        let mut activated = 0usize;

        for desc in pending {
            match kdevice::probe_device_desc(desc.id()) {
                Ok(ProbeOutcome::Activated) => activated += 1,
                Ok(ProbeOutcome::Skipped) => {}
                Ok(ProbeOutcome::Unmatched | ProbeOutcome::Unclaimed) => self.add_unclaimed(desc),
                Ok(ProbeOutcome::Busy) => {
                    // Same-context re-entry means a probe path tried to bind
                    // the descriptor it is already handling. The descriptor
                    // remains Pending and a later pass can retry it.
                    log::warn!(
                        "descriptor {:?} is already being bound on this task",
                        desc.id()
                    );
                    self.add_unclaimed(desc);
                }
                Err(err) => {
                    log::warn!(
                        "failed to probe descriptor {:?} ({:?} at {:?}): {:?}",
                        desc.id(),
                        desc.identity(),
                        desc.location(),
                        err
                    );
                    self.add_unclaimed(desc);
                }
            }
        }

        activated
    }

    /// Iterate over all enumerated descriptors that were not immediately terminal.
    pub fn unclaimed(&self) -> &[DeviceDesc] {
        &self.unclaimed
    }

    /// Number of descriptors emitted in this enumeration pass.
    pub fn registered_count(&self) -> usize {
        self.registered_count
    }

    /// Number of enumerated descriptors that were not immediately terminal.
    pub fn unclaimed_count(&self) -> usize {
        self.unclaimed.len()
    }
}

impl Default for EnumerationContext {
    fn default() -> Self {
        Self::new()
    }
}

unittest::path_mod_test!(enumeration_unittest, "../ut-tests/enumeration_unittest.rs");
// These regressions spawn real kernel tasks and use `ksync` semaphores, so they
// need the task-layer feature rather than whichever driver feature happens to
// pull it in.
unittest::path_mod_test!(
    #[cfg(feature = "unittest-threads")]
    mod descriptor_binding_unittest,
    "../ut-tests/descriptor_binding_unittest.rs"
);
