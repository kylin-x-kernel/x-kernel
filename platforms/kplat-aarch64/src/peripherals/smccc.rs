// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared AArch64 SMCCC call helpers.

use core::sync::atomic::{AtomicU8, Ordering};

core::arch::global_asm!(include_str!("smccc_trap.S"));

/// SMCCC success return value in `x0`.
pub const SMCCC_RET_SUCCESS: usize = 0;

/// SMCCC not-supported / conduit-unavailable return value in `x0`.
pub const SMCCC_RET_NOT_SUPPORTED: usize = usize::MAX;

/// Firmware conduit used for SMCCC calls from the current kernel instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SmcccConduit {
    /// No valid SMCCC conduit has been discovered.
    None = 0,
    /// Hypervisor call conduit.
    Hvc  = 1,
    /// Secure monitor call conduit.
    Smc  = 2,
}

impl SmcccConduit {
    const fn from_raw(value: u8) -> Self {
        match value {
            x if x == Self::Hvc as u8 => Self::Hvc,
            x if x == Self::Smc as u8 => Self::Smc,
            _ => Self::None,
        }
    }

    /// Stable name used in boot logs.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Hvc => "hvc",
            Self::Smc => "smc",
        }
    }
}

static CONDUIT: AtomicU8 = AtomicU8::new(SmcccConduit::None as u8);

/// Raw return registers captured from an SMCCC call.
///
/// Layout must stay `#[repr(C)]` with four `usize` fields in `x0..x3` order so
/// the assembly in `smccc_trap.S` can store return registers with `stp`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SmcccResult {
    pub x0: usize,
    pub x1: usize,
    pub x2: usize,
    pub x3: usize,
}

/// Initialize the SMCCC conduit discovered from firmware description.
///
/// The PSCI device-tree node defines the conduit for standard SMCCC firmware
/// services. When the kernel runs at EL2 (`feature = "vmm"`), standard calls
/// use SMC even if firmware reports HVC, matching the existing VHE convention.
pub(crate) fn init_conduit(method: &str) {
    let conduit = match method {
        "hvc" => SmcccConduit::Hvc,
        "smc" => SmcccConduit::Smc,
        _ => panic!("Unknown SMCCC conduit method: {}", method),
    };
    CONDUIT.store(conduit as u8, Ordering::Release);
}

/// Returns the SMCCC conduit described by firmware.
pub(crate) fn current_conduit() -> SmcccConduit {
    SmcccConduit::from_raw(CONDUIT.load(Ordering::Acquire))
}

/// Returns the conduit used by [`invoke`] for standard SMCCC calls.
pub(crate) fn standard_conduit() -> SmcccConduit {
    standard_conduit_for(current_conduit())
}

fn standard_conduit_for(conduit: SmcccConduit) -> SmcccConduit {
    match conduit {
        SmcccConduit::Hvc if cfg!(feature = "vmm") => SmcccConduit::Smc,
        conduit => conduit,
    }
}

/// Invoke a standard SMCCC call through the discovered conduit.
///
/// If firmware has not provided a usable conduit, this returns
/// [`SMCCC_RET_NOT_SUPPORTED`] in `x0` without executing HVC or SMC.
pub(crate) fn invoke(func: u32, arg0: usize, arg1: usize, arg2: usize) -> SmcccResult {
    match standard_conduit() {
        SmcccConduit::Hvc => hvc_call(func, arg0, arg1, arg2),
        SmcccConduit::Smc => smc_call(func, arg0, arg1, arg2),
        SmcccConduit::None => SmcccResult {
            x0: SMCCC_RET_NOT_SUPPORTED,
            ..SmcccResult::default()
        },
    }
}

// Declared by `smccc_trap.S` (included above via `global_asm!`). Calling these
// is unsafe: they write through `out` and rely on the SMCCC / exception-table
// contract documented at each call site.
unsafe extern "C" {
    fn aarch64_peripherals_smccc_hvc_call(
        func: u32,
        arg0: usize,
        arg1: usize,
        arg2: usize,
        out: *mut SmcccResult,
    );
    fn aarch64_peripherals_smccc_smc_call(
        func: u32,
        arg0: usize,
        arg1: usize,
        arg2: usize,
        out: *mut SmcccResult,
    );
}

/// Invoke an SMCCC HVC call and return the raw `x0`..`x3` results.
pub fn hvc_call(func: u32, arg0: usize, arg1: usize, arg2: usize) -> SmcccResult {
    let mut out = SmcccResult::default();
    // SAFETY:
    // 1. `out` is a stack-local `SmcccResult`; `&mut out` is aligned, exclusive,
    //    and valid for the duration of the call.
    // 2. `SmcccResult` is `#[repr(C)]` with four `usize` fields (`x0..x3`),
    //    matching the assembly which stores return registers via
    //    `stp x0,x1,[out]` and `stp x2,x3,[out,#16]`.
    // 3. Arguments follow the SMCCC calling convention (`x0`=func,
    //    `x1..x3`=args); the helper moves `out` into `x8` before the HVC.
    // 4. `smccc_trap.S` registers an `__ex_table` entry for the `hvc` site so
    //    an unavailable conduit traps into a fixup that writes
    //    `SMCCC_RET_NOT_SUPPORTED` into `out.x0` instead of panicking.
    unsafe {
        aarch64_peripherals_smccc_hvc_call(func, arg0, arg1, arg2, &mut out);
    }
    out
}

/// Invoke an SMCCC SMC call and return the raw `x0`..`x3` results.
pub fn smc_call(func: u32, arg0: usize, arg1: usize, arg2: usize) -> SmcccResult {
    let mut out = SmcccResult::default();
    // SAFETY:
    // 1. `out` is a stack-local `SmcccResult`; `&mut out` is aligned, exclusive,
    //    and valid for the duration of the call.
    // 2. `SmcccResult` is `#[repr(C)]` with four `usize` fields (`x0..x3`),
    //    matching the assembly which stores return registers via
    //    `stp x0,x1,[out]` and `stp x2,x3,[out,#16]`.
    // 3. Arguments follow the SMCCC calling convention (`x0`=func,
    //    `x1..x3`=args); the helper moves `out` into `x8` before the SMC.
    // 4. `smccc_trap.S` registers an `__ex_table` entry for the `smc` site so
    //    an unavailable conduit traps into a fixup that writes
    //    `SMCCC_RET_NOT_SUPPORTED` into `out.x0` instead of panicking.
    unsafe {
        aarch64_peripherals_smccc_smc_call(func, arg0, arg1, arg2, &mut out);
    }
    out
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_smccc_conduit_raw_values() {
        assert_eq!(
            SmcccConduit::from_raw(SmcccConduit::None as u8),
            SmcccConduit::None
        );
        assert_eq!(
            SmcccConduit::from_raw(SmcccConduit::Hvc as u8),
            SmcccConduit::Hvc
        );
        assert_eq!(
            SmcccConduit::from_raw(SmcccConduit::Smc as u8),
            SmcccConduit::Smc
        );
        assert_eq!(SmcccConduit::from_raw(0xff), SmcccConduit::None);
    }

    #[def_test]
    fn test_smccc_conduit_names() {
        assert_eq!(SmcccConduit::None.as_str(), "none");
        assert_eq!(SmcccConduit::Hvc.as_str(), "hvc");
        assert_eq!(SmcccConduit::Smc.as_str(), "smc");
    }

    #[def_test]
    fn test_standard_conduit_preserves_none_and_smc() {
        assert_eq!(standard_conduit_for(SmcccConduit::None), SmcccConduit::None);
        assert_eq!(standard_conduit_for(SmcccConduit::Smc), SmcccConduit::Smc);
    }

    #[def_test]
    fn test_standard_conduit_overrides_hvc_only_for_vmm() {
        let expected = if cfg!(feature = "vmm") {
            SmcccConduit::Smc
        } else {
            SmcccConduit::Hvc
        };
        assert_eq!(standard_conduit_for(SmcccConduit::Hvc), expected);
    }
}
