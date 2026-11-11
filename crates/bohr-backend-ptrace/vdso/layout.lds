/* One loadable segment whose addresses equal its file offsets, as the kernel lays its own vDSO out:
 * a reader of a vDSO, glibc's or the Go runtime's, finds its tables at the image's base plus their
 * file offset, since a vDSO is mapped as its file and never relocated. */
SECTIONS
{
    . = SIZEOF_HEADERS;
    .hash           : { *(.hash) }              :text
    .gnu.hash       : { *(.gnu.hash) }
    .dynsym         : { *(.dynsym) }
    .dynstr         : { *(.dynstr) }
    .gnu.version    : { *(.gnu.version) }
    .gnu.version_d  : { *(.gnu.version_d) }
    .gnu.version_r  : { *(.gnu.version_r) }
    .dynamic        : { *(.dynamic) }           :text :dynamic
    .rodata         : { *(.rodata*) }           :text
    .text           : { *(.text*) }             :text
    /DISCARD/       : { *(.data*) *(.bss*) *(.eh_frame*) *(.note*) *(.comment) *(.got*) }
}

PHDRS
{
    text    PT_LOAD    FLAGS(5) FILEHDR PHDRS;
    dynamic PT_DYNAMIC FLAGS(4);
}
