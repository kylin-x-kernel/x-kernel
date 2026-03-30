// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub(crate) fn init() {
    let rsdp_addr = ::acpi::rsdp_addr().unwrap_or(0);
    kprintln!("ACPI init: rsdp_addr={:#x}", rsdp_addr);
    if let Some((madt, entries)) = ::acpi::find_madt_from_init() {
        kprintln!(
            "ACPI MADT: lapic={:#x} flags={:#x}",
            madt.local_apic_address,
            madt.flags
        );
        for entry in entries {
            match entry {
                ::acpi::MadtEntry::LocalApic(cpu) if cpu.enabled() => {
                    kprintln!(
                        "ACPI MADT LAPIC: uid={} apic_id={}",
                        cpu.processor_uid,
                        cpu.apic_id
                    );
                }
                ::acpi::MadtEntry::IoApic(io_apic) => {
                    kprintln!(
                        "ACPI MADT IOAPIC: id={} addr={:#x} gsi_base={}",
                        io_apic.id,
                        io_apic.address,
                        io_apic.global_system_interrupt_base
                    );
                }
                _ => {}
            }
        }
    } else {
        kprintln!("ACPI MADT not found");
    }

    let Some(mcfg) = ::acpi::find_mcfg_from_init() else {
        kprintln!("ACPI MCFG not found, fallback to static PCI ECAM config");
        return;
    };

    if mcfg.pci_segment != 0 {
        kprintln!(
            "ignoring ACPI MCFG segment {} base={:#x}",
            mcfg.pci_segment,
            mcfg.base_address
        );
        return;
    }

    kprintln!(
        "ACPI MCFG: ecam={:#x}, buses={:#x}..={:#x}",
        mcfg.base_address,
        mcfg.start_bus,
        mcfg.end_bus
    );
    kdriver::set_pci_config_space(mcfg.base_address, mcfg.end_bus);
}
