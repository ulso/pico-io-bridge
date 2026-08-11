# Fruit Jam WASM runtime experiment

This branch is the first bounded WebAssembly milestone for Pico I/O Bridge. It
is deliberately separate from the known-good Fruit Jam checkpoint and is not a
production app-upload system.

## What this milestone proves

The opt-in `fruit-jam-wasm-runtime` feature:

- links `wasmi` 1.1 in `no_std` mode;
- embeds a 197-byte module built from Rust for `wasm32v1-none`;
- validates and instantiates exactly one module;
- fixes guest linear memory to one 64 KiB page;
- limits each guest call to 10,000 fuel units;
- exposes only `pico_io_v1.log(ptr, len)` with host-side bounds and UTF-8
  validation;
- calls `app_init()` and `app_tick(0)` and verifies their results; and
- destroys all interpreter state before the core 1 PIO USB-host executor is
  started.

The boot UART and RTT log report either `WASM smoke passed` or the stage that
failed. The runtime has no filesystem, upload, autostart or hardware-access
capabilities in this milestone.

## Build

Build the trace-enabled Fruit Jam experiment with:

```console
cargo build --locked --release --no-default-features \
  --target thumbv8m.main-none-eabihf \
  --features board-adafruit-fruit-jam,fruit-jam-pio-trace,fruit-jam-wasm-runtime
```

The WASM runtime is not enabled by `board-adafruit-fruit-jam` itself. Omitting
`fruit-jam-wasm-runtime` therefore continues to build the checkpoint behavior
without wasmi.

The first linked experiment ELF has these section sizes:

| Section | Known-good pinned rebuild | WASM experiment |
|---|---:|---:|
| `.text` | 423,668 B | 923,460 B |
| `.rodata` | 144,584 B | 211,796 B |
| `.data` | 30,804 B | 30,804 B |
| `.bss` | 205,568 B | 336,652 B |
| `.uninit` | 1,024 B | 1,024 B |

The experiment reserves a 160 KiB heap instead of the normal 32 KiB heap. The
linked image leaves 155,796 bytes between the end of static RAM and the top of
the 512 KiB main RAM region; that remainder still includes the core 0 stack and
is a constraint, not free app memory. PSRAM is intentionally deferred.

The corresponding debug-info ELF is 21,157,868 bytes with SHA-256:

```text
d526506c497685a9b27e3291302d5661a355bff640ef80b0e43b899be2e12509
```

The derived 2,333,696-byte RP2350 ARM Secure UF2 has SHA-256
`7806f5322762ef195dfb03842ffe9e5190430a18efd0e01c6ff099ae7c712b7d`
and was generated with:

```console
picotool uf2 convert pico-io-bridge.elf -t elf pico-io-bridge.uf2 -t uf2 \
  --family rp2350-arm-s --abs-block
```

## Guest ABI v1 smoke contract

The module imports:

```text
pico_io_v1.log(ptr: i32, len: i32) -> ()
```

The host requires these exports (rustc also emits implementation-detail
globals such as `__data_end` and `__heap_base`):

```text
memory
app_init() -> i32
app_tick(now_ms: i64) -> i32
```

The checked-in module is
`assets/wasm/pico_io_v1_guest.wasm`, 197 bytes, SHA-256:

```text
f836f80497a429063a38b827a664ab9b44fad28adcef68072b149d97c04dfab7
```

Its source and reproducible build command are in
`wasm-apps/hello-rust`. Two clean builds produced byte-identical modules. No
`wasm-opt` pass is needed.

## Safety boundary

Only the embedded, reviewed module is accepted. `StoreLimits` restricts
instantiated resources, but it does not make parser and translator allocation
unconditionally OOM-safe for arbitrary hostile modules. Before uploads are
enabled, the system still needs:

- staged and atomic installation;
- an authenticated or physical maintenance mode;
- module size and WebAssembly feature validation before translation;
- a versioned import/export and capability policy;
- explicit USB-host quiescence during flash writes; and
- trap/crash-loop state surfaced through the web UI.

## Next milestone

After hardware smoke and USB regression testing, keep one interpreter instance
on core 0 and drive it cooperatively with resumable fuel. The next host API
should remain read-only: a sensor snapshot plus start, stop, status and log
views. LittleFS upload and PSRAM come later.
