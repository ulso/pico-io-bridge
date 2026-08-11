//! Experimental, bounded WebAssembly runtime for Fruit Jam.
//!
//! Milestone one intentionally runs once during boot, before core 1 starts the
//! timing-critical PIO USB host. It has no filesystem or device capabilities.

use core::str;

use defmt::{error, info, warn};
use wasmi::{
    Caller, CompilationMode, Config, EnforcedLimits, Engine, Extern, Linker, Module, Store,
    StoreLimits, StoreLimitsBuilder,
};

const SMOKE_MODULE: &[u8] = include_bytes!("../assets/wasm/pico_io_v1_guest.wasm");
const MAX_MODULE_BYTES: usize = 4 * 1024;
const MAX_LINEAR_MEMORY_BYTES: usize = 64 * 1024;
const CALL_FUEL: u64 = 10_000;

struct HostState {
    limits: StoreLimits,
    log_calls: u8,
    invalid_log_calls: u8,
}

impl HostState {
    fn new() -> Self {
        Self {
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_LINEAR_MEMORY_BYTES)
                .instances(1)
                .memories(1)
                .tables(1)
                .table_elements(128)
                .trap_on_grow_failure(true)
                .build(),
            log_calls: 0,
            invalid_log_calls: 0,
        }
    }
}

fn guest_log(mut caller: Caller<'_, HostState>, ptr: i32, len: i32) {
    const MAX_LOG_BYTES: usize = 96;
    const MAX_LOG_CALLS: u8 = 4;

    let calls = caller
        .data()
        .log_calls
        .saturating_add(caller.data().invalid_log_calls);
    if calls >= MAX_LOG_CALLS {
        let invalid_calls = caller.data().invalid_log_calls.saturating_add(1);
        caller.data_mut().invalid_log_calls = invalid_calls;
        warn!("WASM guest exceeded its host log call budget");
        return;
    }

    let valid = (|| {
        let start = usize::try_from(ptr).ok()?;
        let length = usize::try_from(len).ok()?;
        if length > MAX_LOG_BYTES {
            return None;
        }
        let end = start.checked_add(length)?;
        let memory = caller.get_export("memory").and_then(Extern::into_memory)?;
        let bytes = memory.data(&caller).get(start..end)?;
        let text = str::from_utf8(bytes).ok()?;
        info!("WASM guest: {}", text);
        Some(())
    })()
    .is_some();

    let state = caller.data_mut();
    if valid {
        state.log_calls = state.log_calls.saturating_add(1);
    } else {
        state.invalid_log_calls = state.invalid_log_calls.saturating_add(1);
        warn!("WASM guest supplied an invalid log range");
    }
}

fn smoke_inner() -> Result<(), &'static str> {
    if SMOKE_MODULE.len() > MAX_MODULE_BYTES {
        return Err("module too large");
    }

    let mut config = Config::default();
    config
        .consume_fuel(true)
        .ignore_custom_sections(true)
        .compilation_mode(CompilationMode::Eager)
        .enforced_limits(EnforcedLimits::strict())
        .wasm_mutable_global(true)
        .wasm_sign_extension(false)
        .wasm_saturating_float_to_int(false)
        .wasm_multi_value(false)
        .wasm_multi_memory(false)
        .wasm_bulk_memory(false)
        .wasm_reference_types(false)
        .wasm_tail_call(false)
        .wasm_extended_const(false)
        .wasm_memory64(false)
        .set_min_stack_height(1024)
        .set_max_stack_height(16 * 1024)
        .set_max_recursion_depth(64)
        .set_max_cached_stacks(0);

    let engine = Engine::new(&config);
    let module = Module::new(&engine, SMOKE_MODULE).map_err(|_| "module validation")?;
    let mut store = Store::new(&engine, HostState::new());
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(CALL_FUEL)
        .map_err(|_| "instantiation fuel")?;

    let mut linker = Linker::<HostState>::new(&engine);
    linker
        .func_wrap("pico_io_v1", "log", guest_log)
        .map_err(|_| "host ABI link")?;
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .map_err(|_| "module instantiation")?;
    let init = instance
        .get_typed_func::<(), i32>(&store, "app_init")
        .map_err(|_| "app_init export")?;
    let tick = instance
        .get_typed_func::<i64, i32>(&store, "app_tick")
        .map_err(|_| "app_tick export")?;

    store.set_fuel(CALL_FUEL).map_err(|_| "init fuel")?;
    if init.call(&mut store, ()).map_err(|_| "app_init trap")? != 0 {
        return Err("app_init result");
    }

    store.set_fuel(CALL_FUEL).map_err(|_| "tick fuel")?;
    if tick.call(&mut store, 0).map_err(|_| "app_tick trap")? != 1_000 {
        return Err("app_tick result");
    }

    let state = store.data();
    if state.log_calls != 2 || state.invalid_log_calls != 0 {
        return Err("host ABI result");
    }
    Ok(())
}

pub(crate) fn run_smoke() -> bool {
    info!("WASM smoke starting ({} bytes)", SMOKE_MODULE.len());
    match smoke_inner() {
        Ok(()) => {
            info!("WASM smoke passed");
            true
        }
        Err(stage) => {
            error!("WASM smoke failed at {}", stage);
            false
        }
    }
}
