// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IO-APIC registry, boot lifecycle, GSI resolution, and per-pin operations.
//!
//! Controllers are probed without being programmed, validated as a complete
//! topology, and only then installed in declaration order. The firmware GSI
//! range is consumed by that validation; an installed instance keeps only the
//! pins it implements and a per-pin delivery vector. The registry lock
//! serializes every register access after publication.
//!
//! Register-level programming lives in [`programming`]; this module owns the
//! objects and lifetime transitions around it.

use kcpu_id_map::RawCpuId;
use kerrno::{KError, KResult};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::PhysAddr;
use memspace::iomap_device;
use programming::{Registers, VectorTable, configure, retrigger};
use x2apic::ioapic::{IoApic, IrqFlags, IrqMode, RedirectionTableEntry};

#[cfg(unittest)]
use super::{IO_APIC_VECTOR_BASE, IoApicPolarity};
use super::{
    IoApicCapabilities, IoApicInitPolicy, IoApicInstanceId, IoApicPin, IoApicTriggerMode,
    IoApicTriggerPolicy, MAX_IO_APIC_PINS, MAX_IO_APICS, send_ipi_raw,
};

mod programming;

static IO_APICS: LazyInit<SpinNoIrq<IoApicRegistry>> = LazyInit::new();

/// One initialized IO-APIC together with the pins it actually implements.
///
/// `io_apic` supplies register access; `pin_count` bounds every per-pin
/// operation. The controller's firmware GSI base is consumed during boot
/// topology validation and is not needed after installation. `pin_vectors`
/// records the dynamic delivery vector assigned to each configured pin;
/// `configured_pins` marks pins whose most recent configuration transaction
/// succeeded. A reconfiguration attempt clears the mark first, so a failed
/// attempt cannot leave an old success standing in for the new state. Only
/// marked pins may be unmasked; a vector that was assigned to an earlier
/// successful configuration still remains installed.
struct IoApicController {
    io_apic: IoApic,
    pin_count: u16,
    pin_vectors: [Option<u8>; MAX_IO_APIC_PINS],
    configured_pins: [bool; MAX_IO_APIC_PINS],
}

impl IoApicController {
    fn new(io_apic: IoApic, pin_count: u16) -> Self {
        Self {
            io_apic,
            pin_count,
            pin_vectors: [None; MAX_IO_APIC_PINS],
            configured_pins: [false; MAX_IO_APIC_PINS],
        }
    }
}

/// Bounded registry of initialized IO-APIC instances and vector routes.
///
/// The registry lock serializes register access and protects the instance and
/// vector tables, which are immutable after boot except for the lazy vector
/// assignment performed by the first configuration of a pin.
struct IoApicRegistry {
    instances: [Option<IoApicController>; MAX_IO_APICS],
    count: usize,
    vectors: VectorTable,
}

impl IoApicRegistry {
    const fn new() -> Self {
        Self {
            instances: [const { None }; MAX_IO_APICS],
            count: 0,
            vectors: VectorTable::new(),
        }
    }

    fn controller(&self, instance: IoApicInstanceId) -> Option<&IoApicController> {
        self.instances.get(instance.as_usize())?.as_ref()
    }

    fn controller_mut(&mut self, instance: IoApicInstanceId) -> Option<&mut IoApicController> {
        self.instances.get_mut(instance.as_usize())?.as_mut()
    }

    fn install(
        &mut self,
        mut probed: ProbedIoApic,
        policy: IoApicInitPolicy,
    ) -> KResult<IoApicInstanceId> {
        if self.count >= MAX_IO_APICS {
            return Err(KError::Unsupported);
        }
        let instance = IoApicInstanceId::try_from_usize(self.count).ok_or(KError::Unsupported)?;
        let max_entry = u8::try_from(probed.pin_count - 1).map_err(|_| KError::InvalidInput)?;
        program_initial_redirection_table(&mut probed.io_apic, max_entry, policy);
        info!(
            "IO-APIC instance {} initialized and masked, gsi_base={}, pins=0-{}",
            instance.as_usize(),
            probed.gsi_base,
            max_entry
        );
        self.instances[instance.as_usize()] =
            Some(IoApicController::new(probed.io_apic, probed.pin_count));
        self.count += 1;
        Ok(instance)
    }

    /// Returns the vector assigned to one pin, assigning a new one on first use.
    ///
    /// Vectors are never released: a configured source keeps its delivery
    /// vector for the kernel lifetime, and the finite pool bounds the number
    /// of simultaneously configured pins.
    fn vector_for(&mut self, instance: IoApicInstanceId, pin: IoApicPin) -> Option<u8> {
        let controller = self.controller(instance)?;
        if pin.as_usize() >= usize::from(controller.pin_count) {
            return None;
        }
        if let Some(vector) = controller
            .pin_vectors
            .get(pin.as_usize())
            .copied()
            .flatten()
        {
            return Some(vector);
        }
        let vector = self.vectors.allocate(instance, pin)?;
        let controller = self.controller_mut(instance)?;
        controller.pin_vectors[pin.as_usize()] = Some(vector);
        Some(vector)
    }

    fn configure_pin(
        &mut self,
        instance: IoApicInstanceId,
        pin: IoApicPin,
        policy: IoApicTriggerPolicy,
    ) -> bool {
        let Some(vector) = self.vector_for(instance, pin) else {
            return false;
        };
        let Some(controller) = self.controller_mut(instance) else {
            return false;
        };
        // A failed attempt may leave the entry partially programmed. Clear the
        // previous success before touching hardware so the mark always
        // describes the most recent transaction, never an older one.
        controller.configured_pins[pin.as_usize()] = false;
        let configured = configure(&mut controller.io_apic, pin, policy, vector);
        controller.configured_pins[pin.as_usize()] = configured;
        configured
    }

    fn retrigger_pin(&mut self, instance: IoApicInstanceId, pin: IoApicPin) -> bool {
        let expected = self.controller(instance).and_then(|controller| {
            controller
                .pin_vectors
                .get(pin.as_usize())
                .copied()
                .flatten()
        });
        let Some(expected) = expected else {
            return false;
        };
        if self.vectors.route(usize::from(expected)) != Some((instance, pin)) {
            return false;
        }
        let Some(controller) = self.controller_mut(instance) else {
            return false;
        };
        retrigger(
            &mut controller.io_apic,
            pin,
            expected,
            |vector, destination| {
                send_ipi_raw(usize::from(vector), RawCpuId::new(usize::from(destination)));
            },
        )
    }
}

/// A probed IO-APIC that has not been programmed or published.
///
/// Probing maps the controller registers and reads its implemented pin count;
/// it performs no redirection-table write. Boot wiring validates all probed
/// ranges before installing any instance.
pub struct ProbedIoApic {
    io_apic: IoApic,
    gsi_base: u32,
    pin_count: u16,
}

impl ProbedIoApic {
    /// Hardware capabilities read from this controller.
    pub const fn capabilities(&self) -> IoApicCapabilities {
        IoApicCapabilities {
            pin_count: self.pin_count,
        }
    }
}

/// Resolves a GSI inside one controller range to its local redirection pin.
///
/// A GSI is a system route number, not a pin. Resolution is the checked
/// subtraction `gsi - gsi_base` bounded by the probed pin count; a GSI below
/// the base or outside the implemented table has no pin on this controller.
pub const fn pin_for_gsi_in_range(gsi: u32, gsi_base: u32, pin_count: u16) -> Option<IoApicPin> {
    match gsi.checked_sub(gsi_base) {
        Some(offset) if offset < pin_count as u32 => IoApicPin::try_from_usize(offset as usize),
        _ => None,
    }
}

/// Assigns the route's delivery vector while masked, optionally programs
/// trigger/polarity, checks readback, then restores the old mask.
///
/// A descriptor that does not state trigger or polarity still needs a delivery
/// route; [`IoApicTriggerPolicy::KeepHardware`] installs the vector without
/// rewriting those bits. Failure never intentionally unmasks, but a failed
/// mask readback cannot guarantee hardware suppression.
/// Does not change routing or prove that already delivered interrupts drained.
/// Requires initialized controller state; call in task/ordinary IRQ context,
/// never NMI, while the source owner excludes competing configuration.
pub fn configure_pin_checked(
    instance: IoApicInstanceId,
    pin: IoApicPin,
    policy: IoApicTriggerPolicy,
) -> bool {
    let Some(registry) = IO_APICS.get() else {
        return false;
    };
    registry.lock().configure_pin(instance, pin, policy)
}

/// Issues edge retrigger to the physical destination/vector read under the
/// registry lock. Rejects unsupported, changed or reassigned routing without
/// sending an IPI.
/// Call in task/ordinary IRQ context after local APIC and IO-APIC preparation,
/// with source ownership retained. This is not NMI-safe or a delivery drain.
pub fn retrigger_pin(instance: IoApicInstanceId, pin: IoApicPin) -> bool {
    let Some(registry) = IO_APICS.get() else {
        return false;
    };
    registry.lock().retrigger_pin(instance, pin)
}

/// Masks or unmasks one input pin of one IO-APIC instance.
///
/// The pin must be within that instance's probed redirection table; otherwise
/// the call is a no-op. Unmasking also requires the pin's most recent
/// configuration transaction to have succeeded, so a pin that was never given
/// a delivery vector, or whose last reconfiguration failed, stays suppressed.
/// The instance is part of the identity: pin 4 on one controller is unrelated
/// to pin 4 on another.
pub fn set_pin_enabled(instance: IoApicInstanceId, pin: IoApicPin, enabled: bool) {
    let Some(registry) = IO_APICS.get() else {
        return;
    };
    let mut registry = registry.lock();
    let Some(controller) = registry.controller_mut(instance) else {
        return;
    };
    if pin.as_usize() >= usize::from(controller.pin_count)
        || (enabled && !controller.configured_pins[pin.as_usize()])
    {
        return;
    }
    if enabled {
        // SAFETY: The pin is within the probed redirection table, and the
        // registry lock serializes the register-select/window accesses.
        unsafe { controller.io_apic.enable_irq(pin.as_u8()) };
    } else {
        // SAFETY: The pin is within the probed redirection table, and the
        // registry lock serializes masking.
        unsafe { controller.io_apic.disable_irq(pin.as_u8()) };
    }
}

/// Result of a level retarget after EOI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoApicRetargetResult {
    /// Destination was written and verified; delivery restoration followed the
    /// caller's logical/mask constraint.
    Complete,
    /// Remote IRR is still set. Destination was not changed; the caller keeps
    /// the request pending.
    Pending,
    /// A mask, write or readback failed after possible hardware mutation.
    Failed,
}

fn entry_accepts_physical_retarget(entry: &RedirectionTableEntry) -> bool {
    matches!(entry.mode(), IrqMode::Fixed) && !entry.flags().contains(IrqFlags::LOGICAL_DEST)
}

/// Checks that a pin is a physical Fixed RTE and can be retargeted.
///
/// This is a pre-write validation for a deferred affinity request. It does not
/// mask, write destination or claim that an active level source may move now.
pub fn retarget_supported(instance: IoApicInstanceId, pin: IoApicPin) -> bool {
    let Some(registry) = IO_APICS.get() else {
        return false;
    };
    let mut registry = registry.lock();
    let Some(controller) = registry.controller_mut(instance) else {
        return false;
    };
    if pin.as_usize() >= usize::from(controller.pin_count) {
        return false;
    }
    entry_accepts_physical_retarget(&controller.io_apic.read(pin.as_u8()))
}

/// Completes an edge destination transaction before EOI.
///
/// The RTE is masked across destination write/readback and restored according
/// to its original mask unless `keep_masked` is requested. The vector, trigger
/// and polarity bits are preserved. Returns `false` on any failed mask/write/
/// readback, leaving the entry at least requested-masked where possible.
pub fn retarget_edge(
    instance: IoApicInstanceId,
    pin: IoApicPin,
    destination: u8,
    keep_masked: bool,
) -> bool {
    let Some(registry) = IO_APICS.get() else {
        return false;
    };
    let mut registry = registry.lock();
    let Some(controller) = registry.controller_mut(instance) else {
        return false;
    };
    programming::retarget_edge(controller, pin, destination, keep_masked)
}

/// Masks a level pin before EOI and returns its original mask state.
///
/// `None` means the pin/entry is invalid or the mask write/readback failed.
/// The returned value is only a local transaction token; KIRQ owns the pending
/// affinity request itself.
pub fn prepare_level_retarget(instance: IoApicInstanceId, pin: IoApicPin) -> Option<bool> {
    let registry = IO_APICS.get()?;
    let mut registry = registry.lock();
    let controller = registry.controller_mut(instance)?;
    programming::prepare_retarget_mask(controller, pin)
}

/// Finishes a prepared level retarget after EOI.
///
/// Remote IRR set means the source is still active for the old destination;
/// destination is not changed and the caller retains the pending request.
pub fn finish_level_retarget(
    instance: IoApicInstanceId,
    pin: IoApicPin,
    destination: u8,
    was_masked: bool,
    keep_masked: bool,
) -> IoApicRetargetResult {
    let Some(registry) = IO_APICS.get() else {
        return IoApicRetargetResult::Failed;
    };
    let mut registry = registry.lock();
    let Some(controller) = registry.controller_mut(instance) else {
        return IoApicRetargetResult::Failed;
    };
    programming::finish_level(controller, pin, destination, was_masked, keep_masked)
}

/// Returns the programmed trigger mode of one input pin on one instance, or
/// `None` when the pin is outside that instance's physical table.
pub fn pin_trigger_mode(instance: IoApicInstanceId, pin: IoApicPin) -> Option<IoApicTriggerMode> {
    let registry = IO_APICS.get()?;
    let mut registry = registry.lock();
    let controller = registry.controller_mut(instance)?;
    if pin.as_usize() >= usize::from(controller.pin_count) {
        return None;
    }
    // SAFETY: The pin is within the probed redirection table, and the registry
    // lock serializes the register access.
    let flags = unsafe { controller.io_apic.table_entry(pin.as_u8()) }.flags();
    Some(if flags.contains(IrqFlags::LEVEL_TRIGGERED) {
        IoApicTriggerMode::Level
    } else {
        IoApicTriggerMode::Edge
    })
}

/// Maps and probes one IO-APIC without programming it.
///
/// The MMIO mapping is established and the implemented redirection-table size
/// is read. No redirection entry is written, so boot wiring can validate every
/// controller's range before the first instance is installed. `gsi_base` is
/// the controller's first firmware GSI and is retained with the probed pin
/// count for later GSI-to-pin translation.
/// Requires boot CPU context with IRQs and migration disabled.
///
/// # Errors
/// MMIO errors propagate as InvalidInput, Io or NoMemory.
pub fn probe_io_apic(io_apic_paddr: PhysAddr, gsi_base: u32) -> KResult<ProbedIoApic> {
    let io_apic_base = iomap_device(io_apic_paddr, 0x1000, "ioapic")?;
    // SAFETY: `io_apic_base` is the MMIO mapping for this controller and stays
    // mapped for the kernel lifetime.
    let mut io_apic = unsafe { IoApic::new(io_apic_base.as_usize() as u64) };
    // SAFETY: The newly mapped controller is exclusively owned during probe.
    let max_entry = unsafe { io_apic.max_table_entry() };
    let capabilities = IoApicCapabilities::from_max_table_entry(max_entry);
    info!(
        "IO-APIC supports {} IRQ inputs (0-{max_entry})",
        capabilities.pin_count
    );
    Ok(ProbedIoApic {
        io_apic,
        gsi_base,
        pin_count: capabilities.pin_count,
    })
}

/// Programs and publishes one probed IO-APIC instance.
///
/// Every implementable input pin is programmed as masked with no delivery
/// vector, using trigger and polarity defaults supplied by `policy`. A vector
/// is assigned when a route on that pin is configured. Instances are published
/// in installation order; that order is the instance identifier boot wiring
/// retains for later descriptor operations.
/// Requires boot CPU context with IRQs and migration disabled.
///
/// # Errors
/// Returns `Unsupported` when the instance array is full, and `InvalidInput`
/// when the probed pin count cannot be represented as a table index.
pub fn install_io_apic(
    probed: ProbedIoApic,
    policy: IoApicInitPolicy,
) -> KResult<IoApicInstanceId> {
    if let Some(registry) = IO_APICS.get() {
        return registry.lock().install(probed, policy);
    }
    let mut registry = IoApicRegistry::new();
    let instance = registry.install(probed, policy)?;
    IO_APICS.init_once(SpinNoIrq::new(registry));
    Ok(instance)
}

/// Resolves a delivery vector to the configured `(instance, pin)` route.
///
/// Returns `None` for vectors outside the IO-APIC window and for window
/// vectors that no configured pin owns.
pub fn io_apic_route(vector: usize) -> Option<(IoApicInstanceId, IoApicPin)> {
    IO_APICS.get()?.lock().vectors.route(vector)
}

fn initial_redirection_flags(pin: u8, policy: IoApicInitPolicy) -> IrqFlags {
    let level_low = policy
        .default_level_low_from
        .is_some_and(|from| pin >= from);
    if level_low {
        IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE | IrqFlags::MASKED
    } else {
        IrqFlags::MASKED
    }
}

fn program_initial_redirection_table(
    registers: &mut impl Registers,
    max_entry: u8,
    policy: IoApicInitPolicy,
) {
    if usize::from(max_entry) > IoApicPin::MAX {
        warn!(
            "IO-APIC reports {} pins; the backend can address only the first {}",
            u16::from(max_entry) + 1,
            MAX_IO_APIC_PINS
        );
    }
    for pin in 0..=max_entry {
        // Every hardware input must be suppressed during bring-up. Delivery
        // vectors are assigned per configured route, so an entry starts with
        // vector zero and stays masked: a masked entry cannot deliver the
        // null vector or collide with another instance's assigned vector.
        let mut entry = RedirectionTableEntry::default();
        entry.set_dest(0);
        entry.set_mode(IrqMode::Fixed);
        entry.set_flags(initial_redirection_flags(pin, policy));
        // Write access is bounded by the controller-reported table size and
        // goes through the IO-APIC register model.
        registers.write(pin, entry);
    }
}

unittest::path_mod_test!(tests, "../ut-tests/io_apic.rs");

/// Saved redirection state, allocated before entering the CPU/IRQ barrier.
/// Restore routing masked first, then release original masks after devices.
pub struct SavedIoApics {
    entries: alloc::vec::Vec<(usize, u8, RedirectionTableEntry)>,
    is_masked: bool,
}

impl SavedIoApics {
    /// Reserves storage without changing interrupt delivery.
    /// # Errors
    /// Returns NotFound before IO-APIC preparation has completed.
    pub fn prepare() -> KResult<Self> {
        let registry = IO_APICS.get().ok_or(KError::NotFound)?;
        let capacity = {
            let registry = registry.lock();
            registry
                .instances
                .iter()
                .flatten()
                .map(|c| c.pin_count as usize)
                .sum()
        };
        Ok(Self {
            entries: alloc::vec::Vec::with_capacity(capacity),
            is_masked: false,
        })
    }

    /// Captures and masks external interrupt routing without allocating.
    /// # Safety
    /// All other CPUs must be parked, local IRQs disabled and route mutations
    /// excluded until this object restores delivery. Storage must be prepared.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: prepare validated the permanent registry \
                  before the caller excluded route mutation"
    )]
    pub unsafe fn mask_all(&mut self) {
        assert!(!self.is_masked);
        let mut registry = IO_APICS.get().expect("prepared IO-APIC registry").lock();
        for (index, controller) in registry.instances.iter_mut().enumerate() {
            let Some(controller) = controller else {
                continue;
            };
            for pin in 0..controller.pin_count {
                let pin = pin as u8;
                // SAFETY: the registry owns the mapped controller and bounds
                // pin accesses by its probed count; the lock serializes access.
                let entry = unsafe { controller.io_apic.table_entry(pin) };
                assert!(self.entries.len() < self.entries.capacity());
                self.entries.push((index, pin, entry));
                // SAFETY: same mapped and bounded controller, now masked.
                unsafe { controller.io_apic.disable_irq(pin) };
            }
        }
        self.is_masked = true;
    }

    /// Reconstructs routing after reset while retaining interrupt exclusion.
    /// # Safety
    /// Controller mappings must be restored, local IRQs disabled, remote CPUs
    /// held, and external route mutations excluded.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: mask_all retained each installed \
                  controller under the caller's topology exclusion"
    )]
    pub unsafe fn restore_masked(&self) {
        assert!(self.is_masked);
        let mut registry = IO_APICS.get().expect("prepared IO-APIC registry").lock();
        for &(index, pin, mut entry) in &self.entries {
            entry.set_flags(entry.flags() | IrqFlags::MASKED);
            let controller = registry.instances[index].as_mut().expect("saved IO-APIC");
            // SAFETY: this transaction owns the same controller and saved pin.
            unsafe { controller.io_apic.set_table_entry(pin, entry) };
        }
    }

    /// Restores each pin's original mask after its consumers are safe.
    /// # Safety
    /// Device/controller/time recovery must be complete and all resources
    /// reachable from any newly enabled IRQ must be usable.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: the owned snapshot and caller's route \
                  exclusion retain the permanent registry and controllers"
    )]
    pub unsafe fn resume(&mut self) {
        if !self.is_masked {
            return;
        }
        let mut registry = IO_APICS.get().expect("prepared IO-APIC registry").lock();
        for &(index, pin, entry) in &self.entries {
            let controller = registry.instances[index].as_mut().expect("saved IO-APIC");
            // SAFETY: the caller completed recovery; this pin's original route
            // and mask are restored on the retained mapped controller.
            unsafe { controller.io_apic.set_table_entry(pin, entry) };
        }
        self.is_masked = false;
        self.entries.clear();
    }
}
