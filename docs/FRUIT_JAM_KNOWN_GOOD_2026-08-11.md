# Adafruit Fruit Jam known-good checkpoint — 2026-08-11

This branch preserves the diagnostic Fruit Jam firmware that completed USB hub
enumeration, configured a BleuIO dongle, and streamed HibouAir sensor data
overnight without a reported error. It is a milestone/checkpoint branch, not a
production-ready multi-board integration branch.

## Source identity

- Application repository base (`main`):
  `f65699243e842a992782eeffec2fcbc2cc6677ea`
- Exact local application source snapshot before replacing the path dependency:
  `b48117d75ab80a520b8eaf2e8da1255f88ec6f78`
- USB-host repository base (`main`):
  `564b1d102c563b195d61cfa6a92647733db25f75`
- Pinned USB-host checkpoint:
  `5dc18494353b3c511b54f299b679aef0951bb3b8`
- USB-host checkpoint branch:
  `codex/fruit-jam-host-known-good-2026-08-11`
- Application checkpoint branch:
  `codex/fruit-jam-known-good-2026-08-11`

The application branch pins the USB-host dependency to the exact host commit;
it does not depend on a sibling checkout.

## Hardware-tested firmware

The firmware that ran the analyzer and overnight soak test was built with the
local source snapshot and the trace-enabled Fruit Jam feature set:

```console
cargo build --locked --release --no-default-features \
  --target thumbv8m.main-none-eabihf \
  --features board-adafruit-fruit-jam,fruit-jam-pio-trace
```

- Target: `thumbv8m.main-none-eabihf` (RP2350 ARM Secure)
- Rust: `rustc 1.96.0 (ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96)`
- LLVM: `22.1.2`
- Profile: release, `opt-level = "s"`, fat LTO, one codegen unit, debug info
- ELF size: 9,819,456 bytes
- ELF SHA-256:
  `49cc01bbe0a296a4d0e0e0aea16530eed3f9952fbf3801f3f7340a6a49d0e3c0`
- Derived UF2 size: 1,199,104 bytes
- Derived UF2 SHA-256:
  `0824dbeb7fe31f628cdd52c63e5966bf3442dfdbabfecdac4114575e781f47a0`

The UF2 was generated from the tested ELF with picotool 2.3.0:

```console
picotool uf2 convert pico-io-bridge.elf -t elf pico-io-bridge.uf2 -t uf2 \
  --family rp2350-arm-s --abs-block
```

The `--abs-block` option carries the RP2350-E10 absolute block and must not be
omitted.

## Reproducible pinned rebuild

After replacing the local path dependency with the immutable host Git revision,
the same trace build completed successfully with Rust 1.97.1. This rebuild is a
compile/reproducibility check; it was not the image used for the overnight
hardware test.

- ELF size: 9,819,120 bytes
- ELF SHA-256:
  `df725034d47264e83c9002b99343392b06a34703860fb373679f7ed1e1aad8a2`
- UF2 size: 1,199,616 bytes
- UF2 SHA-256:
  `61750186c0a78fe3606535fcfaa8a83d63c60ea589aca842585d80ab79ef20b4`
- Pinned `Cargo.lock` SHA-256:
  `dc409decebad7ba0dfc3a81c798f0a5894c0b4515dc86ed7f5b9437e7b821d0a`

## Acceptance evidence

Hardware:

- Adafruit Fruit Jam with its onboard CH334F hub
- Full-speed BleuIO dongle, VID:PID `2DCF:6002`
- Four HibouAir sensors

Browser soak-test snapshot:

- State: `BleuIO Ready`
- Speed: full speed
- RX bytes: 36,143,120
- Errors: 0
- Maximum transfer: 64 bytes
- Four current sensors
- 220,894 decoded sensor reports in the captured screenshot

The decisive Beagle capture ran for 240.386 seconds after the final reset with
no later USB reset. It showed fast full-size transactions, bounded duplicate
recovery, no permanent DATA-toggle lock, and balanced GP6 trace pulses. The raw
CSV identity is:

- Uncompressed size: 78,228,681 bytes
- Uncompressed SHA-256:
  `e608c749d44f8d4631e0aa72d392c36c5e4dba2965b1cb5273192b48914b18d9`
- Gzip size: 14,296,705 bytes
- Gzip SHA-256:
  `5ce084f9eb5981a8705fe8a03f61636bdc5727ac1a4361641b84b09d59244c3c`

## Build-matrix result at checkpoint

USB-host library checks passed for:

- RP2040
- RP2350A
- RP2350B without trace
- RP2350B with `pio1-edge-trace`

Application builds passed for:

- Fruit Jam with `fruit-jam-pio-trace` (the known-good behavior)
- Fruit Jam without trace (compile regression only)
- Cytron Motion 2350 Pro

The application RP2040 build does **not** compile on this checkpoint branch:
the experimental hub block is not fully cfg-isolated from its Fruit Jam/Cytron
imports. This is intentional checkpoint documentation, not a claim that the
branch is RP2040-neutral. RP2040 remains on the unchanged application `main`
commit listed above. Do not merge this branch wholesale into `main`; perform a
separate cleanup and RP2040 regression effort first.

## Known limitations

- The hardware-tested behavior requires `fruit-jam-pio-trace`; a plain Fruit
  Jam build is not equivalent.
- The hub manager supports one active full-speed downstream device.
- The downstream hub path accepts BleuIO only.
- The timing fast paths are diagnostic and trusted-device oriented; malformed
  packet hardening remains future work.
- The checkpoint preserves shared host-engine changes that have not received a
  new RP2040 hardware regression test.

## Local durable archive

The tested ELF, derived UF2, pinned rebuild, screenshot, and compressed Beagle
capture are stored outside Git history under:

`/Users/ulf/Documents/Projects/rust/pico-io-bridge-artifacts/fruit-jam-known-good-2026-08-11`

Binary artifacts should be attached to a GitHub release rather than committed
to this repository's object history.
