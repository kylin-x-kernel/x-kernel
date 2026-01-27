import os

replacements = {
    # Crate names and macros
    "axplat": "kplat",
    "kyplat-macros": "kplat-macros",
    "kyplat_macros": "kplat_macros",
    "def_plat_interface": "device_interface",
    "impl_plat_interface": "impl_dev_interface",

    # Modules
    "kplat::mem": "kplat::memory",
    "axplat::mem": "kplat::memory",
    "kplat::console": "kplat::io",
    "axplat::console": "kplat::io",
    "kplat::init": "kplat::boot",
    "axplat::init": "kplat::boot",
    "kplat::irq": "kplat::interrupts",
    "axplat::irq": "kplat::interrupts",
    "kplat::nmi": "kplat::nm_irq",
    "axplat::nmi": "kplat::nm_irq",
    "kplat::percpu": "kplat::cpu",
    "axplat::percpu": "kplat::cpu",
    "kplat::pmu": "kplat::perf",
    "axplat::pmu": "kplat::perf",
    "kplat::power": "kplat::sys",
    "axplat::power": "kplat::sys",
    "kplat::time": "kplat::timer",
    "axplat::time": "kplat::timer",

    # Traits and methods
    "InitIf": "BootHandler",
    "init_early": "early_init",
    "init_early_secondary": "early_init_ap",
    "init_later": "final_init",
    "init_later_secondary": "final_init_ap",
    
    "ConsoleIf": "Terminal",
    "write_bytes": "write_data",
    "write_bytes_force": "write_data_atomic",
    "read_bytes": "read_data",
    "irq_num": "interrupt_id",
    
    "MemIf": "HwMemory",
    "phys_ram_ranges": "ram_regions",
    "reserved_phys_ram_ranges": "rsvd_regions",
    "mmio_ranges": "mmio_regions",
    "phys_to_virt": "p2v",
    "virt_to_phys": "v2p",
    "kernel_aspace": "kernel_layout",
    "PhysMemRegion": "MemoryRegion",
    "MemRegionFlags": "MemFlags",
    
    "IrqIf": "IntrManager",
    "set_enable": "enable",
    "register": "reg_handler",
    "unregister": "unreg_handler",
    "handle": "dispatch_irq",
    "send_ipi": "notify_cpu",
    "set_priority": "set_prio",
    "local_irq_save_and_disable": "save_disable",
    "local_irq_restore": "restore",
    "enable_irqs": "enable_local",
    "disable_irqs": "disable_local",
    "irqs_enabled": "is_enabled",
    
    "TimeIf": "GlobalTimer",
    "current_ticks": "now_ticks",
    "ticks_to_nanos": "t2ns",
    "timer_frequency": "freq",
    "nanos_to_ticks": "ns2t",
    "epochoffset_nanos": "offset_ns",
    "set_oneshot_timer": "arm_timer",
    
    "PowerIf": "SysCtrl",
    "cpu_boot": "boot_ap",
    "system_off": "shutdown",
    
    "PmuIf": "PerfMgr",
    "handle_overflows": "on_overflow",
    "register_overflow_handler": "reg_cb",
}

def process_file(filepath):
    try:
        with open(filepath, 'r', encoding='utf-8') as f:
            content = f.read()
        
        new_content = content
        for old, new in replacements.items():
            new_content = new_content.replace(old, new)
            
        if new_content != content:
            print(f"Refactoring {filepath}")
            with open(filepath, 'w', encoding='utf-8') as f:
                f.write(new_content)
    except Exception as e:
        print(f"Error processing {filepath}: {e}")

def main():
    target_dir = "."
    # We skip platforms because we already did it (mostly), but running again is harmless as replacements are usually idempotent or already done.
    # But we specifically want to hit arch/axhal, core, etc.
    
    exclude_dirs = [".git", "target", "scripts"]
    
    for root, dirs, files in os.walk(target_dir):
        # Modification of dirs in place to exclude
        dirs[:] = [d for d in dirs if d not in exclude_dirs]
        
        for file in files:
            if file.endswith(".rs"):
                process_file(os.path.join(root, file))

if __name__ == "__main__":
    main()
