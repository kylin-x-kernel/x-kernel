// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for kplat-x86_64.

use acpi::MadtParseError;
use irq_driver::x86::{
    IoApicDeclaration, IoApicInitPolicy, X86IrqInitConfig,
    discovery::{self, X86DiscoveryError},
};
use kbuild_config::TIMER_FREQUENCY_HZ;
use kerrno::{KError, KResult};
use klazy::Once;
use kplat::boot::{BootInfo, PlatformBootIf};

/// Authorized platform fallback when firmware declares no usable IO-APIC.
const STATIC_IO_APIC_BASE: u32 = 0xFEC0_0000;
/// Legacy ISA line carrying the boot serial console.
const CONSOLE_ISA_IRQ: u8 = 4;

/// Boot IO-APIC configuration selected once during early driver init.
///
/// The console needs its routed descriptor before the unified IRQ entry runs,
/// so the same configuration is retained and consumed by `prepare_interrupts`.
static BOOT_IO_APIC_CONFIG: Once<Result<X86IrqInitConfig, KError>> = Once::new();

/// Platform-relevant outcome of interpreting firmware IO-APIC discovery.
// The boot configuration embeds the fixed 16-entry legacy route table. This
// enum is built once during boot and never stored in a collection, so boxing
// the large variant would add ambiguity and allocation without a benefit.
#[expect(
    clippy::large_enum_variant,
    reason = "single-use boot selection value; boxing the configuration adds allocation for no \
              ownership or size benefit"
)]
enum IoApicBootSource {
    /// Firmware described a controller this runtime can initialize.
    Discovered(X86IrqInitConfig),
    /// No ACPI, no MADT, or no usable controller declaration.
    Absent,
    /// The MADT entry stream is structurally corrupt.
    Malformed(MadtParseError),
    /// Well-formed declarations the current runtime cannot represent.
    Unsupported(KError),
}

fn platform_static_io_apic_config() -> X86IrqInitConfig {
    warn!(
        "ACPI MADT has no usable IO-APIC; using authorized static base {STATIC_IO_APIC_BASE:#010x}"
    );
    X86IrqInitConfig::platform_static(
        IoApicDeclaration {
            id: 0,
            address: STATIC_IO_APIC_BASE,
            gsi_base: 0,
        },
        IoApicInitPolicy::platform_legacy(),
    )
}

fn io_apic_boot_source() -> IoApicBootSource {
    match discovery::from_acpi() {
        Ok(declarations) => {
            match declarations.to_init_config(IoApicInitPolicy::platform_legacy()) {
                Ok(config) => IoApicBootSource::Discovered(config),
                Err(error) => IoApicBootSource::Unsupported(error),
            }
        }
        Err(X86DiscoveryError::Absent) => IoApicBootSource::Absent,
        Err(X86DiscoveryError::Malformed(error)) => IoApicBootSource::Malformed(error),
    }
}

fn select_io_apic_config(source: IoApicBootSource) -> KResult<X86IrqInitConfig> {
    match source {
        IoApicBootSource::Discovered(config) => Ok(config),
        IoApicBootSource::Absent => Ok(platform_static_io_apic_config()),
        IoApicBootSource::Malformed(error) => {
            error!(
                "ACPI MADT IO-APIC discovery failed at offset {}: {:?}",
                error.offset, error.kind
            );
            Err(KError::InvalidInput)
        }
        IoApicBootSource::Unsupported(error) => Err(error),
    }
}

fn io_apic_config() -> KResult<X86IrqInitConfig> {
    select_io_apic_config(io_apic_boot_source())
}

#[kiface::provide]
impl PlatformBootIf {
    fn prepare_boot_memory(boot_info: &BootInfo) {
        crate::peripherals::bootmem::init_ap_trampoline_page(boot_info);
    }

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        timer_driver::x86_lapic_tsc::early_init(
            timer_driver::x86_lapic_tsc::TimerConfig::platform_static(
                ktime_types::Frequency::from_hz(TIMER_FREQUENCY_HZ as u64),
            ),
        );
        kernel_boot::bootln!("timer init");
        let io_apic_config = io_apic_config();
        // The console is the first legacy-IRQ consumer. Route its ISA line
        // through the selected boot configuration; if selection failed, stay
        // polled and let `prepare_interrupts` report the fatal error.
        let console_irq = io_apic_config
            .as_ref()
            .ok()
            .and_then(|config| config.legacy_irq_desc(CONSOLE_ISA_IRQ));
        console_driver::init_stdout_ioport(console_driver::boot_console_io_port(), console_irq);
        BOOT_IO_APIC_CONFIG.call_once(|| io_apic_config);
        kernel_boot::bootln!("console driver init");
        #[cfg(feature = "rtc")]
        {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: stdout configuration \
                          cannot initialize; early_driver_init has no error return or recovery"
            )]
            let sample = rtc_driver::read(rtc_driver::RtcConfig::platform(
                rtc_driver::RtcKind::Cmos,
                rtc_driver::RtcSource::PlatformStatic,
            ))
            .expect("failed to read platform rtc");
            ktime::initialize_realtime(sample);
        }
        kernel_boot::bootln!("rtc init");
    }

    fn prepare_interrupts(_boot_info: &BootInfo) -> kerrno::KResult<()> {
        let config = BOOT_IO_APIC_CONFIG
            .get()
            .copied()
            .ok_or(KError::InvalidInput)??;
        irq_driver::x86::init(config)
    }

    #[cfg(feature = "smp")]
    fn prepare_local_interrupts(_logical_cpu_id: kcpu_id_map::LogicalCpuId) -> kerrno::KResult<()> {
        irq_driver::x86::init_secondary()
    }

    fn activate_local_interrupts() -> kerrno::KResult<()> {
        timer_driver::x86_lapic_tsc::init_percpu();
        Ok(())
    }
}

unittest::path_mod_test!(mod io_apic_config_tests, "../ut-tests/io_apic_config.rs");
