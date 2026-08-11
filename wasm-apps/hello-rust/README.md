# pico_io_v1 Rust smoke guest

This `no_std` Rust guest imports `pico_io_v1.log` and exports the first two
experimental lifecycle functions:

- `app_init() -> i32`
- `app_tick(now_ms: u64) -> i32`

The linker command fixes linear memory to one 64 KiB WebAssembly page. From
this directory, build the checked-in module with:

```console
env \
  RUSTC_BOOTSTRAP=1 \
  cargo build --locked --release --target wasm32v1-none -Z build-std=core
```

The command inherits the repository's pinned Rust 1.96.0 toolchain, uses its
installed `rust-src` component and does not require a preinstalled
`wasm32v1-none` standard library. `.cargo/config.toml` fixes the guest stack and
linear memory to one 64 KiB page. A clean Cargo cache may download the
sysroot's `dlmalloc` build dependency. Copy the resulting raw module without a
`wasm-opt` step and verify its identity:

```console
cp target/wasm32v1-none/release/pico_io_v1_guest.wasm \
  ../../assets/wasm/pico_io_v1_guest.wasm
shasum -a 256 ../../assets/wasm/pico_io_v1_guest.wasm
```

The expected SHA-256 is
`f836f80497a429063a38b827a664ab9b44fad28adcef68072b149d97c04dfab7`.
