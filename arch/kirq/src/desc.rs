// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS-visible IRQ descriptors.

use bitflags::bitflags;

/// Interrupt trigger mode understood by the generic IRQ core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqTrigger {
    EdgeRising,
    EdgeFalling,
    LevelHigh,
    LevelLow,
    /// Trigger mode not described by firmware; carries preserved raw flag bits.
    Unknown(u32),
}

/// Interrupt controller family for IRQ descriptor normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqController {
    Gic,
    Plic,
    IoApic,
    Msi,
    LoongArchExtioi,
    /// Controller not described by firmware or currently unknown.
    Unknown,
}

/// Opaque identifier for an IRQ domain managed by the kernel IRQ core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IrqDomainId(u32);

impl IrqDomainId {
    /// Creates an IRQ domain identifier from its stable numeric id.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// Returns the stable numeric id.
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Outcome reported by an IRQ handler.
///
/// The source bitmap is interpreted by higher layers such as the devres-backed
/// driver framework. `kirq` only uses `handled()` to distinguish claimed work
/// from a shared-line miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqEvent {
    handled: bool,
    sources: u8,
}

impl IrqEvent {
    /// The handler claimed the interrupt but reports no specific source.
    pub const HANDLED: Self = Self {
        handled: true,
        sources: 0,
    };
    /// The handler did not claim the interrupt.
    pub const NOT_HANDLED: Self = Self {
        handled: false,
        sources: 0,
    };

    /// Claim the interrupt with a caller-defined source bitmap.
    pub const fn from_sources(sources: u8) -> Self {
        Self {
            handled: true,
            sources,
        }
    }

    /// Returns whether this handler claimed and serviced the interrupt.
    pub const fn handled(&self) -> bool {
        self.handled
    }

    /// Returns the raw caller-defined source bitmap.
    pub const fn sources(&self) -> u8 {
        self.sources
    }

    /// Combines another IRQ event into this one.
    pub fn merge(&mut self, other: IrqEvent) {
        self.handled |= other.handled;
        self.sources |= other.sources;
    }
}

/// A kernel IRQ handler.
///
/// Handlers run in interrupt context and must not sleep. Any
/// `Fn() -> IrqEvent + Send + Sync` implements this trait.
pub trait IrqHandler: Send + Sync {
    /// Service a fired interrupt and report whether it was handled.
    fn handle(&self) -> IrqEvent;
}

impl<F> IrqHandler for F
where
    F: Fn() -> IrqEvent + Send + Sync,
{
    fn handle(&self) -> IrqEvent {
        self()
    }
}

/// OS-visible logical interrupt number managed by `kirq`.
pub type Virq = usize;

/// Controller-local hardware interrupt number.
pub type Hwirq = usize;

/// Firmware or platform source that described an interrupt resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqSource {
    DeviceTree,
    Acpi,
    PlatformStatic,
    Unknown,
}

pub const GIC_ROOT_DOMAIN: IrqDomainId = IrqDomainId::new(1);
pub const PLIC_ROOT_DOMAIN: IrqDomainId = IrqDomainId::new(2);
pub const IO_APIC_DOMAIN: IrqDomainId = IrqDomainId::new(3);
pub const MSI_DOMAIN: IrqDomainId = IrqDomainId::new(4);

/// Signal polarity described for an interrupt resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqPolarity {
    High,
    Low,
    Unknown,
}

/// Returns the implied signal polarity when one is encoded by the trigger.
pub const fn trigger_polarity(trigger: IrqTrigger) -> IrqPolarity {
    match trigger {
        IrqTrigger::EdgeRising | IrqTrigger::LevelHigh => IrqPolarity::High,
        IrqTrigger::EdgeFalling | IrqTrigger::LevelLow => IrqPolarity::Low,
        IrqTrigger::Unknown(_) => IrqPolarity::Unknown,
    }
}

bitflags! {
    /// OS-visible IRQ resource properties that are independent from any
    /// concrete controller implementation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct IrqFlags: u32 {
        const SHARED = 1 << 0;
        const WAKEUP_SOURCE = 1 << 1;
        const PER_CPU = 1 << 2;
        const MSI = 1 << 3;
    }
}

/// CPU targeting policy attached to an IRQ resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqAffinity {
    Any,
    Cpu(usize),
}

/// Normalized interrupt resource description passed between firmware parsing,
/// platform wiring and higher-level IRQ setup code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqDesc {
    pub virq: Option<Virq>,
    pub hwirq: Hwirq,
    pub trigger: IrqTrigger,
    pub polarity: IrqPolarity,
    pub source: IrqSource,
    pub controller: IrqController,
    pub domain: Option<IrqDomainId>,
    pub affinity: IrqAffinity,
    pub flags: IrqFlags,
}

/// Descriptor merge or mapping conflict detected by the IRQ core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqDescError {
    /// Two descriptors for the same logical IRQ name different hardware IRQs.
    HwirqConflict { existing: Hwirq, newer: Hwirq },
    /// Two descriptors name incompatible IRQ domains.
    DomainConflict {
        existing: Option<IrqDomainId>,
        newer: Option<IrqDomainId>,
    },
    /// Two descriptors name incompatible logical IRQs.
    VirqConflict {
        existing: Option<Virq>,
        newer: Option<Virq>,
    },
    /// A domain/hwirq mapping already points at a different logical IRQ.
    MappingConflict {
        domain: IrqDomainId,
        hwirq: Hwirq,
        existing: Virq,
        newer: Virq,
    },
    /// Dynamic logical IRQ allocation reached the representable IRQ limit.
    VirqExhausted { next: Virq },
    /// The descriptor names an IRQ domain that is not registered in kirq.
    UnknownDomain { domain: IrqDomainId },
}

impl IrqDesc {
    /// Creates a descriptor with unknown source/controller metadata.
    pub const fn new(hwirq: Hwirq, trigger: IrqTrigger) -> Self {
        Self {
            virq: None,
            hwirq,
            trigger,
            polarity: trigger_polarity(trigger),
            source: IrqSource::Unknown,
            controller: IrqController::Unknown,
            domain: None,
            affinity: IrqAffinity::Any,
            flags: IrqFlags::empty(),
        }
    }

    /// Creates a descriptor when only the hardware IRQ number is currently known.
    pub const fn from_hwirq(hwirq: Hwirq) -> Self {
        Self::new(hwirq, IrqTrigger::Unknown(0))
    }

    /// Creates a descriptor when only the logical IRQ number is currently known.
    pub const fn from_virq(virq: Virq) -> Self {
        Self {
            virq: Some(virq),
            hwirq: virq,
            trigger: IrqTrigger::Unknown(0),
            polarity: IrqPolarity::Unknown,
            source: IrqSource::Unknown,
            controller: IrqController::Unknown,
            domain: None,
            affinity: IrqAffinity::Any,
            flags: IrqFlags::empty(),
        }
    }

    /// Marks where this descriptor came from.
    pub const fn with_source(self, source: IrqSource) -> Self {
        Self { source, ..self }
    }

    /// Marks which controller family owns this descriptor.
    pub const fn with_controller(self, controller: IrqController) -> Self {
        Self { controller, ..self }
    }

    /// Marks the IRQ domain associated with this descriptor.
    pub const fn with_domain(self, domain: IrqDomainId) -> Self {
        Self {
            domain: Some(domain),
            ..self
        }
    }

    /// Overrides the polarity metadata for this descriptor.
    pub const fn with_polarity(self, polarity: IrqPolarity) -> Self {
        Self { polarity, ..self }
    }

    /// Marks CPU affinity for this IRQ resource.
    pub const fn with_affinity(self, affinity: IrqAffinity) -> Self {
        Self { affinity, ..self }
    }

    /// Pins this descriptor to a pre-existing logical IRQ number.
    pub const fn with_virq(self, virq: Virq) -> Self {
        Self {
            virq: Some(virq),
            ..self
        }
    }

    /// Adds resource flags while preserving existing ones.
    pub fn with_flags(mut self, flags: IrqFlags) -> Self {
        self.flags |= flags;
        self
    }

    /// Returns the logical IRQ number when it is already known.
    pub const fn logical_irq(self) -> Option<Virq> {
        self.virq
    }

    /// Try to merge newer descriptor metadata into this descriptor.
    pub fn try_merge(self, newer: Self) -> Result<Self, IrqDescError> {
        if self.hwirq != newer.hwirq {
            return Err(IrqDescError::HwirqConflict {
                existing: self.hwirq,
                newer: newer.hwirq,
            });
        }
        if self.domain != newer.domain && self.domain.is_some() && newer.domain.is_some() {
            return Err(IrqDescError::DomainConflict {
                existing: self.domain,
                newer: newer.domain,
            });
        }
        if self.virq != newer.virq && self.virq.is_some() && newer.virq.is_some() {
            return Err(IrqDescError::VirqConflict {
                existing: self.virq,
                newer: newer.virq,
            });
        }
        Ok(Self {
            virq: newer.virq.or(self.virq),
            hwirq: self.hwirq,
            trigger: match newer.trigger {
                IrqTrigger::Unknown(_) => self.trigger,
                _ => newer.trigger,
            },
            polarity: match newer.polarity {
                IrqPolarity::Unknown => self.polarity,
                _ => newer.polarity,
            },
            source: match newer.source {
                IrqSource::Unknown => self.source,
                _ => newer.source,
            },
            controller: match newer.controller {
                IrqController::Unknown => self.controller,
                _ => newer.controller,
            },
            domain: newer.domain.or(self.domain),
            affinity: match newer.affinity {
                IrqAffinity::Any => self.affinity,
                _ => newer.affinity,
            },
            flags: self.flags | newer.flags,
        })
    }
}

pub const fn gic_irq_desc(hwirq: Hwirq, trigger: IrqTrigger) -> IrqDesc {
    IrqDesc::new(hwirq, trigger)
        .with_controller(IrqController::Gic)
        .with_domain(GIC_ROOT_DOMAIN)
}

pub const fn gic_level_irq_desc(hwirq: Hwirq) -> IrqDesc {
    gic_irq_desc(hwirq, IrqTrigger::LevelHigh)
}

pub const fn gic_edge_irq_desc(hwirq: Hwirq) -> IrqDesc {
    gic_irq_desc(hwirq, IrqTrigger::EdgeRising)
}

pub const fn plic_irq_desc(hwirq: Hwirq) -> IrqDesc {
    IrqDesc::from_hwirq(hwirq)
        .with_controller(IrqController::Plic)
        .with_domain(PLIC_ROOT_DOMAIN)
}

pub const fn io_apic_irq_desc(hwirq: Hwirq) -> IrqDesc {
    IrqDesc::from_hwirq(hwirq)
        .with_controller(IrqController::IoApic)
        .with_domain(IO_APIC_DOMAIN)
}

impl From<usize> for IrqDesc {
    fn from(value: usize) -> Self {
        Self::from_virq(value)
    }
}

/// Helper trait so IRQ APIs can take either a plain IRQ number or a full
/// descriptor. New code should prefer passing [`IrqDesc`] when metadata such as
/// trigger mode is available.
pub trait IntoIrqDesc {
    fn into_irq_desc(self) -> IrqDesc;
}

impl IntoIrqDesc for usize {
    fn into_irq_desc(self) -> IrqDesc {
        IrqDesc::from_virq(self)
    }
}

impl IntoIrqDesc for IrqDesc {
    fn into_irq_desc(self) -> IrqDesc {
        self
    }
}

impl IntoIrqDesc for &IrqDesc {
    fn into_irq_desc(self) -> IrqDesc {
        *self
    }
}
