OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = 0xffffffff80000000;
    .text : { KEEP(*(.text.start)) *(.text .text.*) } :text
    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata
    . = ALIGN(4096);
    /* Limine writes response pointers and base revision here. */
    .limine_requests : {
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
    } :data
    .data : { *(.data .data.*) *(.got .got.*) } :data
    .bss (NOLOAD) : {
        *(.bss .bss.*) *(COMMON)
    } :data
    ASSERT(. <= 0xfffffffffffff000, "kernel exceeds upper 2 GiB code model")
    /DISCARD/ : { *(.eh_frame*) *(.note .note.*) *(.comment) }
}
