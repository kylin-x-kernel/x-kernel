// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared IRQ vocabulary independent of kernel runtime and driver resources.
//!
//! These values describe trigger modes, controller families and opaque domain
//! identities. They neither allocate domains nor validate routes. The host IRQ
//! implementation owns domain registration and interprets identifiers.
#![no_std]

/// Interrupt trigger mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqTrigger {
    /// Interrupt asserted on a rising edge.
    EdgeRising,
    /// Interrupt asserted on a falling edge.
    EdgeFalling,
    /// Interrupt active while the line is high.
    LevelHigh,
    /// Interrupt active while the line is low.
    LevelLow,
    /// Trigger mode not described by firmware; preserves raw flag bits.
    Unknown(u32),
}

/// Interrupt controller family, not the identity of a controller instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqController {
    /// ARM Generic Interrupt Controller.
    Gic,
    /// RISC-V Platform-Level Interrupt Controller.
    Plic,
    /// x86 I/O APIC.
    IoApic,
    /// Message-signaled interrupts.
    Msi,
    /// LoongArch extended I/O interrupt controller.
    LoongArchExtioi,
    /// Controller not described by firmware or currently unknown.
    Unknown,
}

/// Opaque IRQ domain identifier interpreted by the host IRQ implementation.
///
/// Constructing a value does not register a domain or establish its validity.
/// Numeric equality is meaningful only within the same host domain namespace.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct IrqDomainId(u32);

impl IrqDomainId {
    /// Wrap a domain identifier supplied by the host.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// Return the identifier for conversion at a host boundary.
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}
