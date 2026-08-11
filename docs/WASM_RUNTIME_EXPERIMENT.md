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

This experimental branch remains Fruit Jam-only. It inherits the checkpoint's
hub integration unchanged; that code still needs a separate `cfg` cleanup and
RP2040 regression pass before anything is considered for `main`. The Wasmi
feature itself is compile-time restricted to `board-adafruit-fruit-jam`.

The hardware-tested layout-preserving ELF has these section sizes:

| Section | Known-good pinned rebuild | WASM layout fix |
|---|---:|---:|
| `.text` | 423,668 B | 923,260 B |
| `.rodata` | 144,584 B | 211,796 B |
| `.data` | 30,804 B | 30,804 B |
| `.bss` | 205,568 B | 205,580 B |
| `.wasm_heap` | 0 B | 163,844 B |
| `.uninit` | 1,024 B | 1,024 B |

The experiment reserves a 160 KiB heap instead of the normal 32 KiB heap. A
32 KiB compatibility cell retains the known-good addresses of the core 1 stack,
executor and PIO USB task state. The real heap is placed in a dedicated
zero-initialized section after ordinary `.bss`. The failing combined image had
moved those objects by 128 KiB, from the striped SRAM0-3 region into the
separate SRAM4-7 region. Restoring the hardware-tested addresses eliminated
the observed enumeration regression; the precise cycle/cache mechanism has
not been isolated further.

The linked image leaves 123,024 bytes between the end of static RAM and the top
of the 512 KiB main RAM region. The async main frame is 14,816 bytes, so this
remainder is adequate for the smoke milestone but remains a stack constraint,
not free app memory. PSRAM is intentionally deferred.

The corresponding debug-info ELF is 21,139,876 bytes with SHA-256:

```text
db54a0c0df1c95138c5f7a83a8e714340462c4abaad4dfd23544c960858d1d56
```

The derived 2,333,184-byte RP2350 ARM Secure UF2 has SHA-256
`4c49fb372361eabec07ae010cebf382a0fca1533173bd85fd9ea01cd3bcfa3c4`
and was generated with:

```console
picotool uf2 convert pico-io-bridge.elf -t elf pico-io-bridge.uf2 -t uf2 \
  --family rp2350-arm-s --abs-block
```

## Hardware validation

The first 160 KiB build used `StaticCell::init([0; HEAP_SIZE])`. That created a
166,400-byte async-main stack frame, exceeded the linked stack gap and cleared
live `.bss`, including Embassy's cached system clock. `init_with` reduced the
frame to 14,816 bytes and the embedded guest then logged `init`, `tick` and
`WASM smoke passed` on Fruit Jam hardware.

A second A/B exposed an independent layout sensitivity. The enlarged ordinary
`.bss` moved the core 1 USB state by exactly `0x20000`; that image reached the
root CH334F hub but never detected the connected BleuIO. Reflashing the exact
17-hour known-good image immediately restored enumeration. Restoring the
known-good SRAM addresses with `.wasm_heap` then produced:

- `BLEUIO_READY`, full speed, address 2 and VID:PID `2DCF:6002`;
- four active HibouAir sensors and zero errors; and
- an increase of 12,154 received bytes and 74 sensor updates during a
  15-second observation.

The test deliberately used BOOTSEL flashing and HTTP status polling after the
initial RTT smoke. No debugger was attached during the final USB validation.

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
