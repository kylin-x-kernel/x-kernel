import os
import re

platforms = [
    "aarch64-crosvm-virt",
    "aarch64-peripherals",
    "aarch64-qemu-virt",
    "aarch64-raspi",
    "loongarch64-qemu-virt",
    "riscv64-qemu-virt",
    "x86-csv",
    "x86-pc"
]

def process_file(filepath):
    with open(filepath, 'r') as f:
        lines = f.readlines()
    
    new_lines = []
    changed = False
    
    for line in lines:
        stripped = line.strip()
        
        # Fix: kplat-PLATFORM = ... -> PLATFORM = ...
        # Match "kplat-NAME =" or "kplat-NAME ="
        # Regex: ^kplat-(NAME)\s*=
        
        match_plat = re.match(r'^kplat-([\w-]+)\s*=', stripped)
        if match_plat:
            p_name = match_plat.group(1)
            # Check if p_name is one of our target platforms
            # Only if it is one of the platforms we manage.
            # aarch64-peripherals is one.
            if p_name in platforms:
                # Replace key
                new_line = line.replace(f"kplat-{p_name}", p_name, 1)
                new_lines.append(new_line)
                changed = True
                continue
                
        # Fix: kplat = { ... git = ... } -> kplat = { workspace = true }
        if stripped.startswith("kplat =") and "git =" in stripped:
             new_lines.append('kplat = { workspace = true }\n')
             changed = True
             continue
             
        # Fix: kplat-macros reference if needed (though it seems okay in root now)
        # But if a platform uses it.
        
        new_lines.append(line)
        
    if changed:
        print(f"Fixing dependencies in {filepath}")
        with open(filepath, 'w') as f:
            f.writelines(new_lines)

def main():
    for root, dirs, files in os.walk("platforms"):
        for file in files:
            if file == "Cargo.toml":
                process_file(os.path.join(root, file))

if __name__ == "__main__":
    main()
