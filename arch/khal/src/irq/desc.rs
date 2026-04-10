// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS-visible IRQ descriptors.

use bitflags::bitflags;

/// OS-visible logical interrupt number managed by `khal::irq`.
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

/// IRQ domain identifier used to distinguish logical interrupt namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IrqDomainId(pub u32);

pub const GIC_ROOT_DOMAIN: IrqDomainId = IrqDomainId(1);
pub const PLIC_ROOT_DOMAIN: IrqDomainId = IrqDomainId(2);
pub const IO_APIC_DOMAIN: IrqDomainId = IrqDomainId(3);

/// Interrupt controller family associated with an IRQ resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqControllerKind {
    Gic,
    IoApic,
    Plic,
    LoongArchExtioi,
    Unknown,
}

/// Signal polarity described for an interrupt resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqPolarity {
    High,
    Low,
    Unknown,
}

/// Trigger semantics described for an interrupt resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqTrigger {
    EdgeRising,
    EdgeFalling,
    LevelHigh,
    LevelLow,
    Unknown(u32),
}

impl IrqTrigger {
    /// Returns whether the interrupt trigger is edge-triggered.
    pub const fn is_edge(self) -> bool {
        matches!(self, Self::EdgeRising | Self::EdgeFalling)
    }

    /// Returns the implied signal polarity when one is encoded by the trigger.
    pub const fn polarity(self) -> IrqPolarity {
        match self {
            Self::EdgeRising | Self::LevelHigh => IrqPolarity::High,
            Self::EdgeFalling | Self::LevelLow => IrqPolarity::Low,
            Self::Unknown(_) => IrqPolarity::Unknown,
        }
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
    pub controller: IrqControllerKind,
    pub domain: Option<IrqDomainId>,
    pub affinity: IrqAffinity,
    pub flags: IrqFlags,
}

impl IrqDesc {
    /// Creates a descriptor with unknown source/controller metadata.
    pub const fn new(hwirq: Hwirq, trigger: IrqTrigger) -> Self {
        Self {
            virq: None,
            hwirq,
            trigger,
            polarity: trigger.polarity(),
            source: IrqSource::Unknown,
            controller: IrqControllerKind::Unknown,
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
            controller: IrqControllerKind::Unknown,
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
    pub const fn with_controller(self, controller: IrqControllerKind) -> Self {
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

    /// Merge newer descriptor metadata into this descriptor.
    pub fn merge(self, newer: Self) -> Self {
        debug_assert_eq!(self.hwirq, newer.hwirq);
        debug_assert!(
            self.domain == newer.domain || self.domain.is_none() || newer.domain.is_none()
        );
        debug_assert!(self.virq == newer.virq || self.virq.is_none() || newer.virq.is_none());
        Self {
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
                IrqControllerKind::Unknown => self.controller,
                _ => newer.controller,
            },
            domain: newer.domain.or(self.domain),
            affinity: match newer.affinity {
                IrqAffinity::Any => self.affinity,
                _ => newer.affinity,
            },
            flags: self.flags | newer.flags,
        }
    }
}

pub const fn gic_irq_desc(hwirq: Hwirq, trigger: IrqTrigger) -> IrqDesc {
    IrqDesc::new(hwirq, trigger)
        .with_controller(IrqControllerKind::Gic)
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
        .with_controller(IrqControllerKind::Plic)
        .with_domain(PLIC_ROOT_DOMAIN)
}

pub const fn io_apic_irq_desc(hwirq: Hwirq) -> IrqDesc {
    IrqDesc::from_hwirq(hwirq)
        .with_controller(IrqControllerKind::IoApic)
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
