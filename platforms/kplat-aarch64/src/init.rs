// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for aarch64.

use kbuild_config::PSCI_METHOD;
use kplat::boot::{BootHandler, BootInfo};

#[kiface::provide]
impl BootHandler {
    fn prepare_boot_memory(_boot_info: &BootInfo) {
        crate::mmio::prepare_boot_memory();
    }

    fn firmware_init(_boot_info: &BootInfo) {
        // PSCI conduit method ("smc"/"hvc") from the /psci FDT node, falling
        // back to the Kconfig default when the node is absent.
        let method = of::find_node("/psci")
            .and_then(|node| node.property_str("method"))
            .unwrap_or(PSCI_METHOD);
        crate::peripherals::psci::init(method);
    }

    fn early_driver_init() {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the required ARM timer \
                      node is missing or invalid; early_driver_init has no error return or \
                      recovery"
        )]
        let timer_config = timer_driver::arm_generic::config_from_device_tree()
            .expect("ARM generic timer DT node is required on aarch64");
        timer_driver::arm_generic::init(timer_config);
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: stdout configuration \
                      cannot be parsed; early_driver_init has no error return or recovery"
        )]
        console_driver::init_stdout_from_device_tree()
            .expect("failed to parse console from device tree");
        #[cfg(feature = "rtc")]
        {
            // RTC init is best-effort: RK3588's RTC is a PMIC (rk806/hym8563
            // over i2c), not the pl031 MMIO RTC this driver probes, so it
            // typically finds nothing. Don't panic the whole boot over a
            // missing RTC; just record it for bring-up diagnostics.
            if let Some(sample) = rtc_driver::read_from_device_tree() {
                ktime::initialize_realtime(sample);
            } else {
                kernel_boot::bootln!("rtc init skipped (non-fatal): no pl031 RTC in device tree");
            }
        }
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: mandatory GIC preparation \
                      failed; this boot hook has no error return and must not activate sources"
        )]
        irq_driver::gic::init_from_device_tree().expect("primary GIC initialization failed");
        console_driver::register_input_irq_handler();
        #[cfg(feature = "nmi")]
        khal::nmi::early_init();
    }

    fn prepare_interrupts(_boot_info: &BootInfo) -> kerrno::KResult<()> {
        // Early GIC discovery already prepared the primary CPU interface.
        // Reinitializing it would reset private IRQ state and priority masks.
        #[cfg(feature = "nmi")]
        khal::nmi::late_init();
        Ok(())
    }

    #[cfg(feature = "smp")]
    fn prepare_local_interrupts(_cpu_id: kcpu_id_map::LogicalCpuId) -> kerrno::KResult<()> {
        irq_driver::gic::init_current_cpu().map_err(|error| {
            error!("secondary GIC preparation failed: {error}");
            kerrno::KError::BadState
        })?;
        #[cfg(feature = "nmi")]
        khal::nmi::late_init();
        Ok(())
    }

    fn activate_local_interrupts() -> kerrno::KResult<()> {
        timer_driver::arm_generic::init_percpu();
        Ok(())
    }
}
