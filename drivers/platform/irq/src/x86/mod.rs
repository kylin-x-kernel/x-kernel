// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86 interrupt-controller integration for `kirq`.
//!
//! The module is split by lifetime:
//!
//! - `config` describes the boot topology, validates it, probes and installs
//!   the controllers once, and publishes the instance topology table.
//! - [`discovery`] turns the ACPI MADT into unvalidated declarations.
//! - `runtime` serves descriptor operations and dispatch after boot.
//!
//! This file wires those pieces to the `IntrManagerIf` provider and remains
//! the module's public surface.

mod config;
pub mod discovery;
mod runtime;

pub use config::{
    IO_APIC_DOMAIN, IoApicDeclaration, IoApicInstanceConfig, LegacyRoute, LegacyRouteOrigin,
    MAX_IO_APICS, MAX_LEGACY_IRQS, X86IrqInitConfig, gsi_desc, init, init_secondary,
};
#[cfg(unittest)]
pub(crate) use config::{
    IoApicTopologyEntry, gsi_desc_in, validate_capabilities, validate_config,
    validate_legacy_routes, validate_ranges,
};
pub(crate) use runtime::complete_delivery;
use runtime::{configure, dispatch_irq, enable, is_msi_source, notify_cpu, select_x86_flow};
#[cfg(unittest)]
pub(crate) use runtime::{configure_with, dispatch_irq_with, io_pin, prepare_regular_delivery};
pub use x86_apic::IoApicInitPolicy;

unittest::path_mod_test!(mod tests, "../../ut-tests/x86.rs");
unittest::path_mod_test!(mod acpi_madt_tests, "../../ut-tests/acpi_madt.rs");

#[kiface::provide]
impl kirq::IntrManagerIf {
    fn guest_interface() -> Option<kirq::GuestIrqInterface> {
        None
    }

    fn percpu_forwarding(_: kirq::IrqDesc) -> Option<&'static kirq::PerCpuForwardingChip> {
        None
    }

    fn percpu_nmi(_: kirq::IrqDesc) -> Option<&'static kirq::PerCpuNmiChip> {
        None
    }

    fn select_flow(desc: kirq::IrqDesc) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
        let flow = select_x86_flow(desc, x86_apic::pin_trigger_mode)?;
        if is_msi_source(desc) && !x86_apic::msi_route_matches(desc.hwirq, desc.affinity) {
            return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
        }
        Ok(flow)
    }

    fn prepare_local_irq_work() -> Option<fn()> {
        None
    }

    fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
        configure(desc)
    }

    fn enable(desc: kirq::IrqDesc, enabled: bool) {
        enable(desc, enabled);
    }

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
    unsafe fn dispatch_irq(vector: usize) -> Option<kirq::Virq> {
        // SAFETY: the captured vector is claimed and consumed exactly once
        // within the facade caller's hardirq and CPU/IRQ exclusion.
        let pending = unsafe { dispatch_irq(vector)? };
        kirq::generic_handle_irq(pending)
    }

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::dispatch_nmi`]'s contract.
    unsafe fn dispatch_nmi(_irq: usize) {}

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::complete_irq`]'s contract.
    unsafe fn complete_irq(completion_cookie: usize) {
        complete_delivery(completion_cookie, x86_apic::end_of_interrupt);
    }

    fn notify_cpu(interrupt_id: usize, target: kirq::TargetCpu) {
        notify_cpu(interrupt_id, target);
    }

    fn set_prio(_irq: usize, _priority: u8) {
        debug!("x86 APIC priority programming is not implemented");
    }
}
