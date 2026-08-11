MEMORY {
    FLASH : ORIGIN = 0x10000000, LENGTH = 2048K
    RAM   : ORIGIN = 0x20000000, LENGTH = 512K
    SRAM4 : ORIGIN = 0x20080000, LENGTH = 4K
    SRAM5 : ORIGIN = 0x20081000, LENGTH = 4K
}

SECTIONS {
    .start_block : ALIGN(4)
    {
        __start_block_addr = .;
        KEEP(*(.start_block));
        KEEP(*(.boot_info));
    } > FLASH
} INSERT AFTER .vector_table;

_stext = ADDR(.start_block) + SIZEOF(.start_block);

SECTIONS {
    .bi_entries : ALIGN(4)
    {
        __bi_entries_start = .;
        KEEP(*(.bi_entries));
        . = ALIGN(4);
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;

/*
 * Keep the optional WASM allocator backing store out of ordinary .bss.
 *
 * A failing combined build moved the Fruit Jam core-1 stack and PIO USB task
 * state into a different SRAM bank group. Inserting this section after .bss
 * preserves their hardware-tested addresses while keeping the larger WASM
 * allocation contiguous. cortex-m-rt deliberately places __ebss after user
 * sections inserted here, so startup still zeroes the backing store before
 * StaticCell claims it.
 */
SECTIONS {
    .wasm_heap (NOLOAD) : ALIGN(4)
    {
        __swasm_heap = .;
        KEEP(*(.wasm_heap .wasm_heap.*));
        . = ALIGN(4);
        __ewasm_heap = .;
    } > RAM
} INSERT AFTER .bss;

SECTIONS {
    .end_block : ALIGN(4)
    {
        __end_block_addr = .;
        KEEP(*(.end_block));
    } > FLASH
} INSERT AFTER .uninit;

PROVIDE(start_to_end = __end_block_addr - __start_block_addr);
PROVIDE(end_to_start = __start_block_addr - __end_block_addr);
