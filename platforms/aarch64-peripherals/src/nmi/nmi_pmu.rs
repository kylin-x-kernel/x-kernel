#[macro_export]
macro_rules! nmi_if_impl {
    ($name:ident) => {
        struct $name;
        use kplat::nm_irq::{NmiHandler, NmiType};
        const CYCLE_COUNTER_INDEX: u32 = 31;
        #[impl_dev_interface]
        impl kplat::nm_irq::NmiIf for $name {
            fn init(threshold: u64) -> bool {
                $crate::gic::set_prio(crate::config::devices::PMU_IRQ, 0);
                $crate::pmu::init_cycle_counter(threshold)
            }

            fn enable() {
                $crate::pmu::enable(CYCLE_COUNTER_INDEX);
            }

            fn disable() {
                $crate::pmu::disable(CYCLE_COUNTER_INDEX);
            }

            fn is_enabled() -> bool {
                $crate::pmu::is_enabled(CYCLE_COUNTER_INDEX)
            }

            fn name() -> &'static str {
                "PMU"
            }

            fn nmi_type() -> NmiType {
                NmiType::PseudoNmi
            }

            fn reg_handler_nmi_handler(handler: NmiHandler) -> bool {
                $crate::pmu::reg_handler_overflow_handler(CYCLE_COUNTER_INDEX, handler)
            }
        }
    };
}
