import os

def process_file(filepath):
    with open(filepath, 'r') as f:
        content = f.read()
    
    replacements = [
        ("RawRange", "MemRange"),
        ("reserved_ram_regions", "rsvd_regions"),
        ("NANOS_PER_SEC", "NS_SEC"),
        ("reg_handler_handler", "register_handler"),
        ("unreg_handler_handler", "unregister_handler"),
        ("TargetCpu::Current { cpu_id }", "TargetCpu::Self_"),
        ("TargetCpu::Other { cpu_id }", "TargetCpu::Specific(cpu_id)"),
        ("TargetCpu::AllExceptCurrent", "TargetCpu::AllButSelf"),
        ("notify_cpu_self", "send_ipi_self"),
        ("notify_cpu(", "send_ipi("), # Careful! This might revert IntrManager impl signature
        # But IntrManager::notify_cpu signature is (id, target).
        # LocalApic::send_ipi(vector, dest).
        # If I revert notify_cpu(, I might break the trait impl.
        # But trait impl is fn notify_cpu(id: usize...)
        # Calls are super::local_apic().notify_cpu(...)
        # So I should change calls to send_ipi.
        (".notify_cpu(", ".send_ipi("),
        (".notify_cpu_all(", ".send_ipi_all("),
        ("sa_dispatch_irqr_kernel", "sa_handler_kernel"), # Retry specific field
        ("sa_handler_kernel_kernel", "sa_handler_kernel"), # If double fix happened
    ]
    
    new_content = content
    for old, new in replacements:
        if old in new_content:
            new_content = new_content.replace(old, new)
            
    # Fix TargetCpu struct usage
    # TargetCpu::AllButSelf { cpu_id, cpu_num } -> ?
    # My definition: AllButSelf { me, total }
    # Old usage: AllExceptCurrent { cpu_id, cpu_num }
    # So cpu_id -> me, cpu_num -> total
    if "TargetCpu::AllButSelf" in new_content:
         new_content = new_content.replace("{ cpu_id, cpu_num }", "{ me: cpu_id, total: cpu_num }")

    if new_content != content:
        print(f"Fixing {filepath}")
        with open(filepath, 'w') as f:
            f.write(new_content)

def main():
    files = [
        "platforms/x86-pc/src/mem.rs", 
        "platforms/x86-pc/src/time.rs",
        "platforms/x86-pc/src/apic.rs",
        "process/starry-signal/src/action.rs"
    ]
    for f in files:
        if os.path.exists(f):
            process_file(f)

if __name__ == "__main__":
    main()
