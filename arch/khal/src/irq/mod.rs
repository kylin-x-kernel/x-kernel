// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt management.

mod desc;
mod domain;
mod manager;

pub use desc::{
    GIC_ROOT_DOMAIN, Hwirq, IO_APIC_DOMAIN, IntoIrqDesc, IrqAffinity, IrqController, IrqDesc,
    IrqDomainId, IrqEvent, IrqFlags, IrqHandler, IrqPolarity, IrqSource, IrqTrigger,
    PLIC_ROOT_DOMAIN, Virq, gic_edge_irq_desc, gic_irq_desc, gic_level_irq_desc, io_apic_irq_desc,
    plic_irq_desc,
};
pub use domain::IrqRef;
pub use manager::*;
