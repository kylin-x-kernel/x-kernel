OUTPUT_ARCH(i386:x86-64)
ENTRY(_start32)

SECTIONS
{
    . = 0x200000;
    __image_start = .;

    .text : ALIGN(4K) {
        *(.text.boot)
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
    }

    .bss : ALIGN(4K) {
        *(.bss.stack)
        *(.bss .bss.*)
        *(COMMON)
    }

    __image_end = .;

    /DISCARD/ : {
        *(.comment) *(.gnu*) *(.note*) *(.eh_frame*)
    }
}
