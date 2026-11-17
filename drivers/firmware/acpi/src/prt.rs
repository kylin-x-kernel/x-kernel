// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PCI root bridge `_PRT` and link `_CRS` query.
//!
//! The result is an owned, bounded routing table. No interpreter object or
//! namespace name is retained, so lookup stays valid after the [`AcpiContext`]
//! is dropped. Firmware link names are resolved and decoded during the query
//! and are only logged; they are not part of the returned data.

use alloc::vec::Vec;
use core::str::FromStr;

use acpi_rs::{
    Handler,
    aml::{
        AmlError,
        namespace::AmlName,
        object::Object,
        pci_routing::{PciRouteSource, PciRoutingTable},
        resource::{
            AddressSpaceResourceType, InterruptPolarity, InterruptTrigger, Resource,
            resource_descriptors,
        },
    },
};

use crate::{
    context::{AcpiContext, ContextError, IrqModel},
    host::{AmlHost, HostFailure},
};

/// Maximum routing entries retained in one [`PrtTable`].
pub const MAX_PRT_ENTRIES: usize = 256;
/// Maximum distinct firmware link names resolved by one query.
pub const MAX_LINKS: usize = 256;
/// Maximum resource descriptors accepted in a `_CRS` buffer.
pub const MAX_CRS_DESCRIPTORS: usize = 32;

/// Trigger and polarity of a PCI interrupt route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrtTrigger {
    /// Edge-sensitive, active-high.
    EdgeRising,
    /// Edge-sensitive, active-low.
    EdgeFalling,
    /// Level-sensitive, active-high.
    LevelHigh,
    /// Level-sensitive, active-low.
    LevelLow,
}

/// One resolved interrupt route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrtRoute {
    /// Global system interrupt number.
    pub gsi: u32,
    /// Trigger and polarity.
    pub trigger: PrtTrigger,
}

/// Why a `_PRT` key could not produce a route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrtLookupError {
    /// The lookup bus is not supported.
    Unsupported,
    /// The PCI pin number is outside `1..=4`.
    InvalidPin,
    /// The entry's source or source index is malformed.
    MalformedEntry,
    /// A link source used a non-zero source index.
    UnsupportedLinkIndex,
    /// The link's `_CRS` is absent, malformed, or does not name one IRQ.
    UnusableCrs,
}

/// Whether the root bridge provided `_PRT`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrtOrigin {
    /// `_PRT` was present and decoded.
    Present,
    /// `_PRT` was absent; the table is empty.
    Absent,
}

/// Failure to load the root bridge routing table.
#[derive(Debug)]
pub enum PrtError {
    /// `\_PIC(1)` has not selected the IO-APIC model.
    IrqModelNotInitialized,
    /// The host adapter recorded a failure.
    HostAccess(HostFailure),
    /// The interpreter rejected an unsupported operation-region space.
    UnsupportedRegion(u8),
    /// The context is poisoned by an earlier failure.
    Poisoned,
    /// Only PCI segment zero, bus zero is supported here.
    UnsupportedBus,
    /// No PCI root bridge was found in the ACPI namespace.
    NoRootBridge,
    /// More than one PCI root bridge was found.
    MultipleRootBridges,
    /// The root bridge identifies an unsupported segment or bus range.
    UnsupportedRootBridge,
    /// The root bridge `_CRS` buffer is malformed.
    MalformedRootCrs,
    /// The `_PRT` object is structurally malformed.
    MalformedPrt,
    /// Evaluating the `_PRT` object failed.
    PrtEvaluation(AmlError),
    /// Another context operation failed.
    Context(ContextError),
    /// The `_PRT` object exceeds [`MAX_PRT_ENTRIES`].
    TableTooLarge,
    /// The query exceeded [`MAX_LINKS`] distinct links.
    TooManyLinks,
}

impl From<ContextError> for PrtError {
    fn from(error: ContextError) -> Self {
        match error {
            ContextError::HostAccess(failure) => Self::HostAccess(failure),
            ContextError::UnsupportedRegion(space) => Self::UnsupportedRegion(space),
            ContextError::Poisoned => Self::Poisoned,
            other => Self::Context(other),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PrtEntry {
    device: u16,
    pin: u8,
    route: Result<PrtRoute, PrtLookupError>,
}

/// A bounded, owned PCI routing table.
#[derive(Clone, Copy)]
pub struct PrtTable {
    entries: [Option<PrtEntry>; MAX_PRT_ENTRIES],
    len: usize,
    origin: PrtOrigin,
}

impl PrtTable {
    fn new(origin: PrtOrigin) -> Self {
        Self {
            entries: [None; MAX_PRT_ENTRIES],
            len: 0,
            origin,
        }
    }

    /// Looks up the route for one PCI device and pin.
    ///
    /// The ACPI `_PRT` address function is intentionally not part of the key:
    /// the specification aligns with Linux, which matches by device and pin
    /// and ignores the function when selecting a route. `function` is accepted
    /// so callers can share one PCI address type; it does not narrow the match.
    ///
    /// `pin` is the one-based PCI interrupt pin (`1` = INTA, `4` = INTD).
    ///
    /// # Errors
    ///
    /// Returns [`PrtLookupError::Unsupported`] for a non-zero bus,
    /// [`PrtLookupError::InvalidPin`] for `pin > 4`, or the stored reason when a
    /// matching key exists but could not be decoded.
    pub fn lookup(
        &self,
        bus: u8,
        device: u8,
        function: u8,
        pin: u8,
    ) -> Result<Option<PrtRoute>, PrtLookupError> {
        if bus != 0 {
            return Err(PrtLookupError::Unsupported);
        }
        if pin == 0 || pin == 0xff {
            return Ok(None);
        }
        if pin > 4 {
            return Err(PrtLookupError::InvalidPin);
        }
        let _ = function;
        let acpi_pin = pin - 1;

        for entry in self.entries[..self.len].iter().flatten() {
            if entry.device == u16::from(device) && entry.pin == acpi_pin {
                return match entry.route {
                    Ok(route) => Ok(Some(route)),
                    Err(error) => Err(error),
                };
            }
        }
        Ok(None)
    }

    /// Returns whether the root bridge supplied `_PRT`.
    pub const fn origin(&self) -> PrtOrigin {
        self.origin
    }

    /// Returns the number of retained entries.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no entries are retained.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, entry: PrtEntry) {
        if self
            .entries
            .iter()
            .flatten()
            .any(|existing| existing.device == entry.device && existing.pin == entry.pin)
        {
            log::warn!(
                "ACPI: duplicate _PRT entry for device {:#06x} pin {}; keeping the first",
                entry.device,
                entry.pin
            );
            return;
        }
        if self.len >= MAX_PRT_ENTRIES {
            return;
        }
        self.entries[self.len] = Some(entry);
        self.len += 1;
    }
}

/// Loads the routing table of the single PCI segment-0 root bridge.
///
/// `_PIC(1)` must already have been executed through
/// [`AcpiContext::init_irq_model`]. This function never evaluates `_PIC`
/// itself.
///
/// # Errors
///
/// Returns a whole-load error when the context is not initialized, the root
/// bridge cannot be identified or validated, `_PRT` is structurally malformed,
/// or the host records an access failure. Per-entry source and link failures
/// are stored in the returned table and reported by [`PrtTable::lookup`].
pub fn load<H: Handler + AmlHost>(ctx: &mut AcpiContext<H>, bus: u8) -> Result<PrtTable, PrtError> {
    ctx.check_host()?;
    if ctx.irq_model() != Some(IrqModel::IoApic) {
        return Err(PrtError::IrqModelNotInitialized);
    }
    if bus != 0 {
        return Err(PrtError::UnsupportedBus);
    }

    let root = find_root_bridge(ctx)?;
    log::info!("ACPI: PCI root bridge {root}");
    validate_root_bridge(ctx, &root)?;

    let prt_path = child_path(ctx, &root, "_PRT")?;
    let firmware_table = match ctx.evaluate_if_present(&prt_path, Vec::new(), |object| {
        PciRoutingTable::from_object(object)
    }) {
        Ok(Some(Ok(table))) => table,
        Ok(Some(Err(error))) => {
            log::warn!("ACPI: PCI root bridge {root} _PRT is malformed: {error:?}");
            return Err(PrtError::MalformedPrt);
        }
        Ok(None) => {
            log::warn!("ACPI: PCI root bridge {root} has no _PRT");
            return Ok(PrtTable::new(PrtOrigin::Absent));
        }
        Err(error) => return Err(evaluation_error(error)),
    };

    if firmware_table.entries().len() > MAX_PRT_ENTRIES {
        return Err(PrtError::TableTooLarge);
    }

    let mut table = PrtTable::new(PrtOrigin::Present);
    let mut links: Vec<(AmlName, Result<PrtRoute, PrtLookupError>)> = Vec::new();
    for entry in firmware_table.entries() {
        let route = match entry.source() {
            Ok(PciRouteSource::Gsi(gsi)) => Ok(PrtRoute {
                gsi: *gsi,
                trigger: PrtTrigger::LevelLow,
            }),
            Ok(PciRouteSource::Link { name, index }) => {
                if *index == 0 {
                    resolve_link(ctx, &prt_path, name, &mut links)?
                } else {
                    log::warn!("ACPI: _PRT link {name} uses unsupported source index {index}");
                    Err(PrtLookupError::UnsupportedLinkIndex)
                }
            }
            // Every source-field decode failure makes this entry unusable, but
            // the `(device, pin)` key it was found under is still valid.
            Err(_) => Err(PrtLookupError::MalformedEntry),
        };
        log_entry(entry.device(), entry.function(), entry.pin() as u8, route);
        table.push(PrtEntry {
            device: entry.device(),
            pin: entry.pin() as u8,
            route,
        });
    }

    Ok(table)
}

/// Locates the only namespace device that identifies a PCI root bridge.
fn find_root_bridge<H: Handler + AmlHost>(ctx: &mut AcpiContext<H>) -> Result<AmlName, PrtError> {
    let mut root = None;
    for path in ctx.device_paths()? {
        if !has_root_bridge_id(ctx, &path)? {
            continue;
        }
        if root.is_some() {
            return Err(PrtError::MultipleRootBridges);
        }
        root = Some(path);
    }
    root.ok_or(PrtError::NoRootBridge)
}

/// Probes `_HID` and `_CID` for a PNP0A03/PNP0A08 identity.
fn has_root_bridge_id<H: Handler + AmlHost>(
    ctx: &mut AcpiContext<H>,
    path: &AmlName,
) -> Result<bool, PrtError> {
    for attribute in ["_HID", "_CID"] {
        let name = child_path(ctx, path, attribute)?;
        match ctx.evaluate_if_present(&name, Vec::new(), root_bridge_id) {
            Ok(Some(true)) => return Ok(true),
            Ok(_) => {}
            Err(error) if is_fatal_context(&error) => return Err(error.into()),
            Err(error) => log::debug!("ACPI: {path}.{attribute} probe failed: {error:?}"),
        }
    }
    Ok(false)
}

/// Returns whether an object contains a PNP0A03 or PNP0A08 identifier.
fn root_bridge_id(object: &Object) -> bool {
    match object {
        Object::String(id) => is_root_bridge_string(id),
        Object::Integer(id) => is_root_bridge_eisa(*id),
        Object::Package(items) => items.iter().any(|item| match &**item {
            Object::String(id) => is_root_bridge_string(id),
            Object::Integer(id) => is_root_bridge_eisa(*id),
            _ => false,
        }),
        _ => false,
    }
}

fn is_root_bridge_string(id: &str) -> bool {
    id == "PNP0A03" || id == "PNP0A08"
}

fn is_root_bridge_eisa(id: u64) -> bool {
    // Little-endian EISAID byte order as stored in an AML DWord constant:
    // PNP0A03 and PNP0A08.
    id == 0x030A_D041 || id == 0x080A_D041
}

/// Checks `_SEG`, `_CRS`, and `_BBN` before `_PRT` is trusted.
fn validate_root_bridge<H: Handler + AmlHost>(
    ctx: &mut AcpiContext<H>,
    root: &AmlName,
) -> Result<(), PrtError> {
    let seg_path = child_path(ctx, root, "_SEG")?;
    match evaluate_integer(ctx, &seg_path) {
        Ok(None | Some(Ok(0))) => {}
        Ok(Some(Ok(seg))) => {
            log::warn!("ACPI: PCI root bridge {root} segment {seg} is not supported");
            return Err(PrtError::UnsupportedRootBridge);
        }
        Ok(Some(Err(()))) => {
            log::warn!("ACPI: PCI root bridge {root} _SEG is not an integer");
            return Err(PrtError::UnsupportedRootBridge);
        }
        Err(error) if is_fatal_context(&error) => return Err(error.into()),
        Err(error) => {
            log::warn!("ACPI: PCI root bridge {root} _SEG is unusable: {error:?}");
            return Err(PrtError::UnsupportedRootBridge);
        }
    }

    let mut has_bus_range = false;
    let crs_path = child_path(ctx, root, "_CRS")?;
    match ctx.evaluate_if_present(&crs_path, Vec::new(), |object| match object {
        Object::Buffer(bytes) => Ok(bytes.clone()),
        _ => Err(()),
    }) {
        Ok(None) => {}
        Ok(Some(Ok(bytes))) => match decode_bus_range(&bytes) {
            Ok(None) => {}
            Ok(Some((min, max))) => {
                if min != 0 {
                    log::warn!(
                        "ACPI: PCI root bridge {root} bus range {min:#x}-{max:#x} does not start \
                         at 0"
                    );
                    return Err(PrtError::UnsupportedRootBridge);
                }
                log::info!("ACPI: PCI root bridge {root} bus range {min:#x}-{max:#x}");
                has_bus_range = true;
            }
            Err(error) => {
                log::warn!("ACPI: PCI root bridge {root} _CRS is malformed: {error:?}");
                return Err(PrtError::MalformedRootCrs);
            }
        },
        Ok(Some(Err(()))) => {
            log::warn!("ACPI: PCI root bridge {root} _CRS is not a buffer");
            return Err(PrtError::MalformedRootCrs);
        }
        Err(error) if is_fatal_context(&error) => return Err(error.into()),
        Err(error) => {
            log::warn!("ACPI: PCI root bridge {root} _CRS evaluation failed: {error:?}");
            return Err(PrtError::MalformedRootCrs);
        }
    }

    if has_bus_range {
        return Ok(());
    }
    let bbn_path = child_path(ctx, root, "_BBN")?;
    match evaluate_integer(ctx, &bbn_path) {
        Ok(None | Some(Ok(0))) => Ok(()),
        Ok(Some(Ok(bbn))) => {
            log::warn!("ACPI: PCI root bridge {root} _BBN {bbn} is not supported");
            Err(PrtError::UnsupportedRootBridge)
        }
        Ok(Some(Err(()))) => {
            log::warn!("ACPI: PCI root bridge {root} _BBN is not an integer");
            Err(PrtError::UnsupportedRootBridge)
        }
        Err(error) if is_fatal_context(&error) => Err(error.into()),
        Err(error) => {
            log::warn!("ACPI: PCI root bridge {root} _BBN is unusable: {error:?}");
            Err(PrtError::UnsupportedRootBridge)
        }
    }
}

/// Evaluates an optional integer, distinguishing absence from a wrong type.
fn evaluate_integer<H: Handler + AmlHost>(
    ctx: &mut AcpiContext<H>,
    path: &AmlName,
) -> Result<Option<Result<u64, ()>>, ContextError> {
    ctx.evaluate_if_present(path, Vec::new(), |object| match object {
        Object::Integer(value) => Ok(*value),
        _ => Err(()),
    })
}

fn is_fatal_context(error: &ContextError) -> bool {
    matches!(
        error,
        ContextError::HostAccess(_) | ContextError::UnsupportedRegion(_) | ContextError::Poisoned
    )
}

fn evaluation_error(error: ContextError) -> PrtError {
    match error {
        ContextError::Aml(error) => PrtError::PrtEvaluation(error),
        other => other.into(),
    }
}

fn child_path<H: Handler + AmlHost>(
    ctx: &mut AcpiContext<H>,
    scope: &AmlName,
    segment: &str,
) -> Result<AmlName, PrtError> {
    let name = AmlName::from_str(segment).map_err(PrtError::PrtEvaluation)?;
    ctx.resolve_name(scope, &name).map_err(PrtError::from)
}

/// Resolves and evaluates one link source, caching by absolute link name.
fn resolve_link<H: Handler + AmlHost>(
    ctx: &mut AcpiContext<H>,
    prt_path: &AmlName,
    source: &AmlName,
    cache: &mut Vec<(AmlName, Result<PrtRoute, PrtLookupError>)>,
) -> Result<Result<PrtRoute, PrtLookupError>, PrtError> {
    let resolved = match ctx.search_for_level(source, prt_path) {
        Ok(resolved) => resolved,
        Err(error) if is_fatal_context(&error) => return Err(error.into()),
        Err(error) => {
            log::warn!("ACPI: _PRT link {source} does not resolve: {error:?}");
            return Ok(Err(PrtLookupError::UnusableCrs));
        }
    };

    if let Some((_, route)) = cache.iter().find(|(name, _)| *name == resolved) {
        return Ok(*route);
    }
    if cache.len() >= MAX_LINKS {
        return Err(PrtError::TooManyLinks);
    }

    let route = evaluate_link_crs(ctx, &resolved)?;
    cache.push((resolved, route));
    Ok(route)
}

/// Evaluates a resolved link's `_CRS` and extracts its single IRQ.
fn evaluate_link_crs<H: Handler + AmlHost>(
    ctx: &mut AcpiContext<H>,
    link: &AmlName,
) -> Result<Result<PrtRoute, PrtLookupError>, PrtError> {
    let crs_path = match child_path(ctx, link, "_CRS") {
        Ok(path) => path,
        Err(error) if is_fatal_prt(&error) => return Err(error),
        Err(error) => {
            log::warn!("ACPI: link {link} _CRS path is unusable: {error:?}");
            return Ok(Err(PrtLookupError::UnusableCrs));
        }
    };

    let crs = match ctx.evaluate_if_present(&crs_path, Vec::new(), |object| match object {
        Object::Buffer(bytes) => Some(bytes.clone()),
        _ => None,
    }) {
        Ok(Some(Some(bytes))) => bytes,
        Ok(Some(None)) | Ok(None) => {
            log::warn!("ACPI: link {link} has no buffer _CRS");
            return Ok(Err(PrtLookupError::UnusableCrs));
        }
        Err(error) if is_fatal_context(&error) => return Err(error.into()),
        Err(error) => {
            log::warn!("ACPI: link {link} _CRS evaluation failed: {error:?}");
            return Ok(Err(PrtLookupError::UnusableCrs));
        }
    };

    match decode_link_irq(&crs) {
        Ok(irq) => {
            log::info!(
                "ACPI: link {link} -> GSI {}, trigger {:?}, shared={}, wake={}, consumer={}",
                irq.gsi,
                irq.trigger,
                irq.shared,
                irq.wake,
                irq.consumer
            );
            Ok(Ok(PrtRoute {
                gsi: irq.gsi,
                trigger: irq.trigger,
            }))
        }
        Err(error) => {
            log::warn!("ACPI: link {link} _CRS is unusable: {error:?}");
            Ok(Err(PrtLookupError::UnusableCrs))
        }
    }
}

/// Whether a `PrtError` is a fatal context failure that must abort the load.
fn is_fatal_prt(error: &PrtError) -> bool {
    matches!(
        error,
        PrtError::HostAccess(_) | PrtError::UnsupportedRegion(_) | PrtError::Poisoned
    )
}

fn log_entry(device: u16, function: u16, pin: u8, route: Result<PrtRoute, PrtLookupError>) {
    if function == u16::MAX {
        match route {
            Ok(route) => log::info!(
                "ACPI: _PRT {device:#06x}.* INTA+{pin} -> GSI {}, trigger {:?}",
                route.gsi,
                route.trigger
            ),
            Err(error) => {
                log::warn!("ACPI: _PRT {device:#06x}.* INTA+{pin} is unusable: {error:?}")
            }
        }
    } else {
        match route {
            Ok(route) => log::info!(
                "ACPI: _PRT {device:#06x}.{function} INTA+{pin} -> GSI {}, trigger {:?}",
                route.gsi,
                route.trigger
            ),
            Err(error) => {
                log::warn!("ACPI: _PRT {device:#06x}.{function} INTA+{pin} is unusable: {error:?}")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinkIrq {
    gsi: u32,
    trigger: PrtTrigger,
    shared: bool,
    wake: bool,
    consumer: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrsError {
    Malformed,
    MissingEndTag,
    TooManyDescriptors,
    NoIrq,
    MultipleIrqs,
    UnsupportedSource,
    MultipleBusRanges,
    BadBusRange,
}

/// Decodes the single IRQ named by a link `_CRS` buffer.
///
/// Descriptor framing and fields are decoded by the vendor AML resource layer.
/// This function applies only the subsystem's support scope: at most
/// [`MAX_CRS_DESCRIPTORS`] framed items, a terminating end tag, exactly one IRQ
/// number, and no `ResourceSource` suffix (following the source device is not
/// implemented).
fn decode_link_irq(bytes: &[u8]) -> Result<LinkIrq, CrsError> {
    let mut found: Option<LinkIrq> = None;
    let mut count = 0usize;
    let mut descriptors = resource_descriptors(bytes);
    for descriptor in descriptors.by_ref() {
        count += 1;
        if count > MAX_CRS_DESCRIPTORS {
            return Err(CrsError::TooManyDescriptors);
        }
        let descriptor = descriptor.map_err(|_| CrsError::Malformed)?;
        let Some(Resource::Irq(irq)) = descriptor.decode().map_err(|_| CrsError::Malformed)? else {
            continue;
        };
        // The descriptor is valid, but this subsystem does not follow a
        // `ResourceSource` to another device's resource template. Reject the
        // link as unsupported policy instead of treating it as malformed.
        if irq.source.is_some() {
            return Err(CrsError::UnsupportedSource);
        }
        if found.is_some() || irq.irqs.len() != 1 {
            return Err(CrsError::MultipleIrqs);
        }
        let Some(gsi) = irq.irqs.first() else {
            return Err(CrsError::NoIrq);
        };
        found = Some(LinkIrq {
            gsi: *gsi,
            trigger: trigger_of(irq.trigger, irq.polarity),
            shared: irq.is_shared,
            wake: irq.is_wake_capable,
            consumer: irq.is_consumer,
        });
    }
    if !descriptors.end_tag_seen() {
        return Err(CrsError::MissingEndTag);
    }
    found.ok_or(CrsError::NoIrq)
}

const fn trigger_of(trigger: InterruptTrigger, polarity: InterruptPolarity) -> PrtTrigger {
    match (trigger, polarity) {
        (InterruptTrigger::Edge, InterruptPolarity::ActiveHigh) => PrtTrigger::EdgeRising,
        (InterruptTrigger::Edge, InterruptPolarity::ActiveLow) => PrtTrigger::EdgeFalling,
        (InterruptTrigger::Level, InterruptPolarity::ActiveLow) => PrtTrigger::LevelLow,
        (InterruptTrigger::Level, InterruptPolarity::ActiveHigh) => PrtTrigger::LevelHigh,
    }
}

/// Finds the bus-number range of the root bridge `_CRS`.
///
/// Address-field widths and offsets are decoded by the vendor AML resource
/// layer. This function applies the descriptor-count limit and rejects more
/// than one bus-number range.
fn decode_bus_range(bytes: &[u8]) -> Result<Option<(u32, u32)>, CrsError> {
    let mut found = None;
    let mut count = 0usize;
    let mut descriptors = resource_descriptors(bytes);
    for descriptor in descriptors.by_ref() {
        count += 1;
        if count > MAX_CRS_DESCRIPTORS {
            return Err(CrsError::TooManyDescriptors);
        }
        let descriptor = descriptor.map_err(|_| CrsError::Malformed)?;
        let Some(Resource::AddressSpace(info)) =
            descriptor.decode().map_err(|_| CrsError::Malformed)?
        else {
            continue;
        };
        if info.resource_type != AddressSpaceResourceType::BusNumberRange {
            continue;
        }
        let min = u32::try_from(info.address_range.0).map_err(|_| CrsError::BadBusRange)?;
        let max = u32::try_from(info.address_range.1).map_err(|_| CrsError::BadBusRange)?;
        if min > max {
            return Err(CrsError::BadBusRange);
        }
        if found.is_some() {
            return Err(CrsError::MultipleBusRanges);
        }
        found = Some((min, max));
    }
    if !descriptors.end_tag_seen() {
        return Err(CrsError::MissingEndTag);
    }
    Ok(found)
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/acpi_prt.rs");
