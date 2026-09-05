/* A Bramble user program: static, position-dependent, loaded at 4 MiB.
   The kernel's ELF loader handles nothing more exotic than this.

   Sections are page-aligned where it is free to do so, but the kernel does not
   rely on that: lld synthesises .got after section patterns are matched, so no
   script can stop two segments sharing a page. The loader unions the
   protections of every segment covering a page instead, which is what a real
   dynamic loader does. */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : { *(.text .text.*) }

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata : { *(.rodata .rodata.*) }

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data : {
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.plt)
        *(.data .data.*)
    }
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : { *(.eh_frame*) *(.note .note.*) *(.comment) }
}
