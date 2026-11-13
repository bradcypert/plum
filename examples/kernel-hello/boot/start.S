.section .text.start,"ax",@progbits
.global _start
.type _start, @function
_start:
    cli
    cld
    leaq plum_boot_stack_top(%rip), %rsp
    xorq %rbp, %rbp

    /* Preserve paging/protection bits, enable baseline x87 and SSE2. */
    movq %cr0, %rax
    andq $~((1 << 2) | (1 << 3)), %rax /* clear EM and TS */
    orq $(1 << 1), %rax              /* MP */
    movq %rax, %cr0
    movq %cr4, %rax
    orq $((1 << 9) | (1 << 10)), %rax /* OSFXSR, OSXMMEXCPT */
    movq %rax, %cr4
    fninit
    ldmxcsr plum_boot_mxcsr(%rip)

    /* RSP is 16-byte aligned before call: the usual SysV callee entry. */
    call plum_boot_enter
.Lhalt:
    cli
    hlt
    jmp .Lhalt
.size _start, . - _start

.section .rodata,"a",@progbits
.balign 4
plum_boot_mxcsr:
    .long 0x1f80

.section .bss.stack,"aw",@nobits
.balign 16
.global plum_boot_stack_bottom
plum_boot_stack_bottom:
    .skip 65536
.global plum_boot_stack_top
plum_boot_stack_top:

.section .note.GNU-stack,"",@progbits
