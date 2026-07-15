// Shared exception-table macro for user-access assembly helpers.
// Include once per global_asm! block before any .S file that uses _asm_extable.

.ifndef _ASM_EXTABLE_DEFINED
.equ _ASM_EXTABLE_DEFINED, 1

.macro _asm_extable, from, to
    .pushsection __ex_table, "a"
    .balign 8
    .quad \from
    .quad \to
    .popsection
.endm

.endif
