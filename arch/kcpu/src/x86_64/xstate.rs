// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Eager x87/SSE/AVX state management with a system-wide XCR0 contract.
//!
//! Uses standard XSAVE layout, as described by Intel SDM CPUID leaf 0xD
//! and Linux arch/x86/kernel/fpu/{init,xstate,signal}.c. Only x87, SSE and
//! YMM state are enabled; AVX-512, AMX and supervisor state remain disabled.

use core::arch::x86_64::{__cpuid_count, CpuidResult};

use kerrno::{KError, KResult};
use klazy::Once;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

use super::FxStateBlock;

const FPSSE: u64 = 3;
const YMM: u64 = 4;
const AVX_STATE: u64 = FPSSE | YMM;
const LEGACY_BYTES: usize = 512;
const HEADER_BYTES: usize = 64;
const YMM_OFFSET: usize = LEGACY_BYTES + HEADER_BYTES;
const YMM_BYTES: usize = 256;
const XSAVE_BYTES: usize = YMM_OFFSET + YMM_BYTES;
const MAGIC1: u32 = 0x4650_5853;
const MAGIC2: u32 = 0x4650_5845;

/// Maximum Linux signal FP payload, including its trailing magic word.
pub const SIGNAL_FP_BYTES: usize = XSAVE_BYTES + 4;

#[derive(Clone, Copy)]
struct StateConfig {
    // Zero selects FXSAVE; nonzero is the exact XCR0 mask on every CPU.
    mask: u64,
    mxcsr_mask: u32,
}

static CONFIG: Once<StateConfig> = Once::new();

fn supported_mask(mut cpuid: impl FnMut(u32, u32) -> CpuidResult) -> u64 {
    if cpuid(0, 0).eax < 0xd || cpuid(1, 0).ecx & (1 << 26) == 0 {
        return 0;
    }
    let supported = cpuid(0xd, 0).eax as u64;
    if supported & FPSSE != FPSSE {
        return 0;
    }
    let avx = cpuid(1, 0).ecx & (1 << 28) != 0;
    if avx && supported & YMM != 0 {
        let layout = cpuid(0xd, 2);
        if layout.eax as usize == YMM_BYTES
            && layout.ebx as usize == YMM_OFFSET
            && layout.ecx & 1 == 0
        {
            return AVX_STATE;
        }
    }
    FPSSE
}

/// Establishes the same save format on the BSP and every AP before scheduling.
/// The published per-CPU area and FP/SIMD boot setup must already exist.
#[cfg(feature = "fp-simd")]
pub(super) fn init() {
    let supported = supported_mask(__cpuid_count);
    let mut probe = FxStateBlock::initial();
    // SAFETY: boot enables FPU/OSFXSR; probe is a live aligned 512-byte region.
    unsafe { core::arch::x86_64::_fxsave64((&mut probe as *mut FxStateBlock).cast()) };
    let mxcsr_mask = if probe.sse_mxcsr_mask == 0 {
        0xffbf
    } else {
        probe.sse_mxcsr_mask
    };
    let config = CONFIG.call_once(|| StateConfig {
        mask: supported,
        mxcsr_mask,
    });
    // Tasks may migrate immediately after an AP joins the scheduler. Reject
    // incompatible APs before publication rather than truncate their state.
    assert!(
        supported & config.mask == config.mask,
        "AP lacks the BSP xstate format"
    );
    // Users can set every MXCSR bit their current CPU accepts. A superset on
    // an AP would allow state that faults after migration back to the BSP.
    assert_eq!(
        mxcsr_mask, config.mxcsr_mask,
        "AP MXCSR mask differs from BSP"
    );
    if supported != 0 {
        // SAFETY: CPUID verified XSAVE. Boot is CPU-local, before scheduling;
        // existing CR4 flags are retained and XCR0 is set before any XSAVE use.
        unsafe { Cr4::update(|flags| flags.insert(Cr4Flags::OSXSAVE)) };
        // Even an FXSAVE system constrains a newer AP to x87/SSE. Never leave
        // firmware-enabled YMM/ZMM/AMX state outside the scheduler's save mask.
        let mask = config.mask.max(FPSSE);
        // SAFETY: the CPUID-validated mask includes x87, and YMM implies SSE.
        // No user task has run on this CPU and no unsupported XCR0 bit is set.
        unsafe { core::arch::x86_64::_xsetbv(0, mask) };
        if config.mask == 0 {
            // SAFETY: no XSAVE path is used for the selected FXSAVE policy.
            unsafe { Cr4::update(|flags| flags.remove(Cr4Flags::OSXSAVE)) };
        }
    }
    // SAFETY: eager state switching never relies on #NM/lazy FPU ownership.
    unsafe {
        Cr0::update(|flags| flags.remove(Cr0Flags::TASK_SWITCHED | Cr0Flags::EMULATE_COPROCESSOR))
    };
    ExtendedState::default().restore();
}

#[cfg(not(feature = "fp-simd"))]
pub(super) fn init() {}

/// Enabled user xstate bits, or zero when the kernel uses FXSAVE/no FP state.
pub fn enabled_xstate_mask() -> u64 {
    CONFIG.get().map_or(0, |config| config.mask)
}

pub(super) fn mxcsr_mask() -> u32 {
    CONFIG.get().map_or(0xffbf, |config| config.mxcsr_mask)
}

/// Fully initialized, standard-layout XSAVE storage. Private headers prevent
/// unvalidated user data from reaching XRSTOR. Fixed capacity is sufficient
/// because initialization enables only x87/SSE and the checked YMM component.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
pub struct ExtendedState {
    pub(super) fx: FxStateBlock,
    xstate_bv: u64,
    xcomp_bv: u64,
    reserved: [u64; 6],
    ymm_hi: [[u64; 2]; 16],
}

const _: () = assert!(core::mem::size_of::<ExtendedState>() == XSAVE_BYTES);
const _: () = assert!(core::mem::offset_of!(ExtendedState, ymm_hi) == YMM_OFFSET);

impl ExtendedState {
    /// Clean x87/SSE state and initially zero YMM upper halves.
    pub const fn default() -> Self {
        Self {
            fx: FxStateBlock::initial(),
            xstate_bv: FPSSE,
            xcomp_bv: 0,
            reserved: [0; 6],
            ymm_hi: [[0; 2]; 16],
        }
    }

    /// Saves the current task's FP state. The caller must own the running task;
    /// kernel code must not use SIMD between user entry and this capture.
    #[cfg(feature = "fp-simd")]
    pub fn save(&mut self) {
        let mask = enabled_xstate_mask();
        if mask == 0 {
            // SAFETY: the first 512 bytes are aligned, live FXSAVE storage.
            unsafe { core::arch::x86_64::_fxsave64((self as *mut Self).cast()) };
        } else {
            // SAFETY: all CPUs enabled this mask before scheduling; the
            // aligned 832-byte object covers every enabled standard component.
            unsafe { core::arch::x86_64::_xsave64((self as *mut Self).cast(), mask) };
        }
    }

    /// Restores an initialized or validated snapshot into the current task.
    /// All runnable CPUs share the selected XCR0 contract.
    #[cfg(feature = "fp-simd")]
    pub fn restore(&self) {
        let mask = enabled_xstate_mask();
        if mask == 0 {
            // SAFETY: constructors and signal decoding validate MXCSR; the
            // aligned legacy region has the FXSAVE64 layout.
            unsafe { core::arch::x86_64::_fxrstor64((self as *const Self).cast()) };
        } else {
            // SAFETY: private headers contain only enabled bits, zero reserved
            // words and non-compacted format; MXCSR is masked at user import.
            unsafe { core::arch::x86_64::_xrstor64((self as *const Self).cast(), mask) };
        }
    }

    /// Captures live state in fresh, zero-filled storage for fork or signals.
    pub fn capture() -> Self {
        let mut state = Self::default();
        #[cfg(feature = "fp-simd")]
        state.save();
        state
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: repr(C) consists solely of fully initialized integer fields;
        // asserted size/layout leaves no implicit padding, and self stays live.
        unsafe { core::slice::from_raw_parts((self as *const Self).cast(), XSAVE_BYTES) }
    }

    /// Encodes a Linux signal FP frame (legacy or standard XSAVE plus magic2).
    /// The returned length bounds initialized bytes to write to user memory.
    pub fn signal_bytes(&self) -> ([u8; SIGNAL_FP_BYTES], usize) {
        let mut bytes = [0; SIGNAL_FP_BYTES];
        let mask = enabled_xstate_mask();
        let size = if mask == 0 {
            LEGACY_BYTES
        } else if mask & YMM == 0 {
            YMM_OFFSET
        } else {
            XSAVE_BYTES
        };
        bytes[..size].copy_from_slice(&self.bytes()[..size]);
        bytes[464..512].fill(0);
        if mask == 0 {
            return (bytes, size);
        }
        bytes[464..468].copy_from_slice(&MAGIC1.to_le_bytes());
        bytes[468..472].copy_from_slice(&((size + 4) as u32).to_le_bytes());
        bytes[472..480].copy_from_slice(&mask.to_le_bytes());
        bytes[480..484].copy_from_slice(&(size as u32).to_le_bytes());
        bytes[size..size + 4].copy_from_slice(&MAGIC2.to_le_bytes());
        (bytes, size + 4)
    }

    /// Decodes a copied Linux FP signal payload. Never reads user memory.
    /// Rejects unsupported/compacted headers and invalid length/magic fields;
    /// absent components are initialized by XRSTOR. Legacy frames clear YMM.
    pub fn from_signal_bytes(bytes: &[u8]) -> KResult<Self> {
        if bytes.len() < LEGACY_BYTES {
            return Err(KError::InvalidInput);
        }
        let length = signal_fp_length(bytes)?;
        if bytes.len() != length {
            return Err(KError::InvalidInput);
        }
        let mut state = Self::default();
        let size = if length == LEGACY_BYTES {
            LEGACY_BYTES
        } else {
            length - 4
        };
        // SAFETY: both regions are disjoint and at least size bytes long;
        // every field of ExtendedState is an integer accepting all bit patterns.
        // The header is validated below before this value can reach restore().
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), (&mut state as *mut Self).cast(), size)
        };
        state.fx.sse_mxcsr &= mxcsr_mask();
        if length != LEGACY_BYTES {
            let features = u64::from_le_bytes(
                bytes[472..480]
                    .try_into()
                    .map_err(|_| KError::InvalidInput)?,
            );
            if u32::from_le_bytes(
                bytes[size..size + 4]
                    .try_into()
                    .map_err(|_| KError::InvalidInput)?,
            ) != MAGIC2
                || state.xcomp_bv != 0
                || state.reserved != [0; 6]
                || state.xstate_bv & !features != 0
                || features & !enabled_xstate_mask() != 0
                || (state.xstate_bv & YMM != 0 && size < XSAVE_BYTES)
            {
                return Err(KError::InvalidInput);
            }
        }
        Ok(state)
    }
}

/// Determines the bounded Linux FP payload length from a copied 512-byte
/// legacy prefix. A zero/unknown magic selects legacy FXSAVE compatibility.
pub fn signal_fp_length(prefix: &[u8]) -> KResult<usize> {
    if prefix.len() < LEGACY_BYTES {
        return Err(KError::InvalidInput);
    }
    let word = |offset| {
        u32::from_le_bytes([
            prefix[offset],
            prefix[offset + 1],
            prefix[offset + 2],
            prefix[offset + 3],
        ])
    };
    if word(464) != MAGIC1 {
        return Ok(LEGACY_BYTES);
    }
    let size = word(480) as usize;
    let total = word(468) as usize;
    if enabled_xstate_mask() == 0
        || !(YMM_OFFSET..=XSAVE_BYTES).contains(&size)
        || total != size + 4
    {
        return Err(KError::InvalidInput);
    }
    Ok(total)
}

unittest::path_mod_test!(tests, "../../ut-tests/x86_xstate.rs");
