// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NMI capability provider for AArch64 / GIC.
//!
//! # Strategy
//!
//! The NMI **mechanism** is selected at build time (`nmi-pseudo` for GICv3
//! PMR priority‑masking, `nmi-hardware` for GICv3.3 + FEAT_NMI), so each
//! build carries exactly one mechanism and its matching interrupt‑masking
//! discipline.  At boot the platform only **validates** that the compiled
//! mechanism is supported by the hardware and reports/falls back to
//! `None` otherwise.
//!
//! This module knows about the **NMI mode** (hardware vs. pseudo), not about
//! the NMI **source** (PMU today, future sources).  Source‑specific setup
//! belongs to the source's own subsystem (see `NmiPeriodic` in `kplat`).

#[macro_export]
macro_rules! nmi_if_impl {
    () => {
        use irq_driver::gic;
        #[cfg(any(feature = "nmi-pseudo", feature = "nmi-hardware"))]
        use irq_driver::gic::GicVersion;
        use kplat::nm_irq::{NmiMode, NmiSourceInfo};
        use lazyinit::LazyInit;

        /// Set once during `early_init()`, read everywhere else.
        static CURRENT_MODE: LazyInit<NmiMode> = LazyInit::new();

        fn current_mode() -> NmiMode {
            CURRENT_MODE.get().copied().unwrap_or(NmiMode::None)
        }

        /// Validate that the compile‑time selected NMI mechanism is
        /// supported by this hardware.
        ///
        /// The mechanism is a build‑time choice whose exception-entry code
        /// touches the mechanism's registers; when the runtime hardware does
        /// not support it, the build degrades to plain‑IRQ operation (NMI
        /// disabled) and every NMI register access is gated on the runtime
        /// mode reported here.  The build declares its GIC target via
        /// `GIC_VERSION_V3`; booting it elsewhere is still tolerated, with
        /// NMI simply disabled.
        fn detect_mode() -> NmiMode {
            #[cfg(feature = "nmi-hardware")]
            {
                let version = gic::active_version();
                if version < GicVersion::V3 {
                    kernel_boot::bootln!(
                        "NMI mode: hardware NMI requires GICv3.3, found {version}; \
                         NMI disabled",
                    );
                    return NmiMode::None;
                }

                // Hardware NMI requires the GICv3.3 NMI extension advertised
                // by GICD_TYPER.NMI and CPU FEAT_NMI. Capability detection is
                // read-only; programming GICR_INMIR0 is deferred until the
                // PMU PPI is explicitly promoted on each CPU.
                let gic_nmi = gic::supports_hardware_nmi();
                let cpu_nmi = id_aa64pfr1_el1_nmi();
                if gic_nmi && cpu_nmi {
                    kernel_boot::bootln!("NMI mode: Hardware (GICv3.3)");
                    NmiMode::Hardware
                } else {
                    kernel_boot::bootln!(
                        "NMI mode: hardware NMI requires GICv3.3 GICD_TYPER.NMI \
                         ({gic_nmi}) and FEAT_NMI ({cpu_nmi}), but they are unavailable; \
                         NMI disabled"
                    );
                    NmiMode::None
                }
            }

            #[cfg(feature = "nmi-pseudo")]
            {
                let version = gic::active_version();
                if version < GicVersion::V3 {
                    kernel_boot::bootln!(
                        "NMI mode: pseudo-NMI requires GICv3+ PMR, found {version}; \
                         NMI disabled",
                    );
                    NmiMode::None
                } else {
                    kernel_boot::bootln!("NMI mode: Pseudo (PMR)");
                    NmiMode::Pseudo
                }
            }

            #[cfg(not(any(feature = "nmi-hardware", feature = "nmi-pseudo")))]
            {
                NmiMode::None
            }
        }

        /// `ID_AA64PFR1_EL1.NMI` — FEAT_NMI support (bits [39:36]).
        ///
        /// Value `0b0001` means `SCTLR_ELx.{SPINTMASK, NMI}` and
        /// `PSTATE.ALLINT` are supported (FEAT_NMI); other values are
        /// reserved.
        #[cfg(feature = "nmi-hardware")]
        fn id_aa64pfr1_el1_nmi() -> bool {
            let pfr1: u64;
            // SAFETY: ID_AA64PFR1_EL1 is a read-only feature register
            // accessible from EL1; pure register read.
            unsafe { core::arch::asm!("mrs {}, ID_AA64PFR1_EL1", out(reg) pfr1, options(nomem, nostack)) };
            (pfr1 >> 36) & 0xF == 0b0001
        }

        #[impl_dev_interface]
        impl kplat::nm_irq::NmiDef {
            fn early_init() -> bool {
                let mode = detect_mode();
                CURRENT_MODE.init_once(mode);
                true
            }

            fn late_init() -> bool {
                #[cfg(feature = "nmi-hardware")]
                if matches!(current_mode(), NmiMode::Hardware) {
                    // The NMI mode is decided by `detect_mode()`; expose the
                    // derived readiness flag to the low-level ALLINT
                    // accessors (they must not touch the register on a
                    // degraded build).
                    karch::mark_allint_active();
                    // Enable FEAT_NMI on this CPU.  SCTLR_EL1.NMI (bit 61)
                    // activates the PSTATE.ALLINT mask and gives IRQ/FIQ
                    // Superpriority.  Per-CPU: called on primary and every
                    // secondary CPU.
                    //
                    // ALLINT resets to an architecturally UNKNOWN value and
                    // exception entry sets it to 1 (SPINTMASK=0) to suppress
                    // nesting, including NMIs.  Clear it now so normal IRQs
                    // are unmasked before NMI delivery is enabled; the
                    // exception exit path clears it again before ERET.
                    //
                    // SPINTMASK (bit 62) is intentionally left 0 for now:
                    // with SPINTMASK=1, PSTATE.SP==1 masks every IRQ/FIQ on
                    // this CPU, and QEMU's FEAT_NMI modeling of PSTATE.SP is
                    // not trustworthy yet.  Nested-NMI protection (the reason
                    // for SPINTMASK=1) must be revisited on real hardware.
                    // The reset value is architecturally UNKNOWN, so the bit
                    // must be cleared explicitly rather than relied on being
                    // 0 from reset or the bootloader.
                    let sctlr: u64;
                    // SAFETY: SCTLR_EL1 and ALLINT are accessible from EL1;
                    // read-modify-write preserves all other control bits.
                    unsafe {
                        karch::allint_clear();
                        core::arch::asm!("isb", options(nomem, nostack));
                        core::arch::asm!(
                            "mrs {}, SCTLR_EL1",
                            out(reg) sctlr,
                            options(nomem, nostack)
                        );
                        core::arch::asm!(
                            "msr SCTLR_EL1, {}",
                            in(reg) (sctlr | (1 << 61)) & !(1 << 62), // NMI=1, SPINTMASK=0
                            options(nomem, nostack)
                        );
                        core::arch::asm!("isb", options(nomem, nostack));
                    }
                }
                true
            }

            fn mode() -> NmiMode {
                current_mode()
            }

            fn configure_nmi(hwirq: usize) -> bool {
                #[cfg(feature = "nmi-hardware")]
                if matches!(current_mode(), NmiMode::Hardware) {
                    // Propagate controller-level failures (unsupported
                    // INTID, missing redistributor frame) so the caller can
                    // report that NMI delivery was never actually enabled.
                    return gic::set_nmi_attr(hwirq, true);
                }
                #[cfg(feature = "nmi-pseudo")]
                if matches!(current_mode(), NmiMode::Pseudo) {
                    // Promote to pseudo‑NMI by raising GIC priority to 0.
                    gic::set_prio(hwirq, 0);
                    return true;
                }
                warn!("configure_nmi: NMI not supported on this platform; hwirq {hwirq}");
                false
            }

            fn info() -> NmiSourceInfo {
                NmiSourceInfo {
                    name: if current_mode() == NmiMode::Hardware {
                        "GICv3.3 hardware NMI"
                    } else if current_mode() == NmiMode::Pseudo {
                        "GICv3 pseudo-NMI"
                    } else {
                        "NMI unavailable"
                    },
                }
            }
        }
    };
}
