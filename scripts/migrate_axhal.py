import os

replacements = [
    # Memory API
    ("reserved_ram_regions", "rsvd_regions"),
    ("total_ram_size", "total_ram"),
    ("check_sorted_ranges_overlap", "check_overlap"),
    ("ranges_difference", "sub_ranges"),
    ("MemFlags::RESERVED", "MemFlags::RSVD"),
    ("MemFlags::READ", "MemFlags::R"),
    ("MemFlags::WRITE", "MemFlags::W"),
    ("MemFlags::EXECUTE", "MemFlags::X"),
    ("MemoryRegion::new_reserved", "MemoryRegion::new_rsvd"),
    
    # Timer API
    ("MICROS_PER_SEC", "US_SEC"),
    ("MILLIS_PER_SEC", "MS_SEC"),
    ("NANOS_PER_MICROS", "NS_US"),
    ("NANOS_PER_MILLIS", "NS_MS"),
    ("NANOS_PER_SEC", "NS_SEC"),
    ("monotonic_time", "now"),
    ("monotonic_time_nanos", "now_ns"),
    ("wall_time", "wall"),
    ("wall_time_nanos", "wall_ns"),
    ("busy_wait", "spin_wait"),
    ("busy_wait_until", "spin_until"),
    
    # Interrupt API
    # "register" is generic, might be dangerous. But in axhal/src/irq.rs imports:
    ("use kplat::interrupts::{", "use kplat::interrupts::{"), # Context anchor not useful for replace
    (", register,", ", reg_handler as register,"), # Alias import to avoid code change? 
    # Or change usage.
    # In irq.rs: pub fn register_handler(...) { kplat::interrupts::register(...) }
    # So replacing "register" with "reg_handler" in calls.
    # But "register" appears in "register_handler" function name.
    # "kplat::interrupts::register" -> "kplat::interrupts::reg_handler".
    
    # Let's replace import first
    ("use kplat::interrupts::register;", "use kplat::interrupts::reg_handler as register;"),
    ("use kplat::interrupts::unregister;", "use kplat::interrupts::unreg_handler as unregister;"),
    # If it's a list:
    (" register,", " reg_handler as register,"), 
    (" unregister,", " unreg_handler as unregister,"),

    # Platform crates
    ("extern crate kplat_x86_pc", "extern crate x86_pc"),
    ("extern crate kplat_aarch64_qemu_virt", "extern crate aarch64_qemu_virt"),
    ("extern crate kplat_riscv64_qemu_virt", "extern crate riscv64_qemu_virt"),
    ("extern crate kplat_loongarch64_qemu_virt", "extern crate loongarch64_qemu_virt"),
    
    # MemFlags fix (associated consts)
    # The previous simple string replace handles MemFlags::RESERVED -> MemFlags::RSVD
    
    # misc
    ("kplat::memory::RawRange", "kplat::memory::MemRange"),
    ("RawRange", "MemRange"), # Hope no collision
    
    # Fix duration import
    ("use kplat::timer::Duration;", "use core::time::Duration;"),
    ("use kplat::timer::{Duration,", "use core::time::Duration;\nuse kplat::timer::{"),
]

def process_file(filepath):
    with open(filepath, 'r') as f:
        content = f.read()
    
    new_content = content
    for old, new in replacements:
        new_content = new_content.replace(old, new)
        
    if new_content != content:
        print(f"Fixing {filepath}")
        with open(filepath, 'w') as f:
            f.write(new_content)

def main():
    root_dir = "arch/axhal"
    for root, dirs, files in os.walk(root_dir):
        for file in files:
            if file.endswith(".rs"):
                process_file(os.path.join(root, file))

if __name__ == "__main__":
    main()
