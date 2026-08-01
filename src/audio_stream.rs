//! Bounded PCM snapshot transport from the time-critical USB host task to one web client.

use core::cell::UnsafeCell;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use portable_atomic::{AtomicBool, AtomicU8, Ordering};

pub(crate) const SAMPLE_RATE_HZ: u32 = 48_000;
pub(crate) const SAMPLES_PER_BLOCK: usize = 480;
pub(crate) const PCM_BYTES_PER_BLOCK: usize = SAMPLES_PER_BLOCK * 2;
pub(crate) const FRAME_HEADER_BYTES: usize = 12;
pub(crate) const FRAME_BYTES: usize = FRAME_HEADER_BYTES + PCM_BYTES_PER_BLOCK;

const SLOT_FREE: u8 = 0;
const SLOT_WRITING: u8 = 1;
const SLOT_READY: u8 = 2;
const SLOT_READING: u8 = 3;
const SLOT_COUNT: usize = 3;
const SNAPSHOT_BLOCKS: u8 = 9;
const SNAPSHOT_PERIOD_BLOCKS: u8 = 40;

pub(crate) const FLAG_SNAPSHOT_START: u16 = 1;

#[derive(Clone, Copy)]
pub(crate) struct AudioBlock {
    pub(crate) sequence: u32,
    pub(crate) flags: u16,
    pub(crate) samples: [i16; SAMPLES_PER_BLOCK],
}

struct Slot {
    state: AtomicU8,
    block: UnsafeCell<AudioBlock>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(SLOT_FREE),
            block: UnsafeCell::new(AudioBlock {
                sequence: 0,
                flags: 0,
                samples: [0; SAMPLES_PER_BLOCK],
            }),
        }
    }
}

// A slot is written only after FREE -> WRITING and read only after READY ->
// READING. Release/acquire transitions ensure that its UnsafeCell is never
// accessed concurrently from the two RP2040 cores.
unsafe impl Sync for Slot {}

static SLOTS: [Slot; SLOT_COUNT] = [const { Slot::new() }; SLOT_COUNT];
static BLOCK_READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static CLIENT_CONNECTED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_PHASE: AtomicU8 = AtomicU8::new(0);

/// Publish without backpressure. A full queue drops the incoming block and the
/// sequence gap tells the browser that the current snapshot is incomplete.
pub(crate) fn publish(sequence: u32, samples: &[i16; SAMPLES_PER_BLOCK]) {
    if !CLIENT_CONNECTED.load(Ordering::Relaxed) {
        return;
    }

    // Preserve contiguous 4096-sample FFT windows, but leave most capture
    // blocks on the RP2040. A continuous 96 kB/s PCM stream over CDC-NCM
    // contends with the 1 ms PIO-host schedule; nine blocks every 400 ms keep
    // the browser FFT accurate while reducing average network traffic by 78%.
    let phase = SNAPSHOT_PHASE.fetch_add(1, Ordering::Relaxed);
    let next_phase = if phase + 1 >= SNAPSHOT_PERIOD_BLOCKS {
        0
    } else {
        phase + 1
    };
    SNAPSHOT_PHASE.store(next_phase, Ordering::Relaxed);
    if phase >= SNAPSHOT_BLOCKS {
        return;
    }
    let flags = if phase == 0 { FLAG_SNAPSHOT_START } else { 0 };

    for slot in &SLOTS {
        if slot
            .state
            .compare_exchange(
                SLOT_FREE,
                SLOT_WRITING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            // The WRITING state gives this producer exclusive access without
            // disabling interrupts during the 960-byte copy.
            let block = unsafe { &mut *slot.block.get() };
            block.sequence = sequence;
            block.flags = flags;
            block.samples.copy_from_slice(samples);
            slot.state.store(SLOT_READY, Ordering::Release);
            BLOCK_READY.signal(());
            return;
        }
    }
}

pub(crate) fn reset() {
    SNAPSHOT_PHASE.store(0, Ordering::Relaxed);
    clear_ready();
    BLOCK_READY.reset();
}

pub(crate) async fn next_block() -> AudioBlock {
    loop {
        if let Some(block) = take_oldest() {
            return block;
        }
        BLOCK_READY.wait().await;
    }
}

fn take_oldest() -> Option<AudioBlock> {
    let mut oldest_index: Option<usize> = None;
    let mut oldest_sequence = 0_u32;

    for (index, slot) in SLOTS.iter().enumerate() {
        if slot.state.load(Ordering::Acquire) != SLOT_READY {
            continue;
        }
        let sequence = unsafe { (*slot.block.get()).sequence };
        if oldest_index.is_none() || oldest_sequence.wrapping_sub(sequence) < (u32::MAX / 2) {
            oldest_index = Some(index);
            oldest_sequence = sequence;
        }
    }

    let slot = oldest_index.map(|index| &SLOTS[index])?;
    slot.state
        .compare_exchange(
            SLOT_READY,
            SLOT_READING,
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .ok()?;
    let block = unsafe { *slot.block.get() };
    slot.state.store(SLOT_FREE, Ordering::Release);
    Some(block)
}

fn clear_ready() {
    for slot in &SLOTS {
        let _ =
            slot.state
                .compare_exchange(SLOT_READY, SLOT_FREE, Ordering::AcqRel, Ordering::Relaxed);
    }
}

pub(crate) struct ClientGuard;

impl ClientGuard {
    pub(crate) fn acquire() -> Option<Self> {
        let guard = CLIENT_CONNECTED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| Self);
        if guard.is_some() {
            SNAPSHOT_PHASE.store(0, Ordering::Release);
        }
        guard
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        CLIENT_CONNECTED.store(false, Ordering::Release);
        clear_ready();
        BLOCK_READY.reset();
    }
}
