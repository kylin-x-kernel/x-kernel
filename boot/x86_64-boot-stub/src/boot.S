.section .text.boot
.code32
.macro SERIAL_PUTC imm
1:
    mov     dx, 0x3fd
    in      al, dx
    test    al, 0x20
    jz      1b
    mov     dx, 0x3f8
    mov     al, \imm
    out     dx, al
.endm

.macro SERIAL_PUTC64 imm
    push    rax
    push    rdx
1:
    mov     dx, 0x3fd
    in      al, dx
    test    al, 0x20
    jz      1b
    mov     dx, 0x3f8
    mov     al, \imm
    out     dx, al
    pop     rdx
    pop     rax
.endm

.global _start32
_start32:
    cmp     eax, {mb_magic}
    je      1f
    cmp     eax, {linux_magic}
    je      2f
    test    esi, esi
    jz      .Lboot_hlt
    cmp     dword ptr [esi + 0x202], 0x53726448
    jne     .Lboot_hlt
    mov     edi, {linux_magic}
    jmp     bsp_entry32
1:
    mov     edi, eax
    mov     esi, ebx
    jmp     bsp_entry32
2:
    mov     edi, eax
    mov     esi, ebx
    jmp     bsp_entry32

.Lboot_hlt:
    hlt
    jmp     .Lboot_hlt

.balign 8
.type multiboot2_header, STT_OBJECT
multiboot2_header:
    .long   0xe85250d6
    .long   0
    .long   multiboot2_header_end - multiboot2_header
    .long   -(0xe85250d6 + 0 + (multiboot2_header_end - multiboot2_header))

    .short  1
    .short  1
    .long   16
    .long   6
    .long   17

    .short  10
    .short  0
    .long   24
    .long   0x00200000
    .long   0xffffffff
    .long   0x00200000
    .long   2

    .short  0
    .short  0
    .long   8
multiboot2_header_end:

.macro GET_RUNTIME_PADDR reg, tmp, sym
    call    .Lget_runtime_paddr_\@
.Lruntime_delta_\@:
    .long   \sym - .Lruntime_delta_\@
.Lget_runtime_paddr_\@:
    pop     \reg
    mov     \tmp, [\reg]
    add     \reg, \tmp
.endm

.macro ENTRY32_COMMON
    mov     ax, 0x18
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax

    mov     eax, {cr4}
    mov     cr4, eax

    GET_RUNTIME_PADDR eax, edx, .Ltmp_pml4
    mov     cr3, eax

    mov     ecx, {efer_msr}
    mov     edx, 0
    mov     eax, {efer}
    wrmsr

    mov     eax, {cr0}
    mov     cr0, eax
.endm

.macro JMP_LONGMODE target
    GET_RUNTIME_PADDR eax, edx, \target
    push    0x10
    push    eax
    retf
.endm

.macro ENTRY64_COMMON
    xor     ax, ax
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax
.endm

.code32
bsp_entry32:
    GET_RUNTIME_PADDR eax, ecx, .Ltmp_gdt_desc
    GET_RUNTIME_PADDR edx, ecx, .Ltmp_gdt
    mov     [eax + 2], edx

    GET_RUNTIME_PADDR eax, ecx, .Ltmp_pml4
    GET_RUNTIME_PADDR edx, ecx, .Ltmp_pdpt_low
    mov     ecx, edx
    or      ecx, 0x3
    mov     [eax], ecx
    mov     dword ptr [eax + 4], 0

    GET_RUNTIME_PADDR edx, ecx, .Ltmp_pdpt_high
    mov     ecx, edx
    or      ecx, 0x3
    mov     [eax + 8 * 256], ecx
    mov     dword ptr [eax + 8 * 256 + 4], 0

    GET_RUNTIME_PADDR eax, ecx, .Ltmp_gdt_desc
    lgdt    [eax]
    ENTRY32_COMMON
    JMP_LONGMODE bsp_entry64

.code64
bsp_entry64:
    ENTRY64_COMMON
    mov     r12, rdi
    mov     r13, rsi
    lea     r14, [rip + __image_start]
    mov     rsi, r14
    mov     rdi, [rip + .Llink_image_start]
    cmp     rsi, rdi
    je      bsp_entry64_relocated
    cmp     r12d, {mb_magic}
    jne     1f
    call    preserve_module_tag
    jmp     2f
1:
    cmp     r12d, {linux_magic}
    jne     2f
    call    preserve_linux_payload
2:
    mov     rsi, r14
    mov     rdi, [rip + .Llink_image_start]
    mov     rcx, [rip + .Llink_image_end]
    sub     rcx, rdi
    cld
    rep     movsb
    mov     rax, [rip + .Llink_relocated_entry]
    jmp     rax

bsp_entry64_relocated:
    mov     rdi, r12
    mov     rsi, r13
    mov     rdx, r14
    lea     rsp, [rip + {boot_stack}]
    add     rsp, {boot_stack_size}
    lea     rax, [rip + {entry}]
    call    rax

.Lhlt:
    hlt
    jmp     .Lhlt

.type preserve_module_tag, STT_FUNC
preserve_module_tag:
    mov     rbx, r13
    add     rbx, 8

.Lfind_module_tag:
    mov     eax, dword ptr [rbx]
    test    eax, eax
    je      .Lpreserve_done
    cmp     eax, 3
    je      .Lmove_module_if_needed
    mov     ecx, dword ptr [rbx + 4]
    add     rcx, 7
    and     rcx, -8
    add     rbx, rcx
    jmp     .Lfind_module_tag

.Lmove_module_if_needed:
    mov     r8d, dword ptr [rbx + 8]
    mov     r9d, dword ptr [rbx + 12]
    mov     r10, [rip + .Llink_image_start]
    mov     r11, [rip + .Llink_image_end]
    cmp     r9, r10
    jbe     .Lpreserve_done
    cmp     r8, r11
    jae     .Lpreserve_done

    mov     rdx, r9
    cmp     rdx, r11
    cmovb   rdx, r11
    add     rdx, 0x1fffff
    and     rdx, -0x200000

    mov     rax, r9
    sub     rax, r8
    mov     rcx, [rip + .Llink_image_end]
    sub     rcx, [rip + .Llink_image_start]
    add     rcx, r14
    mov     rsi, rdx
    add     rsi, rax
    cmp     rsi, r14
    jbe     .Lmodule_dst_ready
    cmp     rcx, rdx
    jbe     .Lmodule_dst_ready
    mov     rdx, rcx
    add     rdx, 0x1fffff
    and     rdx, -0x200000

.Lmodule_dst_ready:
    mov     rcx, rax
    mov     rsi, r8
    mov     rdi, rdx
    cld
    rep     movsb

    mov     dword ptr [rbx + 8], edx
    add     eax, edx
    mov     dword ptr [rbx + 12], eax

.Lpreserve_done:
    ret

.type preserve_linux_payload, STT_FUNC
preserve_linux_payload:
    mov     r8d, dword ptr [r13 + 0x248]
    mov     r9d, dword ptr [r13 + 0x24c]
    test    r9d, r9d
    je      .Lpreserve_linux_done

    add     r8, r14
    lea     r9, [r8 + r9]
    mov     r10, [rip + .Llink_image_start]
    mov     r11, [rip + .Llink_image_end]
    cmp     r9, r10
    jbe     .Lpreserve_linux_done
    cmp     r8, r11
    jae     .Lpreserve_linux_done

    mov     rdx, r9
    cmp     rdx, r11
    cmovb   rdx, r11
    add     rdx, 0x1fffff
    and     rdx, -0x200000

    mov     rcx, r9
    sub     rcx, r8
    mov     rsi, r8
    mov     rdi, rdx
    cld
    rep     movsb

    sub     edx, r14d
    mov     dword ptr [r13 + 0x248], edx

.Lpreserve_linux_done:
    ret

.section .rodata
.balign 8
.Llink_image_start:
    .quad   __image_start
.Llink_image_end:
    .quad   __image_end
.Llink_relocated_entry:
    .quad   bsp_entry64_relocated

.balign 8
.Ltmp_gdt_desc:
    .short  .Ltmp_gdt_end - .Ltmp_gdt - 1
    .long   0

.section .data
.balign 16
.Ltmp_gdt:
    .quad 0x0000000000000000
    .quad 0x00cf9b000000ffff
    .quad 0x00af9b000000ffff
    .quad 0x00cf93000000ffff
.Ltmp_gdt_end:

.balign 4096
.Ltmp_pml4:
    .quad 0
    .zero 8 * 255
    .quad 0
    .zero 8 * 255

.Ltmp_pdpt_low:
.set i, 0
.rept 512
    .quad 0x40000000 * i | 0x83
    .set i, i + 1
.endr

.Ltmp_pdpt_high:
.set i, 0
.rept 512
    .quad 0x40000000 * i | 0x83
    .set i, i + 1
.endr
