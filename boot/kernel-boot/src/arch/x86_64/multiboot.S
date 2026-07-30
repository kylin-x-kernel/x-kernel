# Bootstrapping from 32-bit with the Multiboot2 specification.
#
# Unified Multiboot2 entry for the x86_64 qemu machine and its defconfig variants.
# The {cbit_mask} argument is 0 for non-SEV platforms and (1 << SEV_CBIT_POS)
# for CSV/SEV feature variants on the x86_64 qemu machine.

.section .text.boot
.code32
.global _start32
_start32:
    mov     edi, eax        # arg1: magic: 0x2BADB002
    mov     esi, ebx        # arg2: multiboot info
    jmp     bsp_entry32

.balign 8
.type multiboot2_header, STT_OBJECT
multiboot2_header:
    .long   0xe85250d6
    .long   0                           # i386 / x86 / x86_64
    .long   multiboot2_header_end - multiboot2_header
    .long   -(0xe85250d6 + 0 + (multiboot2_header_end - multiboot2_header))

    # Information request tag (optional):
    #   Mmap(6), Framebuffer(8), AcpiV1(14), AcpiV2(15)
    .short  1
    .short  1
    .long   24
    .long   6
    .long   8
    .long   14
    .long   15

    # Relocatable tag (required):
    #   2 MiB alignment so temporary KIMAGE mappings can use 2 MiB huge pages.
    .short  10
    .short  0
    .long   24
    .long   0x00200000
    .long   0xffffffff
    .long   0x00200000
    .long   2                           # high preference

    # End tag
    .short  0
    .short  0
    .long   8
multiboot2_header_end:

# Common code in 32-bit, prepare states to enter 64-bit.
.macro GET_RUNTIME_PADDR reg, tmp, sym
    call    .Lget_runtime_paddr_\@
.Lget_runtime_paddr_\@:
    pop     \reg
    .set    .Ldelta_runtime_paddr_\@, \sym - .Lget_runtime_paddr_\@
    lea     \reg, [\reg + .Ldelta_runtime_paddr_\@]
.endm

.macro ENTRY32_COMMON
    # set data segment selectors
    mov     ax, 0x18
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax

    # set PAE, PGE bit in CR4
    mov     eax, {cr4}
    mov     cr4, eax

    # load the temporary page table
    GET_RUNTIME_PADDR eax, edx, x86_ap_boot_pml4
    mov     cr3, eax

    # set LME, NXE bit in IA32_EFER
    mov     ecx, {efer_msr}
    mov     edx, 0
    mov     eax, {efer}
    wrmsr

    # set protected mode, write protect, paging bit in CR0
    mov     eax, {cr0}
    mov     cr0, eax
.endm

.macro JMP_LONGMODE target
    GET_RUNTIME_PADDR eax, edx, \target
    push    0x10
    push    eax
    retf
.endm

# Common code in 64-bit
.macro ENTRY64_COMMON
    # clear segment selectors
    xor     ax, ax
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax
.endm

.code32
bsp_entry32:
    GET_RUNTIME_PADDR eax, edx, x86_ap_boot_gdt_desc
    lgdt    [eax]
    ENTRY32_COMMON
    JMP_LONGMODE bsp_entry64

.code32
.global ap_entry32
ap_entry32:
    GET_RUNTIME_PADDR eax, edx, x86_ap_boot_gdt_desc
    lgdt    [eax]
    ENTRY32_COMMON
    JMP_LONGMODE ap_entry64

.code64
.global _start
_start:
    jmp     bsp_entry64

.code64
bsp_entry64:
    ENTRY64_COMMON

    # Detect the actual physical load base at runtime.
    # At this point we are executing via the identity map, so RIP equals the
    # actual physical address of the current instruction.
    # call pushes PA(.Lbsp_pc), pop retrieves it.
    call    .Lbsp_pc
.Lbsp_pc:
    pop     rax                         # rax = actual PA of .Lbsp_pc
    movabs  rcx, offset .Lbsp_pc       # rcx = linked VA of .Lbsp_pc (KIMAGE_VADDR + imm_off)
    # kimage_voffset = linked_VA - actual_PA
    sub     rcx, rax                    # rcx = kimage_voffset
    # Pass kimage_voffset as third argument (rdx) to rust_entry.
    # rdi = magic, rsi = mbi (preserved from _start32 through ENTRY32_COMMON)
    mov     rdx, rcx                    # rdx = kimage_voffset

    # set RSP to boot stack (KIMAGE_VADDR)
    movabs  rsp, offset {boot_stack}
    add     rsp, {boot_stack_size}

    # call rust_entry(magic, mbi, kimage_voffset)
    movabs  rax, offset {entry}
    call    rax
    jmp     .Lhlt

.code64
ap_entry64:
    ENTRY64_COMMON

    # Detect kimage_voffset the same way as bsp_entry64.
    # RSP from ap_start.S is the physical address of the secondary stack top.
    # Adjust it to the kernel-image virtual address.
    call    .Lap_pc
.Lap_pc:
    pop     rax                         # rax = actual PA of .Lap_pc
    movabs  rcx, offset .Lap_pc        # rcx = linked VA of .Lap_pc
    sub     rcx, rax                    # rcx = kimage_voffset
    add     rsp, rcx                    # rsp = PA_stack_top + kimage_voffset = KIMAGE_VA

    # call rust_entry_secondary(magic)
    mov     rdi, {mb_magic}
    movabs  rax, offset {entry_secondary}
    call    rax
    jmp     .Lhlt

.Lhlt:
    hlt
    jmp     .Lhlt

.section .rodata
.balign 8
 .global x86_ap_boot_gdt_desc
x86_ap_boot_gdt_desc:
.Ltmp_gdt_desc:
    .short  .Ltmp_gdt_end - .Ltmp_gdt - 1   # limit
    .long   0                               # base, patched by the BSP before AP startup

.section .data
.balign 16
 .global x86_ap_boot_gdt
x86_ap_boot_gdt:
.Ltmp_gdt:
    .quad 0x0000000000000000    # 0x00: null
    .quad 0x00cf9b000000ffff    # 0x08: code segment (base=0, limit=0xfffff, type=32bit code exec/read, DPL=0, 4k)
    .quad 0x00af9b000000ffff    # 0x10: code segment (base=0, limit=0xfffff, type=64bit code exec/read, DPL=0, 4k)
    .quad 0x00cf93000000ffff    # 0x18: data segment (base=0, limit=0xfffff, type=32bit data read/write, DPL=0, 4k)
.Ltmp_gdt_end:

.balign 4096
 .global x86_ap_boot_pml4
x86_ap_boot_pml4:
.Ltmp_pml4:
    # PML4[0]: 0x0000_0000 ~ 0x7f_ffff_ffff (512 GiB) — identity map
    .quad 0                                 # patched at runtime
    .zero 8 * 255
    # PML4[256]: 0xffff_8000_0000_0000 ~ 0xffff_807f_ffff_ffff (512 GiB) — linear map
    .quad 0                                 # patched at runtime
    .zero 8 * 254
    # PML4[511]: 0xffff_ff80_0000_0000 ~ 0xffff_ffbf_ffff_ffff (512 GiB) — KIMAGE_VADDR
    .quad 0                                 # patched at runtime

# FIXME: may not work on macOS using hvf as the CPU does not support 1GB page (pdpe1gb)
 .global x86_ap_boot_pdpt_low
x86_ap_boot_pdpt_low:
.Ltmp_pdpt_low:
.set i, 0
.rept 512
    .quad 0x40000000 * i | 0x83 | {cbit_mask}  # PRESENT | WRITABLE | HUGE_PAGE | paddr | C-bit
    .set i, i + 1
.endr

 .global x86_ap_boot_pdpt_high
x86_ap_boot_pdpt_high:
.Ltmp_pdpt_high:
.set i, 0
.rept 512
    .quad 0x40000000 * i | 0x83 | {cbit_mask}  # PRESENT | WRITABLE | HUGE_PAGE | paddr | C-bit
    .set i, i + 1
.endr

# PDPT for KIMAGE_VADDR region.
# KIMAGE_VADDR = 0xffff_ff80_0000_0000: PML4[511], PDPT[0].
# PDPT[0] points to a 2MB-granularity PD because the runtime physical load base
# may be only 2 MiB aligned.
 .global x86_ap_boot_pdpt_kimage
x86_ap_boot_pdpt_kimage:
.Ltmp_pdpt_kimage:
    .quad 0                                 # patched at runtime
    .zero 8 * 511

# PD for KIMAGE_VADDR region.
# Patched by the BSP before AP startup to point at 4KB PT pages so AP bootstrap
# keeps working even when the kernel physical load address is not 2MB aligned.
 .global x86_ap_boot_pd_kimage
x86_ap_boot_pd_kimage:
.Ltmp_pd_kimage:
    .zero 8 * 512

# PT pages for the AP KIMAGE bootstrap mapping.
# Reserve the full 1GiB window (512 PT pages) so runtime patching can map the
# actual kernel image with 4KB granularity without assuming any physical alignment.
 .global x86_ap_boot_pt_kimage
x86_ap_boot_pt_kimage:
.Ltmp_pt_kimage:
    .zero 8 * 512 * 512
