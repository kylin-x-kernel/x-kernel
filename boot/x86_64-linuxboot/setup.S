// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

.intel_syntax noprefix

.set BOOT_PARAMS_ADDR,        0x7000
.set BOOT_PARAMS_SEG,         0x0700
.set BOOT_PARAMS_SENTINEL,    0x01ef
.set BOOT_PARAMS_ACPI_RSDP_ADDR, 0x0070
.set BOOT_PARAMS_E820_ENTRIES, 0x01e8
.set BOOT_PARAMS_E820_TABLE,  0x02d0
.set DIRECT_BOOT_STACK,       0x8000
.set PROTECTED_KERNEL_LOAD,   0x00100000
.set LINUX_BOOT_MAGIC,        0x584b4c42
.set BDA_EBDA_SEG_PTR,        0x040e
.set BIOS_RSDP_SEARCH_START,  0x000e0000
.set BIOS_RSDP_SEARCH_SIZE,   0x00020000

.code16
.section .setup, "ax"
.org 0x01ef

sentinel:
    .byte   0xff
    .byte   0x00

hdr:
    .byte   0x04                    // setup_sects (patched at image build time)
    .word   0x0000                  // root_flags
    .long   0x00000000              // syssize (patched at image build time)
    .word   0x0000                  // ram_size
    .word   0x0000                  // vid_mode
    .word   0x0000                  // root_dev
    .word   0xaa55                  // boot_flag

.globl _start
.set SENTINEL_OFF, sentinel - _start
.set HEADER_SIZE, header_end - sentinel
.set GDT_OFF, gdt - _start
.set GDT_DESC_OFF, gdt_desc - _start
.set GDT_LIMIT, gdt_end - gdt - 1
_start:
    .byte   0xeb
    .byte   start_of_setup - 1f
1:
    .ascii  "HdrS"
    .word   0x020f                  // protocol version
    .long   0x00000000              // realmode_swtch
    .word   0x0000                  // start_sys_seg
    .word   0x0000                  // kernel_version
    .byte   0xff                    // type_of_loader
    .byte   0x81                    // LOADED_HIGH | CAN_USE_HEAP
    .word   0x0000                  // setup_move_size
    .long   PROTECTED_KERNEL_LOAD   // code32_start
    .long   0x00000000              // ramdisk_image
    .long   0x00000000              // ramdisk_size
    .long   0x00000000              // bootsect_kludge
    .word   0xde00                  // heap_end_ptr
    .byte   0x00                    // ext_loader_ver
    .byte   0x00                    // ext_loader_type
    .long   0x00000000              // cmd_line_ptr
    .long   0xffffffff              // initrd_addr_max
    .long   0x00200000              // kernel_alignment
    .byte   0x01                    // relocatable_kernel
    .byte   21                      // min_alignment
    .word   0x0001                  // xloadflags: XLF_KERNEL_64
    .long   0x00000800              // cmdline_size (2 KiB command line buffer)
    .long   0x00000000              // hardware_subarch
    .quad   0x0000000000000000      // hardware_subarch_data
payload_offset_field:
    .long   0x00000000              // payload_offset (patched at image build time)
payload_length_field:
    .long   0x00000000              // payload_length (patched at image build time)
    .quad   0x0000000000000000      // setup_data
    .quad   PROTECTED_KERNEL_LOAD   // pref_address
init_size_field:
    .long   0x00000000              // init_size (patched at image build time)
    .long   0x00000000              // handover_offset
    .long   0x00000000              // kernel_info_offset

header_end:

start_of_setup:
    cli
    cld

    mov     ax, cs
    mov     ds, ax
    xor     ax, ax
    mov     ss, ax
    mov     sp, DIRECT_BOOT_STACK

    mov     ax, BOOT_PARAMS_SEG
    mov     es, ax
    xor     di, di
    xor     ax, ax
    mov     cx, 0x0800
    rep     stosw

    mov     ax, cs
    mov     ds, ax
    mov     ax, BOOT_PARAMS_SEG
    mov     es, ax
    mov     si, SENTINEL_OFF
    mov     di, BOOT_PARAMS_SENTINEL
    mov     cx, HEADER_SIZE
    rep     movsb

    mov     ax, BOOT_PARAMS_SEG
    mov     es, ax
    mov     byte ptr es:[BOOT_PARAMS_E820_ENTRIES], 0
    mov     di, BOOT_PARAMS_E820_TABLE
    xor     ebx, ebx
    xor     bp, bp

.Le820_loop:
    mov     eax, 0xe820
    mov     edx, 0x534d4150
    mov     ecx, 24
    mov     dword ptr es:[di + 20], 1
    int     0x15
    jc      .Le820_done
    cmp     eax, 0x534d4150
    jne     .Le820_done
    cmp     ecx, 20
    jb      .Le820_next
    inc     bp
    add     di, 24
    cmp     bp, 128
    jae     .Le820_done
.Le820_next:
    test    ebx, ebx
    jne     .Le820_loop

.Le820_done:
    mov     ax, BOOT_PARAMS_SEG
    mov     es, ax
    mov     ax, bp
    mov     byte ptr es:[BOOT_PARAMS_E820_ENTRIES], al
    call    detect_rsdp
    mov     dword ptr es:[BOOT_PARAMS_ACPI_RSDP_ADDR], eax
    mov     dword ptr es:[BOOT_PARAMS_ACPI_RSDP_ADDR + 4], 0

    mov     ax, cs
    mov     ds, ax
    xor     eax, eax
    mov     ax, cs
    shl     eax, 4
    add     eax, GDT_OFF
    mov     edx, eax
    mov     ax, GDT_LIMIT
    mov     word ptr [GDT_DESC_OFF], ax
    mov     dword ptr [GDT_DESC_OFF + 2], edx
    sub     edx, GDT_OFF
    mov     word ptr [GDT_OFF + 18], dx
    shr     edx, 16
    mov     byte ptr [GDT_OFF + 20], dl
    mov     byte ptr [GDT_OFF + 23], dh
    .byte   0x66
    lgdt    [GDT_DESC_OFF]

    mov     eax, cr0
    or      eax, 1
    mov     cr0, eax
    ljmp    0x10, offset protected_mode_entry - _start

detect_rsdp:
    push    ds
    xor     ax, ax
    mov     ds, ax
    mov     ax, word ptr ds:[BDA_EBDA_SEG_PTR]
    test    ax, ax
    jz      .Ldetect_rsdp_high
    mov     cx, 0x40
    call    scan_rsdp_segments
    test    eax, eax
    jnz     .Ldetect_rsdp_done

.Ldetect_rsdp_high:
    mov     ax, BIOS_RSDP_SEARCH_START >> 4
    mov     cx, BIOS_RSDP_SEARCH_SIZE >> 4
    call    scan_rsdp_segments

.Ldetect_rsdp_done:
    pop     ds
    ret

scan_rsdp_segments:
    push    si

.Lscan_rsdp_next:
    test    cx, cx
    jz      .Lscan_rsdp_not_found
    mov     ds, ax
    xor     si, si
    cmp     word ptr ds:[si], 0x5352
    jne     .Lscan_rsdp_advance
    cmp     word ptr ds:[si + 2], 0x2044
    jne     .Lscan_rsdp_advance
    cmp     word ptr ds:[si + 4], 0x5450
    jne     .Lscan_rsdp_advance
    cmp     word ptr ds:[si + 6], 0x2052
    jne     .Lscan_rsdp_advance
    call    rsdp_checksum_ok
    jne     .Lscan_rsdp_advance
    movzx   eax, ax
    shl     eax, 4
    pop     si
    ret

.Lscan_rsdp_advance:
    inc     ax
    dec     cx
    jmp     .Lscan_rsdp_next

.Lscan_rsdp_not_found:
    xor     eax, eax
    pop     si
    ret

rsdp_checksum_ok:
    push    cx
    push    si
    xor     dx, dx
    mov     cx, 20

.Lrsdp_checksum_next:
    add     dl, byte ptr ds:[si]
    inc     si
    loop    .Lrsdp_checksum_next
    or      dl, dl
    pop     si
    pop     cx
    ret

.code32
protected_mode_entry:
    mov     ax, 0x18
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax
    mov     ss, ax
    mov     esp, DIRECT_BOOT_STACK

    mov     eax, LINUX_BOOT_MAGIC
    mov     ebx, BOOT_PARAMS_ADDR
    xor     ebp, ebp
    xor     edi, edi
    xor     esi, esi
    xor     edx, edx
    push    0x20
    push    PROTECTED_KERNEL_LOAD
    retf

.balign 8
gdt_desc:
    .word   gdt_end - gdt - 1
    .long   0

.balign 8
gdt:
    .quad   0x0000000000000000
    .quad   0x0000000000000000
    .quad   0x00cf9b000000ffff
    .quad   0x00cf93000000ffff
    .quad   0x00cf9b000000ffff
gdt_end:
