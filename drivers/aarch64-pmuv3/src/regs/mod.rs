#[macro_use]
pub mod macros;

pub use pmcr_el0::PMCR_EL0;
mod pmcr_el0;

pub use pmuserenr_el0::PMUSERENR_EL0;
mod pmuserenr_el0;

pub use pmccfiltr_el0::PMCCFILTR_EL0;
mod pmccfiltr_el0;

mod regs;
pub use regs::*;

#[macro_export]
macro_rules! define_pmu_register {
    ($mod_name:ident, $reg_name:ident, $reg_literal:tt) => {
        pub mod $mod_name {
            use tock_registers::interfaces::{Readable, Writeable};
            pub struct Reg;

            impl Readable for Reg {
                type R = ();
                type T = u64;

                sys_coproc_read_raw!(u64, $reg_literal, "x");
            }

            impl Writeable for Reg {
                type R = ();
                type T = u64;

                sys_coproc_write_raw!(u64, $reg_literal, "x");
            }

            pub const $reg_name: Reg = Reg {};
        }
    };
}
