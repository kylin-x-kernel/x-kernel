# Boot application processors into the protected mode.

# Each non-boot CPU ("AP") is started up in response to a STARTUP
# IPI from the boot CPU.  Section B.4.2 of the Multi-Processor
# Specification says that the AP will start in real mode with CS:IP
# set to XY00:0000, where XY is an 8-bit value sent with the
# STARTUP. Thus this code must start at a 4096-byte boundary.
#
# The BSP copies this blob to a runtime-chosen low-memory page (< 1 MiB), then
# patches the protected-mode far-jump target and the temporary GDT base in
# place before sending the SIPI.

.equ stack_ptr, 0xff0
.equ entry_ptr, 0xff8
.equ ap_tmp_gdt_desc_ptr, .Lap_tmp_gdt_desc - ap_start

.section .text
.code16
.p2align 12
.global ap_start
ap_start:
    cli
    wbinvd

    mov     ax, cs
    mov     ds, ax
    mov     es, ax
    mov     ss, ax
    mov     fs, ax
    mov     gs, ax

    # load the 64-bit GDT
    lgdt    [ap_tmp_gdt_desc_ptr]

    # switch to protected-mode
    mov     eax, cr0
    or      eax, (1 << 0)
    mov     cr0, eax

    # Far jump to the page-local 32-bit entry after patching in its runtime
    # linear address. 0x8 is the temporary 32-bit code selector.
    .byte   0x66, 0xea
    .global x86_ap_patch_protected_entry
x86_ap_patch_protected_entry:
    .long   0
    .word   0x8

.code32
.global x86_ap_trampoline_protected_entry
x86_ap_trampoline_protected_entry:
ap_start32:
    mov     esp, [stack_ptr]
    mov     eax, [entry_ptr]
    # ap_entry32 immediately dereferences kernel-image symbols. Reload the flat
    # data segments before that jump; keeping the low-page segment cache here
    # makes lgdt [eax] fetch the GDT descriptor from the wrong address.
    # Use DX so the patched entry pointer in EAX stays intact.
    mov     dx, 0x18
    mov     ds, dx
    mov     es, dx
    mov     ss, dx
    mov     fs, dx
    mov     gs, dx
    jmp     eax

.balign 8
# .type multiboot_header, STT_OBJECT
.Lap_tmp_gdt_desc:
    .short .Lap_tmp_gdt_end - .Lap_tmp_gdt - 1  # limit
    .global x86_ap_patch_gdt_base
x86_ap_patch_gdt_base:
    .long 0                                     # runtime linear base of .Lap_tmp_gdt

.balign 16
.global x86_ap_trampoline_gdt
x86_ap_trampoline_gdt:
.Lap_tmp_gdt:
    .quad 0x0000000000000000    # 0x00: null
    .quad 0x00cf9b000000ffff    # 0x08: code segment (base=0, limit=0xfffff, type=32bit code exec/read, DPL=0, 4k)
    .quad 0x00af9b000000ffff    # 0x10: code segment (base=0, limit=0xfffff, type=64bit code exec/read, DPL=0, 4k)
    .quad 0x00cf93000000ffff    # 0x18: data segment (base=0, limit=0xfffff, type=32bit data read/write, DPL=0, 4k)
.Lap_tmp_gdt_end:

.code64
# 0x7000
.p2align 12
.global ap_end
ap_end:
