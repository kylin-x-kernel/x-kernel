//! Concrete guard type implementations.

use super::BaseGuard;

/// No-op guard (does nothing).
#[derive(Debug, Clone, Copy)]
pub struct NoOp;

impl BaseGuard for NoOp {
    type State = ();

    #[inline(always)]
    fn acquire() -> Self::State {}

    #[inline(always)]
    fn release(_state: Self::State) {}
}

impl NoOp {
    /// Create a new no-op guard.
    #[inline(always)]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for NoOp {
    fn default() -> Self {
        Self
    }
}

// Kernel-mode guards
#[cfg(target_os = "none")]
mod kernel {
    use super::*;

    /// Guard that saves/restores IRQ state.
    #[derive(Debug)]
    pub struct IrqSave(pub(super) usize);

    /// Guard that disables/enables preemption.
    #[derive(Debug)]
    pub struct NoPreempt;

    /// Guard that disables both preemption and IRQs.
    #[derive(Debug)]
    pub struct NoPreemptIrqSave(pub(super) usize);

    // IrqSave implementation
    impl BaseGuard for IrqSave {
        type State = usize;

        #[inline]
        fn acquire() -> Self::State {
            crate::guard::arch::save_disable()
        }

        #[inline]
        fn release(state: Self::State) {
            crate::guard::arch::restore(state)
        }
    }

    impl IrqSave {
        /// Create a new guard, entering the critical section.
        #[inline]
        pub fn new() -> Self {
            Self(<Self as BaseGuard>::acquire())
        }
    }

    impl Drop for IrqSave {
        #[inline]
        fn drop(&mut self) {
            <Self as BaseGuard>::release(self.0)
        }
    }

    impl Default for IrqSave {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    // NoPreempt implementation
    impl BaseGuard for NoPreempt {
        type State = ();

        #[inline]
        fn acquire() -> Self::State {
            #[cfg(feature = "preempt")]
            crate_interface::call_interface!(crate::guard::KernelGuardIf::disable_preempt);
        }

        #[inline]
        fn release(_state: Self::State) {
            #[cfg(feature = "preempt")]
            crate_interface::call_interface!(crate::guard::KernelGuardIf::enable_preempt);
        }
    }

    impl NoPreempt {
        /// Create a new guard, entering the critical section.
        #[inline]
        pub fn new() -> Self {
            <Self as BaseGuard>::acquire();
            Self
        }
    }

    impl Drop for NoPreempt {
        #[inline]
        fn drop(&mut self) {
            <Self as BaseGuard>::release(())
        }
    }

    impl Default for NoPreempt {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    // NoPreemptIrqSave implementation
    impl BaseGuard for NoPreemptIrqSave {
        type State = usize;

        #[inline]
        fn acquire() -> Self::State {
            // Order: disable preemption first, then IRQs
            #[cfg(feature = "preempt")]
            crate_interface::call_interface!(crate::guard::KernelGuardIf::disable_preempt);

            crate::guard::arch::save_disable()
        }

        #[inline]
        fn release(state: Self::State) {
            // Order: restore IRQs first, then enable preemption
            crate::guard::arch::restore(state);

            #[cfg(feature = "preempt")]
            crate_interface::call_interface!(crate::guard::KernelGuardIf::enable_preempt);
        }
    }

    impl NoPreemptIrqSave {
        /// Create a new guard, entering the critical section.
        #[inline]
        pub fn new() -> Self {
            Self(<Self as BaseGuard>::acquire())
        }
    }

    impl Drop for NoPreemptIrqSave {
        #[inline]
        fn drop(&mut self) {
            <Self as BaseGuard>::release(self.0)
        }
    }

    impl Default for NoPreemptIrqSave {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }
}

// User-mode aliases (all no-ops)
#[cfg(not(target_os = "none"))]
pub use NoOp as IrqSave;
#[cfg(not(target_os = "none"))]
pub use NoOp as NoPreempt;
#[cfg(not(target_os = "none"))]
pub use NoOp as NoPreemptIrqSave;
#[cfg(target_os = "none")]
pub use kernel::*;

#[cfg(feature = "unittest")]
pub mod tests_guard_types {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;

    test_fn! {
        using TestResult;

        fn test_guard_noop_multiple_acquire_release() {
            for _ in 0..100 {
                NoOp::acquire();
                NoOp::release(());
            }
        }
    }

    test_fn! {
        using TestResult;

        fn test_guard_noop_nested() {
            NoOp::acquire();
            NoOp::acquire();
            NoOp::acquire();
            NoOp::release(());
            NoOp::release(());
            NoOp::release(());
        }
    }

    test_fn! {
        using TestResult;

        fn test_irqsave_acquire_release() {
            // Just verify acquire/release work without panicking
            let state = IrqSave::acquire();
            IrqSave::release(state);
        }
    }

    test_fn! {
        using TestResult;

        fn test_nopreempt_acquire_release() {
            NoPreempt::acquire();
            NoPreempt::release(());
        }
    }

    test_fn! {
        using TestResult;

        fn test_nopreempt_irqsave_acquire_release() {
            let state = NoPreemptIrqSave::acquire();
            NoPreemptIrqSave::release(state);
        }
    }

    tests_name!(TEST_GUARD_TYPES;
        test_guard_noop_multiple_acquire_release,
        test_guard_noop_nested,
        test_irqsave_acquire_release,
        test_nopreempt_acquire_release,
        test_nopreempt_irqsave_acquire_release,
    );
}
