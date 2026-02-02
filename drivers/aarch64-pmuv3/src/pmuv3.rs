//! PMU Counter Configuration Module
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{isb, mrs, msr};

/// PMU overflow interrupt number (typically PPI 23, so INTID 23).
pub const PMU_OVERFLOW_IRQ: u32 = 23;

/// See ARM PMU Events
///
/// todo: more event to support
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum PmuEvent {
    CpuCycles      = 0x11, // Cpu Cycles counter
    MemAccess      = 0x13, // Data memory access
    L2dCache       = 0x16, // Level 2 data cache access
    L2dCacheRefill = 0x17, // Level 2 data cache refill
}

/// Error type for PMU operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PmuError {
    /// PMU not available on this platform.
    NotAvailable,
    /// Invalid counter index.
    InvalidCounter,
    /// Overflow not happended.
    NoOverflow,
    /// PMU not enabled.
    NotEnabled,
    /// Operation failed.
    Failed,
}

/// PMU Counter configuration.
pub struct PmuCounter {
    /// Counter index to use (0-30 for event counters, 31 for cycle counter).
    counter_index: u32,
    /// Threshold value - interrupt triggers when counter reaches this.
    threshold: u64,
    /// Whether the NMI source is enabled.
    enabled: AtomicBool,

    event: Option<PmuEvent>,
}

impl PmuCounter {
    /// Create a new cycle counter (counter 31).
    ///
    /// # Arguments
    /// * `threshold` - Counter value at which to trigger interrupt.
    ///                 For cycle counter, this is CPU cycles.
    pub const fn new_cycle_counter(threshold: u64) -> Self {
        Self {
            counter_index: 31,
            threshold,
            enabled: AtomicBool::new(false),
            event: None,
        }
    }

    /// Create a new event counter.
    ///
    /// # Arguments
    /// * `counter_index` - Event counter index (0-30)
    /// * `event` - PMU event type to count
    /// * `threshold` - Counter value at which to trigger interrupt
    pub const fn new_event_counter(counter_index: u32, threshold: u64, event: PmuEvent) -> Self {
        Self {
            counter_index,
            threshold,
            enabled: AtomicBool::new(false),
            event: Some(event),
        }
    }

    /// Get the counter index.
    pub fn counter_index(&self) -> u32 {
        self.counter_index
    }

    /// Get the threshold value.
    pub fn threshold(&self) -> u64 {
        self.threshold
    }

    /// Set a new threshold value.
    pub fn set_threshold(&mut self, threshold: u64) {
        self.threshold = threshold;
    }

    /// Check pmu support.
    pub fn check_pmu_support(&self) -> Result<(), PmuError> {
        // Read PMU version from ID_AA64DFR0_EL1
        let aa64dfr0: u64 = mrs!(ID_AA64DFR0_EL1);
        let pmu_ver = (aa64dfr0 >> 8) & 0xF;

        // Check pmu version
        if pmu_ver == 0 || pmu_ver == 0xF {
            return Err(PmuError::NotAvailable);
        }

        // Read number of counters from PMCR_EL0
        let pmcr: u64 = mrs!(PMCR_EL0);
        let num_counters = ((pmcr >> 11) & 0x1F) as u32;

        // Validate counter index
        if self.counter_index < 31 && self.counter_index >= num_counters {
            return Err(PmuError::InvalidCounter);
        }
        Ok(())
    }

    /// Enable the PMU Counter.
    ///
    /// This starts the counter and enables overflow interrupt.
    pub fn enable(&self) {
        // Enable overflow interrupt
        msr!(PMINTENSET_EL1, 1u64 << self.counter_index);

        // Enable counter
        msr!(PMCNTENSET_EL0, 1u64 << self.counter_index);

        // Ensure PMU is enabled
        let pmcr: u64 = mrs!(PMCR_EL0);
        msr!(PMCR_EL0, pmcr | (1 << 0) | (1 << 1) | (1 << 2) | (1 << 6)); // Set E, P, U and LC bits

        // Clear any pending overflow
        msr!(PMOVSCLR_EL0, 1u64 << 31);

        msr!(PMCCFILTR_EL0, 0u64, "x");

        // If event counter, select event type
        if let Some(event) = self.event {
            msr!(PMSELR_EL0, self.counter_index, "x");
            msr!(PMXEVTYPER_EL0, event as u32, "x");
        }
        self.set_counter();

        self.enabled.store(true, Ordering::Release);
    }

    /// Disable the PMU Counter.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);

        // Disable counter
        msr!(PMCNTENCLR_EL0, 1u64 << self.counter_index);
        // Disable overflow interrupt
        msr!(PMINTENCLR_EL1, 1u64 << self.counter_index);

        isb!();
    }

    /// Set the counter value.
    pub fn set_counter(&self) {
        if self.counter_index == 31 {
            // Set cycle counter to (MAX - threshold) so it overflows after `threshold` cycles
            let initial_value = u64::MAX - self.threshold;
            msr!(PMCCNTR_EL0, initial_value);
        } else {
            // For event counters, set to (MAX_U32 - threshold)
            let initial_value = (u32::MAX as u64) - (self.threshold & 0xFFFFFFFF);
            msr!(PMXEVCNTR_EL0, initial_value as u32, "x");
        }
        isb!();
    }

    /// Check if overflow occurred and clear the flag.
    ///
    /// Returns true if overflow was detected (and cleared).
    /// This should be called from the interrupt handler.
    pub fn check_and_clear_overflow(&self) -> Result<(), PmuError> {
        let mask = if self.counter_index == 31 {
            1u64 << 31
        } else {
            1u64 << self.counter_index
        };

        // Read overflow status
        let overflow: u64 = mrs!(PMOVSSET_EL0);

        if (overflow & mask) != 0 {
            // Clear overflow flag
            msr!(PMOVSCLR_EL0, mask);
            isb!();
            Ok(())
        } else {
            Err(PmuError::NoOverflow)
        }
    }

    /// Handle PMU overflow interrupt.
    ///
    /// Call this from your interrupt handler. Returns true if this was
    /// a PMU overflow that was handled.
    pub fn handle_overflow(&self) -> Result<(), PmuError> {
        if !self.enabled.load(Ordering::Acquire) {
            return Err(PmuError::NotEnabled);
        }

        self.check_and_clear_overflow()?;
        self.set_counter();
        Ok(())
    }

    /// Check if the NMI source is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
}
