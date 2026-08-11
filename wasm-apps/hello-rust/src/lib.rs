#![no_std]

#[link(wasm_import_module = "pico_io_v1")]
unsafe extern "C" {
    #[link_name = "log"]
    fn host_log(ptr: *const u8, len: usize);
}

fn log(message: &'static [u8]) {
    // SAFETY: `message` is immutable guest linear-memory data for the duration
    // of the synchronous host call.
    unsafe { host_log(message.as_ptr(), message.len()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn app_init() -> i32 {
    log(b"init");
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn app_tick(_now_ms: u64) -> i32 {
    log(b"tick");
    1_000
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}
