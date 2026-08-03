OUTPUT_ARCH(%ARCH%)

BASE_ADDRESS = %KIMAGE_VADDR%;

SECTIONS
{
    . = BASE_ADDRESS;
    _skernel = .;
    .head.text : {
        _text = .;
        KEEP(*(.head.text))
    }

    .idmap.text ALIGN(64) : {
        _idmap_text = .;
        KEEP(*(.idmap.text))
        . = ALIGN(4K);
        _idmap_text_end = .;
    }

    .text ALIGN(4K) : {
        _stext = .;
        *(.text.boot)
        *(.text .text.*)
        *(.ltext .ltext.*)
        . = ALIGN(4K);
        _etext = .;
    }

    .rodata ALIGN(4K) : {
        _srodata = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        *(.sdata2 .sdata2.*)
    }

    .note.xkernel.build-info ALIGN(4) : {
        LONG(8);
        LONG(%BUILD_INFO_SIZE%);
        LONG(0x584b0001);
        BYTE(0x58); BYTE(0x4b); BYTE(0x45); BYTE(0x52);
        BYTE(0x4e); BYTE(0x45); BYTE(0x4c); BYTE(0);
        __xkernel_build_info_start = .;
        BYTE(0);
        . = __xkernel_build_info_start + %BUILD_INFO_SIZE%;
        __xkernel_build_info_end = .;
    }

    .note.gnu.build-id ALIGN(4) : {
        __xkernel_build_id_note_start = .;
        LONG(4);
        LONG(32);
        LONG(3);
        BYTE(0x47); BYTE(0x4e); BYTE(0x55); BYTE(0);
        __xkernel_build_id_start = .;
        . += 32;
        __xkernel_build_id_end = .;
        __xkernel_build_id_note_end = .;
    }

    %DWARF%

    . = ALIGN(4K);
    _erodata = .;

    .data ALIGN(4K) : {
        _sdata = .;
        *(.data.boot_page_table)
        . = ALIGN(4K);
        *(.data .data.*)
        *(.ldata .ldata.*)
        *(.sdata .sdata.*)

        . = ALIGN(0x10);
        __init_array_start = .;
        KEEP(*(.init_array .init_array.*))
        __init_array_end = .;

        *(.got .got.*)

        . = ALIGN(0x10);
        __sdriver_register = .;
        KEEP(*(.driver.register*))
        __edriver_register = .;

        . = ALIGN(0x10);
        _ex_table_start = .;
        KEEP(*(__ex_table))
        _ex_table_end = .;
    }

    .tracepoint :
    {
        __start_tracepoint = .;
        KEEP(*(.tracepoint))
        __stop_tracepoint = .;
    }

    .__static_keys :
    {
        __start___static_keys = .;
        KEEP(*(__static_keys))
        __stop___static_keys = .;
    }

    .tdata ALIGN(0x10) : {
        _stdata = .;
        *(.tdata .tdata.*)
        _etdata = .;
    }

    .tbss ALIGN(0x10) : {
        _stbss = .;
        *(.tbss .tbss.*)
        *(.tcommon)
        _etbss = .;
    }

    . = ALIGN(4K);
    _percpu_start = .;
    _percpu_end = _percpu_start + SIZEOF(.percpu);
    .percpu 0x0 : AT(_percpu_start) {
        _percpu_load_start = .;
        *(.percpu .percpu.*)
        _percpu_load_end = .;
        . = _percpu_load_start + ALIGN(64) * %NR_CPUS%;
    }
    . = _percpu_end;

    . = ALIGN(4K);
    _edata = .;

    .bss ALIGN(4K) : {
        boot_stack = .;
        *(.bss.stack)
        . = ALIGN(4K);
        boot_stack_top = .;

        _sbss = .;
        *(.bss .bss.*)
        *(.lbss .lbss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        . = ALIGN(4K);
        _ebss = .;
    }

    _ekernel = .;

    /DISCARD/ : {
        *(.comment) *(.gnu*) *(.note*) *(.eh_frame*)
    }
}

SECTIONS {
    linkme_IRQ : { KEEP(*(linkme_IRQ)) }
    linkm2_IRQ : { KEEP(*(linkm2_IRQ)) }
    linkme_NMI : { KEEP(*(linkme_NMI)) }
    linkm2_NMI : { KEEP(*(linkm2_NMI)) }
    linkme_PAGE_FAULT : { KEEP(*(linkme_PAGE_FAULT)) }
    linkm2_PAGE_FAULT : { KEEP(*(linkm2_PAGE_FAULT)) }
    linkme_INIT_TRAP : { KEEP(*(linkme_INIT_TRAP)) }
    linkm2_INIT_TRAP : { KEEP(*(linkm2_INIT_TRAP)) }
    linkme_LOCK_CLASSES : { KEEP(*(linkme_LOCK_CLASSES)) }
    linkm2_LOCK_CLASSES : { KEEP(*(linkm2_LOCK_CLASSES)) }
    scope_local : { KEEP(*(scope_local)) }

    /* Unittest section */
    .unittest : ALIGN(8) {
        __unittest_start = .;
        KEEP(*(.unittest .unittest.*))
        __unittest_end = .;
    }

        /* LLVM profiling sections */
    __llvm_prf_data : ALIGN(8) {
        PROVIDE(__start___llvm_prf_data = .);
        __llvm_prf_data_start = .;
        KEEP(*(__llvm_prf_data))
        PROVIDE(__stop___llvm_prf_data = .);
        __llvm_prf_data_end = .;
    }
    
    __llvm_prf_vnds : ALIGN(8) {
        PROVIDE(__start___llvm_prf_vnds = .);
        __llvm_prf_vnds_start = .;
        KEEP(*(__llvm_prf_vnds))
        PROVIDE(__stop___llvm_prf_vnds = .);
        __llvm_prf_vnds_end = .;
    }

    __llvm_prf_vns : ALIGN(8) {
        PROVIDE(__start___llvm_prf_vns = .);
        KEEP(*(__llvm_prf_vns))
        PROVIDE(__stop___llvm_prf_vns = .);
    }

    __llvm_prf_vtab : ALIGN(8) {
        PROVIDE(__start___llvm_prf_vtab = .);
        KEEP(*(__llvm_prf_vtab))
        PROVIDE(__stop___llvm_prf_vtab = .);
    }
    
    __llvm_prf_names : ALIGN(8) {
        PROVIDE(__start___llvm_prf_names = .);
        __llvm_prf_names_start = .;
        KEEP(*(__llvm_prf_names))
        PROVIDE(__stop___llvm_prf_names = .);
        __llvm_prf_names_end = .;
    }

    __llvm_prf_bits : ALIGN(8) {
        PROVIDE(__start___llvm_prf_bits = .);
        KEEP(*(__llvm_prf_bits))
        PROVIDE(__stop___llvm_prf_bits = .);
    }
    
    __llvm_prf_cnts : ALIGN(8) {
        PROVIDE(__start___llvm_prf_cnts = .);
        __llvm_prf_cnts_start = .;
        KEEP(*(__llvm_prf_cnts))
        PROVIDE(__stop___llvm_prf_cnts = .);
        __llvm_prf_cnts_end = .;
    }

    __llvm_orderfile : ALIGN(8) {
        PROVIDE(__start___llvm_orderfile = .);
        KEEP(*(__llvm_orderfile))
        PROVIDE(__stop___llvm_orderfile = .);
    }

}
INSERT AFTER .tbss;
