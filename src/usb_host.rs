//! PIO USB root-port manager for the Feather RP2040 USB Host profile.
//!
//! This task is the sole owner of enumeration, device addresses, CDC-ACM
//! pipes, and P8055 HID pipes. SCPI exchanges owned fixed-size messages with
//! it; GP13 remains exclusively owned by the board's existing
//! `StatusIndicator`.

use defmt::{info, warn};
use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_rp::Peri;
use embassy_rp::bind_interrupts;
use embassy_rp::dma::InterruptHandler as DmaInterruptHandler;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIN_16, PIN_17, PIN_18, PIO0, PIO1};
use embassy_rp::pio::InterruptHandler as PioInterruptHandler;
use embassy_rp_pio_usb_host::cdc_acm::{
    CdcAcmError, allocate_from_enumeration as allocate_cdc_from_enumeration,
};
use embassy_rp_pio_usb_host::hid::{
    HidError, allocate_from_enumeration as allocate_hid_from_enumeration,
};
use embassy_rp_pio_usb_host::host::{DeviceEvent, PipeError, Speed, UsbHostController};
use embassy_rp_pio_usb_host::pio_host::PioHostState;
use embassy_rp_pio_usb_host::pio_host::rp2040::Rp2040PioEngine;
use embassy_rp_pio_usb_host::usb::CdcLineCoding;
use embassy_rp_pio_usb_host::{AttachDetector, BusEvent, DeviceSpeed};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Ticker, Timer, with_timeout};
use embassy_usb_host::{BusController, BusRoute, BusState};

use crate::p8055;

const ATTACH_DEBOUNCE_SAMPLES: u16 = 100;
const CONFIG_DESCRIPTOR_CAPACITY: usize = 512;
const REPORT_DESCRIPTOR_CAPACITY: usize = 256;
const COMMAND_CAPACITY: usize = 4;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(4);
const EXCHANGE_COMMAND_TIMEOUT: Duration = Duration::from_secs(18);
const CLASS_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2);
const EXCHANGE_FIRST_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_IDLE_TIMEOUT: Duration = Duration::from_millis(50);
pub(crate) const CDC_MAX_TRANSFER: usize = crate::USB_HOST_CDC_MAX_TRANSFER;

bind_interrupts!(struct PioUsbHostIrqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    PIO1_IRQ_0 => PioInterruptHandler<PIO1>;
    DMA_IRQ_0 => DmaInterruptHandler<DMA_CH0>;
});

pub(crate) struct Hardware {
    pio0: Peri<'static, PIO0>,
    pio1: Peri<'static, PIO1>,
    dma_ch0: Peri<'static, DMA_CH0>,
    dp: Peri<'static, PIN_16>,
    dm: Peri<'static, PIN_17>,
    vbus_enable: Peri<'static, PIN_18>,
}

impl Hardware {
    pub(crate) fn new(
        pio0: Peri<'static, PIO0>,
        pio1: Peri<'static, PIO1>,
        dma_ch0: Peri<'static, DMA_CH0>,
        dp: Peri<'static, PIN_16>,
        dm: Peri<'static, PIN_17>,
        vbus_enable: Peri<'static, PIN_18>,
    ) -> Self {
        Self {
            pio0,
            pio1,
            dma_ch0,
            dp,
            dm,
            vbus_enable,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Phase {
    PowerOff,
    Waiting,
    Resetting,
    Enumerating,
    CdcReady,
    P8055Ready,
    UnsupportedSpeed,
    UnsupportedDevice,
    EnumerationError,
    CdcError,
    P8055Error,
}

impl Phase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PowerOff => "POWER_OFF",
            Self::Waiting => "WAITING",
            Self::Resetting => "RESETTING",
            Self::Enumerating => "ENUMERATING",
            Self::CdcReady => "CDC_READY",
            Self::P8055Ready => "P8055_READY",
            Self::UnsupportedSpeed => "UNSUPPORTED_SPEED",
            Self::UnsupportedDevice => "UNSUPPORTED_DEVICE",
            Self::EnumerationError => "ENUMERATION_ERROR",
            Self::CdcError => "CDC_ERROR",
            Self::P8055Error => "P8055_ERROR",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum HostSpeed {
    Low,
    Full,
}

impl HostSpeed {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::Full => "FULL",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Status {
    pub(crate) phase: Phase,
    pub(crate) speed: Option<HostSpeed>,
    pub(crate) address: u8,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) rx_bytes: u32,
    pub(crate) tx_bytes: u32,
    pub(crate) error_count: u32,
    generation: u32,
}

impl Status {
    const fn power_off() -> Self {
        Self {
            phase: Phase::PowerOff,
            speed: None,
            address: 0,
            vendor_id: 0,
            product_id: 0,
            rx_bytes: 0,
            tx_bytes: 0,
            error_count: 0,
            generation: 0,
        }
    }

    fn clear_device(&mut self, phase: Phase) {
        if self.phase != phase {
            self.generation = self.generation.wrapping_add(1);
        }
        self.phase = phase;
        self.speed = None;
        self.address = 0;
        self.vendor_id = 0;
        self.product_id = 0;
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CdcData {
    len: u8,
    bytes: [u8; CDC_MAX_TRANSFER],
}

impl CdcData {
    const fn empty() -> Self {
        Self {
            len: 0,
            bytes: [0; CDC_MAX_TRANSFER],
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Error {
    InvalidLength,
    InvalidHex,
    InvalidParameter,
    DataStale,
    NotReady,
    Timeout,
    Transfer,
    Protocol,
}

#[derive(Clone, Copy)]
enum Operation {
    Read { length: u8 },
    Write(CdcData),
    Exchange { write: CdcData, read_length: u8 },
    P8055ReadInput,
    P8055GetOutput,
    P8055SetOutput(p8055::OutputState),
    P8055ResetCounter { channel: u8 },
    P8055SetDebounce { channel: u8, microseconds: u32 },
    P8055GetDebounce { channel: u8 },
}

impl Operation {
    const fn ready_phase(self) -> Phase {
        match self {
            Self::Read { .. } | Self::Write(_) | Self::Exchange { .. } => Phase::CdcReady,
            Self::P8055ReadInput
            | Self::P8055GetOutput
            | Self::P8055SetOutput(_)
            | Self::P8055ResetCounter { .. }
            | Self::P8055SetDebounce { .. }
            | Self::P8055GetDebounce { .. } => Phase::P8055Ready,
        }
    }

    const fn reply_timeout(self) -> Duration {
        match self {
            Self::Exchange { .. } => EXCHANGE_COMMAND_TIMEOUT,
            Self::Read { .. }
            | Self::Write(_)
            | Self::P8055ReadInput
            | Self::P8055GetOutput
            | Self::P8055SetOutput(_)
            | Self::P8055ResetCounter { .. }
            | Self::P8055SetDebounce { .. }
            | Self::P8055GetDebounce { .. } => COMMAND_TIMEOUT,
        }
    }
}

#[derive(Clone, Copy)]
struct Command {
    sequence: u32,
    generation: u32,
    operation: Operation,
}

#[derive(Clone, Copy)]
enum ReplyResult {
    Read(Result<CdcData, Error>),
    Write(Result<u8, Error>),
    Exchange(Result<CdcData, Error>),
    P8055Input(Result<p8055::InputReport, Error>),
    P8055Output(Result<p8055::OutputState, Error>),
    P8055Unit(Result<(), Error>),
    P8055Debounce(Result<u32, Error>),
}

#[derive(Clone, Copy)]
struct Reply {
    sequence: u32,
    result: ReplyResult,
}

static HOST_STATE: Mutex<CriticalSectionRawMutex, Status> = Mutex::new(Status::power_off());
static HOST_COMMANDS: Channel<CriticalSectionRawMutex, Command, COMMAND_CAPACITY> = Channel::new();
static HOST_REPLIES: Channel<CriticalSectionRawMutex, Reply, COMMAND_CAPACITY> = Channel::new();
static HOST_COMMAND_LOCK: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());
static HOST_COMMAND_SEQUENCE: Mutex<CriticalSectionRawMutex, u32> = Mutex::new(0);

pub(crate) async fn status() -> Status {
    *HOST_STATE.lock().await
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_hex(value: &str) -> Result<CdcData, Error> {
    let value = value.as_bytes();
    if value.is_empty() || value.len() / 2 > CDC_MAX_TRANSFER {
        return Err(Error::InvalidLength);
    }
    if !value.len().is_multiple_of(2) {
        return Err(Error::InvalidHex);
    }

    let byte_count = value.len() / 2;
    let mut data = CdcData::empty();
    for (index, pair) in value.chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or(Error::InvalidHex)?;
        let low = hex_nibble(pair[1]).ok_or(Error::InvalidHex)?;
        data.bytes[index] = (high << 4) | low;
    }
    data.len = byte_count as u8;
    Ok(data)
}

async fn send_command(operation: Operation) -> Result<ReplyResult, Error> {
    let reply_timeout = operation.reply_timeout();

    let _guard = HOST_COMMAND_LOCK.lock().await;
    let state = status().await;
    if state.phase != operation.ready_phase() {
        return Err(Error::NotReady);
    }
    let sequence = {
        let mut next = HOST_COMMAND_SEQUENCE.lock().await;
        *next = next.wrapping_add(1);
        *next
    };
    while HOST_REPLIES.try_receive().is_ok() {}
    HOST_COMMANDS
        .send(Command {
            sequence,
            generation: state.generation,
            operation,
        })
        .await;

    let matching_reply = async {
        loop {
            let reply = HOST_REPLIES.receive().await;
            if reply.sequence == sequence {
                break reply.result;
            }
        }
    };

    match select(matching_reply, Timer::after(reply_timeout)).await {
        Either::First(reply) => Ok(reply),
        Either::Second(()) => Err(Error::Timeout),
    }
}

pub(crate) async fn cdc_write_hex(value: &str) -> Result<u8, Error> {
    let data = parse_hex(value)?;
    match send_command(Operation::Write(data)).await? {
        ReplyResult::Write(result) => result,
        _ => Err(Error::Transfer),
    }
}

pub(crate) async fn cdc_read(length: u8) -> Result<CdcData, Error> {
    if length == 0 || usize::from(length) > CDC_MAX_TRANSFER {
        return Err(Error::InvalidLength);
    }

    match send_command(Operation::Read { length }).await? {
        ReplyResult::Read(result) => result,
        _ => Err(Error::Transfer),
    }
}

pub(crate) async fn cdc_exchange_hex(value: &str, read_length: u8) -> Result<CdcData, Error> {
    if read_length == 0 || usize::from(read_length) > CDC_MAX_TRANSFER {
        return Err(Error::InvalidLength);
    }

    let write = parse_hex(value)?;
    match send_command(Operation::Exchange { write, read_length }).await? {
        ReplyResult::Exchange(result) => result,
        _ => Err(Error::Transfer),
    }
}

pub(crate) async fn p8055_read_input() -> Result<p8055::InputReport, Error> {
    match send_command(Operation::P8055ReadInput).await? {
        ReplyResult::P8055Input(result) => result,
        _ => Err(Error::Transfer),
    }
}

pub(crate) async fn p8055_get_output() -> Result<p8055::OutputState, Error> {
    match send_command(Operation::P8055GetOutput).await? {
        ReplyResult::P8055Output(result) => result,
        _ => Err(Error::Transfer),
    }
}

pub(crate) async fn p8055_set_output(
    digital_outputs: u8,
    analog_output_1: u8,
    analog_output_2: u8,
) -> Result<(), Error> {
    let output = p8055::OutputState {
        digital_outputs,
        analog_output_1,
        analog_output_2,
    };
    match send_command(Operation::P8055SetOutput(output)).await? {
        ReplyResult::P8055Unit(result) => result,
        _ => Err(Error::Transfer),
    }
}

pub(crate) async fn p8055_reset_counter(channel: u8) -> Result<(), Error> {
    if !matches!(channel, 1 | 2) {
        return Err(Error::InvalidParameter);
    }
    match send_command(Operation::P8055ResetCounter { channel }).await? {
        ReplyResult::P8055Unit(result) => result,
        _ => Err(Error::Transfer),
    }
}

pub(crate) async fn p8055_set_debounce(channel: u8, microseconds: u32) -> Result<(), Error> {
    if !matches!(channel, 1 | 2) || microseconds > p8055::MAX_DEBOUNCE_MICROS {
        return Err(Error::InvalidParameter);
    }
    match send_command(Operation::P8055SetDebounce {
        channel,
        microseconds,
    })
    .await?
    {
        ReplyResult::P8055Unit(result) => result,
        _ => Err(Error::Transfer),
    }
}

pub(crate) async fn p8055_get_debounce(channel: u8) -> Result<u32, Error> {
    if !matches!(channel, 1 | 2) {
        return Err(Error::InvalidParameter);
    }
    match send_command(Operation::P8055GetDebounce { channel }).await? {
        ReplyResult::P8055Debounce(result) => result,
        _ => Err(Error::Transfer),
    }
}

async fn set_waiting() {
    HOST_STATE.lock().await.clear_device(Phase::Waiting);
}

async fn set_waiting_if_current(generation: u32) {
    let mut state = HOST_STATE.lock().await;
    if state.generation == generation {
        state.clear_device(Phase::Waiting);
    }
}

async fn set_resetting(speed: HostSpeed) {
    let mut state = HOST_STATE.lock().await;
    state.generation = state.generation.wrapping_add(1);
    state.phase = Phase::Resetting;
    state.speed = Some(speed);
    state.address = 0;
    state.vendor_id = 0;
    state.product_id = 0;
}

async fn begin_enumeration(speed: HostSpeed) -> Option<u32> {
    let mut state = HOST_STATE.lock().await;
    if state.phase != Phase::Resetting || state.speed != Some(speed) {
        return None;
    }
    state.phase = Phase::Enumerating;
    Some(state.generation)
}

async fn set_identity_if_current(
    generation: u32,
    address: u8,
    vendor_id: u16,
    product_id: u16,
) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != Phase::Enumerating {
        return false;
    }
    state.address = address;
    state.vendor_id = vendor_id;
    state.product_id = product_id;
    true
}

async fn set_phase_if_current(generation: u32, expected: Phase, phase: Phase) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != expected {
        return false;
    }
    state.phase = phase;
    true
}

async fn set_error_phase(phase: Phase) {
    let mut state = HOST_STATE.lock().await;
    state.phase = phase;
    state.error_count = state.error_count.wrapping_add(1);
}

async fn set_error_phase_if_current(generation: u32, expected: Phase, phase: Phase) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != expected {
        return false;
    }
    state.phase = phase;
    state.error_count = state.error_count.wrapping_add(1);
    true
}

async fn record_error_if_current(generation: u32, expected: Phase) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation == generation && state.phase == expected {
        state.error_count = state.error_count.wrapping_add(1);
        true
    } else {
        false
    }
}

async fn fail_current_session(
    generation: u32,
    expected: Phase,
    phase: Phase,
    error: Error,
) -> Error {
    if set_error_phase_if_current(generation, expected, phase).await {
        error
    } else {
        Error::NotReady
    }
}

async fn record_current_session_error(generation: u32, expected: Phase, error: Error) -> Error {
    if record_error_if_current(generation, expected).await {
        error
    } else {
        Error::NotReady
    }
}

async fn record_rx_if_current(generation: u32, expected: Phase, count: usize) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != expected {
        return false;
    }
    state.rx_bytes = state.rx_bytes.wrapping_add(count as u32);
    true
}

async fn record_tx_if_current(generation: u32, expected: Phase, count: usize) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != expected {
        return false;
    }
    state.tx_bytes = state.tx_bytes.wrapping_add(count as u32);
    true
}

fn send_reply(reply: Reply) {
    if HOST_REPLIES.try_send(reply).is_err() {
        warn!("PIO USB host reply queue full");
    }
}

fn reject_command(command: Command, error: Error) {
    let result = match command.operation {
        Operation::Read { .. } => ReplyResult::Read(Err(error)),
        Operation::Write(_) => ReplyResult::Write(Err(error)),
        Operation::Exchange { .. } => ReplyResult::Exchange(Err(error)),
        Operation::P8055ReadInput => ReplyResult::P8055Input(Err(error)),
        Operation::P8055GetOutput => ReplyResult::P8055Output(Err(error)),
        Operation::P8055SetOutput(_)
        | Operation::P8055ResetCounter { .. }
        | Operation::P8055SetDebounce { .. } => ReplyResult::P8055Unit(Err(error)),
        Operation::P8055GetDebounce { .. } => ReplyResult::P8055Debounce(Err(error)),
    };
    send_reply(Reply {
        sequence: command.sequence,
        result,
    });
}

async fn command_is_current(command: &Command) -> bool {
    session_is_current(command.generation, command.operation.ready_phase()).await
}

async fn session_is_current(generation: u32, expected: Phase) -> bool {
    let state = status().await;
    state.generation == generation && state.phase == expected
}

async fn wait_for_removal<'d, C>(controller: &mut BusController<'d, C>)
where
    C: UsbHostController<'d>,
{
    loop {
        match select(controller.wait_for_device_event(), HOST_COMMANDS.receive()).await {
            Either::First(DeviceEvent::Disconnected | DeviceEvent::Overcurrent) => break,
            Either::First(_) => {}
            Either::Second(command) => reject_command(command, Error::NotReady),
        }
    }
}

async fn root_port_monitor<'d>(host_state: &PioHostState<Rp2040PioEngine<'d>>) {
    let mut detector = AttachDetector::new(ATTACH_DEBOUNCE_SAMPLES);
    let mut ticker = Ticker::every(Duration::from_millis(1));
    let mut connected = false;

    loop {
        ticker.next().await;

        let Some(line_state) = host_state.root_line_state_if_not_resetting() else {
            continue;
        };

        if let Some(event) = detector.update(line_state) {
            match event {
                BusEvent::Attached(device_speed) => {
                    let (host_speed, speed) = match device_speed {
                        DeviceSpeed::Low => (HostSpeed::Low, Speed::Low),
                        DeviceSpeed::Full => (HostSpeed::Full, Speed::Full),
                    };
                    set_resetting(host_speed).await;
                    info!("PIO USB root device attached");
                    match host_state.reset_and_report_connected(speed).await {
                        Ok(()) => {
                            connected = true;
                            ticker = Ticker::every(Duration::from_millis(1));
                        }
                        Err(_) => {
                            connected = false;
                            set_error_phase(Phase::EnumerationError).await;
                            warn!("PIO USB root reset failed");
                        }
                    }
                }
                BusEvent::Detached => {
                    if connected && host_state.report_disconnected_if_not_resetting() {
                        connected = false;
                    }
                    set_waiting().await;
                    info!("PIO USB root device detached");
                }
                BusEvent::Invalid => {
                    if connected && host_state.report_disconnected_if_not_resetting() {
                        connected = false;
                    }
                    set_error_phase(Phase::EnumerationError).await;
                    warn!("invalid SE1 state on PIO USB root port");
                }
            }
        }

        if connected {
            let _ = host_state.service_frame().await;
        }
    }
}

async fn run(hardware: Hardware) {
    let mut vbus_enable = Output::new(hardware.vbus_enable, Level::Low);
    let engine = Rp2040PioEngine::new(
        hardware.pio0,
        hardware.pio1,
        hardware.dma_ch0,
        hardware.dp,
        hardware.dm,
        PioUsbHostIrqs,
        PioUsbHostIrqs,
        PioUsbHostIrqs,
    );
    let host_state = PioHostState::new(engine);
    let bus_state = BusState::new();
    let controller = host_state
        .controller()
        .expect("one PIO USB root controller");
    let (mut controller, bus_handle) = embassy_usb_host::bus(controller, &bus_state);

    Timer::after_millis(100).await;
    vbus_enable.set_high();
    set_waiting().await;
    info!("PIO USB host VBUS enabled");

    let application = async move {
        let _vbus_enable = vbus_enable;

        loop {
            let speed = loop {
                match select(controller.wait_for_device_event(), HOST_COMMANDS.receive()).await {
                    Either::First(DeviceEvent::Connected(speed)) => break speed,
                    Either::First(_) => {}
                    Either::Second(command) => reject_command(command, Error::NotReady),
                }
            };

            let host_speed = match speed {
                Speed::Low => HostSpeed::Low,
                Speed::Full => HostSpeed::Full,
                Speed::High => {
                    set_error_phase(Phase::UnsupportedSpeed).await;
                    warn!("high-speed PIO USB root device is unsupported");
                    loop {
                        match select(controller.wait_for_device_event(), HOST_COMMANDS.receive())
                            .await
                        {
                            Either::First(DeviceEvent::Disconnected | DeviceEvent::Overcurrent) => {
                                break;
                            }
                            Either::First(_) => {}
                            Either::Second(command) => reject_command(command, Error::NotReady),
                        }
                    }
                    set_waiting().await;
                    continue;
                }
            };
            let Some(session_generation) = begin_enumeration(host_speed).await else {
                continue;
            };
            let mut configuration = [0_u8; CONFIG_DESCRIPTOR_CAPACITY];
            let (enumeration, configuration_len) = match bus_handle
                .enumerate(BusRoute::Direct(speed), &mut configuration)
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    if set_error_phase_if_current(
                        session_generation,
                        Phase::Enumerating,
                        Phase::EnumerationError,
                    )
                    .await
                    {
                        warn!("PIO USB device enumeration failed");
                        wait_for_removal(&mut controller).await;
                    }

                    // embassy-usb-host 0.1.0 can retain an address lease on an
                    // enumeration error. This root-only backend has no hubs,
                    // so releasing the complete address space after physical
                    // detach is safe.
                    for address in 1_u8..=127 {
                        bus_handle.free_address(address);
                    }
                    set_waiting_if_current(session_generation).await;
                    continue;
                }
            };

            let address = enumeration.device_address;
            if !set_identity_if_current(
                session_generation,
                address,
                enumeration.device_desc.vendor_id,
                enumeration.device_desc.product_id,
            )
            .await
            {
                bus_handle.free_address(address);
                continue;
            }

            if speed == Speed::Low {
                if !p8055::is_original(
                    enumeration.device_desc.vendor_id,
                    enumeration.device_desc.product_id,
                ) {
                    if set_phase_if_current(
                        session_generation,
                        Phase::Enumerating,
                        Phase::UnsupportedDevice,
                    )
                    .await
                    {
                        warn!("low-speed USB device is not a supported P8055");
                        wait_for_removal(&mut controller).await;
                    }
                } else {
                    match allocate_hid_from_enumeration(
                        &bus_handle,
                        &configuration[..configuration_len],
                        &enumeration,
                    ) {
                        Err(_) => {
                            if set_error_phase_if_current(
                                session_generation,
                                Phase::Enumerating,
                                Phase::P8055Error,
                            )
                            .await
                            {
                                warn!("P8055 HID pipe allocation failed");
                                wait_for_removal(&mut controller).await;
                            }
                        }
                        Ok(mut hid) => {
                            hid.reset_data_toggles();
                            let mut report_descriptor = [0_u8; REPORT_DESCRIPTOR_CAPACITY];
                            let descriptor_ready = matches!(
                                with_timeout(
                                    CLASS_CONTROL_TIMEOUT,
                                    hid.get_report_descriptor(&mut report_descriptor)
                                )
                                .await,
                                Ok(Ok(_))
                            );

                            let reset_ready = if descriptor_ready {
                                match with_timeout(
                                    TRANSFER_TIMEOUT,
                                    hid.write_output_report(&p8055::OutputState::reset_report()),
                                )
                                .await
                                {
                                    Ok(Ok(())) => {
                                        let mut state = HOST_STATE.lock().await;
                                        if state.generation == session_generation
                                            && state.phase == Phase::Enumerating
                                        {
                                            state.tx_bytes = state
                                                .tx_bytes
                                                .wrapping_add(p8055::REPORT_LEN as u32);
                                            true
                                        } else {
                                            false
                                        }
                                    }
                                    _ => false,
                                }
                            } else {
                                false
                            };

                            let initial_input = if reset_ready {
                                let mut raw = [0_u8; p8055::REPORT_LEN];
                                match with_timeout(
                                    TRANSFER_TIMEOUT,
                                    hid.read_input_report(&mut raw),
                                )
                                .await
                                {
                                    Ok(Ok(count)) if count == p8055::REPORT_LEN => {
                                        p8055::InputReport::parse(&raw)
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };

                            if initial_input.is_none() {
                                if set_error_phase_if_current(
                                    session_generation,
                                    Phase::Enumerating,
                                    Phase::P8055Error,
                                )
                                .await
                                {
                                    warn!("P8055 HID initialization failed");
                                    wait_for_removal(&mut controller).await;
                                }
                            } else {
                                let mut output = p8055::OutputState::all_off();
                                let mut debounce = [None; 2];
                                let ready = {
                                    let mut state = HOST_STATE.lock().await;
                                    if state.generation == session_generation
                                        && state.phase == Phase::Enumerating
                                    {
                                        state.rx_bytes =
                                            state.rx_bytes.wrapping_add(p8055::REPORT_LEN as u32);
                                        state.phase = Phase::P8055Ready;
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if ready {
                                    info!(
                                        "PIO USB P8055 ready at address {}, PID={=u16:04x}",
                                        address, enumeration.device_desc.product_id
                                    );

                                    loop {
                                        match select(
                                            controller.wait_for_device_event(),
                                            HOST_COMMANDS.receive(),
                                        )
                                        .await
                                        {
                                            Either::First(
                                                DeviceEvent::Disconnected
                                                | DeviceEvent::Overcurrent,
                                            ) => break,
                                            Either::First(_) => {}
                                            Either::Second(command) => {
                                                if !command_is_current(&command).await {
                                                    reject_command(command, Error::NotReady);
                                                    continue;
                                                }
                                                let Command {
                                                    sequence,
                                                    operation,
                                                    ..
                                                } = command;
                                                match operation {
                                                    Operation::P8055ReadInput => {
                                                        let mut raw = [0_u8; p8055::REPORT_LEN];
                                                        let result = match with_timeout(
                                                            TRANSFER_TIMEOUT,
                                                            hid.read_input_report(&mut raw),
                                                        )
                                                        .await
                                                        {
                                                            Ok(Ok(count))
                                                                if count == p8055::REPORT_LEN =>
                                                            {
                                                                match p8055::InputReport::parse(
                                                                    &raw,
                                                                ) {
                                                                    Some(input) => {
                                                                        let mut state =
                                                                            HOST_STATE.lock().await;
                                                                        if state.generation
                                                                            == session_generation
                                                                            && state.phase
                                                                                == Phase::P8055Ready
                                                                        {
                                                                            state.rx_bytes = state
                                                                            .rx_bytes
                                                                            .wrapping_add(
                                                                                p8055::REPORT_LEN
                                                                                    as u32,
                                                                            );
                                                                            Ok(input)
                                                                        } else {
                                                                            Err(Error::NotReady)
                                                                        }
                                                                    }
                                                                    None => {
                                                                        let current =
                                                                        set_error_phase_if_current(
                                                                            session_generation,
                                                                            Phase::P8055Ready,
                                                                        Phase::P8055Error,
                                                                    )
                                                                    .await;
                                                                        Err(if current {
                                                                            Error::Protocol
                                                                        } else {
                                                                            Error::NotReady
                                                                        })
                                                                    }
                                                                }
                                                            }
                                                            Ok(Err(HidError::Transfer(
                                                                PipeError::Disconnected,
                                                            ))) => Err(Error::NotReady),
                                                            Ok(Ok(_)) => {
                                                                let current =
                                                                    set_error_phase_if_current(
                                                                        session_generation,
                                                                        Phase::P8055Ready,
                                                                        Phase::P8055Error,
                                                                    )
                                                                    .await;
                                                                Err(if current {
                                                                    Error::Protocol
                                                                } else {
                                                                    Error::NotReady
                                                                })
                                                            }
                                                            Ok(Err(_)) => {
                                                                let current =
                                                                    set_error_phase_if_current(
                                                                        session_generation,
                                                                        Phase::P8055Ready,
                                                                        Phase::P8055Error,
                                                                    )
                                                                    .await;
                                                                Err(if current {
                                                                    Error::Transfer
                                                                } else {
                                                                    Error::NotReady
                                                                })
                                                            }
                                                            Err(_) => {
                                                                let current =
                                                                    record_error_if_current(
                                                                        session_generation,
                                                                        Phase::P8055Ready,
                                                                    )
                                                                    .await;
                                                                Err(if current {
                                                                    Error::Timeout
                                                                } else {
                                                                    Error::NotReady
                                                                })
                                                            }
                                                        };
                                                        send_reply(Reply {
                                                            sequence,
                                                            result: ReplyResult::P8055Input(result),
                                                        });
                                                    }
                                                    Operation::P8055GetOutput => {
                                                        send_reply(Reply {
                                                            sequence,
                                                            result: ReplyResult::P8055Output(Ok(
                                                                output,
                                                            )),
                                                        });
                                                    }
                                                    Operation::P8055GetDebounce { channel } => {
                                                        let result = debounce[usize::from(
                                                            channel.saturating_sub(1),
                                                        )]
                                                        .ok_or(Error::DataStale);
                                                        send_reply(Reply {
                                                            sequence,
                                                            result: ReplyResult::P8055Debounce(
                                                                result,
                                                            ),
                                                        });
                                                    }
                                                    operation @ (Operation::P8055SetOutput(_)
                                                    | Operation::P8055ResetCounter {
                                                        ..
                                                    }
                                                    | Operation::P8055SetDebounce {
                                                        ..
                                                    }) => {
                                                        let (
                                                        report,
                                                        next_output,
                                                        next_debounce,
                                                    ) = match operation {
                                                        Operation::P8055SetOutput(next) => {
                                                            (
                                                                next.apply_report(),
                                                                Some(next),
                                                                None,
                                                            )
                                                        }
                                                        Operation::P8055ResetCounter { channel } => {
                                                            (
                                                                output
                                                                    .reset_counter_report(channel),
                                                                None,
                                                                None,
                                                            )
                                                        }
                                                        Operation::P8055SetDebounce {
                                                            channel,
                                                            microseconds,
                                                        } => (
                                                            output.set_debounce_report(
                                                                channel,
                                                                microseconds,
                                                            ),
                                                            None,
                                                            Some((
                                                                channel,
                                                                p8055::quantized_debounce_micros(
                                                                    microseconds,
                                                                ),
                                                            )),
                                                        ),
                                                        _ => unreachable!(),
                                                    };
                                                        let result = match with_timeout(
                                                            TRANSFER_TIMEOUT,
                                                            hid.write_output_report(&report),
                                                        )
                                                        .await
                                                        {
                                                            Ok(Ok(())) => {
                                                                let mut state =
                                                                    HOST_STATE.lock().await;
                                                                if state.generation
                                                                    == session_generation
                                                                    && state.phase
                                                                        == Phase::P8055Ready
                                                                {
                                                                    state.tx_bytes = state
                                                                        .tx_bytes
                                                                        .wrapping_add(
                                                                            p8055::REPORT_LEN
                                                                                as u32,
                                                                        );
                                                                    drop(state);
                                                                    if let Some(next) = next_output
                                                                    {
                                                                        output = next;
                                                                    }
                                                                    if let Some((channel, actual)) =
                                                                        next_debounce
                                                                    {
                                                                        debounce[usize::from(
                                                                            channel - 1,
                                                                        )] = Some(actual);
                                                                    }
                                                                    Ok(())
                                                                } else {
                                                                    Err(Error::NotReady)
                                                                }
                                                            }
                                                            Ok(Err(HidError::Transfer(
                                                                PipeError::Disconnected,
                                                            ))) => Err(Error::NotReady),
                                                            Ok(Err(_)) => {
                                                                let current =
                                                                    set_error_phase_if_current(
                                                                        session_generation,
                                                                        Phase::P8055Ready,
                                                                        Phase::P8055Error,
                                                                    )
                                                                    .await;
                                                                Err(if current {
                                                                    Error::Transfer
                                                                } else {
                                                                    Error::NotReady
                                                                })
                                                            }
                                                            Err(_) => {
                                                                let current =
                                                                    set_error_phase_if_current(
                                                                        session_generation,
                                                                        Phase::P8055Ready,
                                                                        Phase::P8055Error,
                                                                    )
                                                                    .await;
                                                                Err(if current {
                                                                    Error::Timeout
                                                                } else {
                                                                    Error::NotReady
                                                                })
                                                            }
                                                        };
                                                        send_reply(Reply {
                                                            sequence,
                                                            result: ReplyResult::P8055Unit(result),
                                                        });
                                                    }
                                                    operation => reject_command(
                                                        Command {
                                                            sequence,
                                                            generation: command.generation,
                                                            operation,
                                                        },
                                                        Error::NotReady,
                                                    ),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                bus_handle.free_address(address);
                set_waiting_if_current(session_generation).await;
                info!("PIO USB root address {} released", address);
                continue;
            }

            match allocate_cdc_from_enumeration(
                &bus_handle,
                &configuration[..configuration_len],
                &enumeration,
            ) {
                Ok(mut cdc) => {
                    let controls_ready = if cdc.function().supports_line_requests() {
                        match with_timeout(CLASS_CONTROL_TIMEOUT, async {
                            // Mirror the sequence verified by the pinned
                            // backend's BleuIO host example: open both modem
                            // control lines before selecting 115200 8-N-1.
                            cdc.set_control_line_state(true, true).await?;
                            cdc.set_line_coding(CdcLineCoding::eight_n_one(115_200))
                                .await?;
                            Ok::<(), CdcAcmError>(())
                        })
                        .await
                        {
                            Ok(Ok(())) => true,
                            Ok(Err(_)) | Err(_) => false,
                        }
                    } else {
                        true
                    };

                    if controls_ready {
                        cdc.reset_data_toggles();
                        if set_phase_if_current(
                            session_generation,
                            Phase::Enumerating,
                            Phase::CdcReady,
                        )
                        .await
                        {
                            info!(
                                "PIO USB CDC-ACM ready at address {}, VID={=u16:04x} PID={=u16:04x}",
                                address,
                                enumeration.device_desc.vendor_id,
                                enumeration.device_desc.product_id
                            );

                            loop {
                                match select(
                                    controller.wait_for_device_event(),
                                    HOST_COMMANDS.receive(),
                                )
                                .await
                                {
                                    Either::First(
                                        DeviceEvent::Disconnected | DeviceEvent::Overcurrent,
                                    ) => break,
                                    Either::First(_) => {}
                                    Either::Second(
                                        command @ Command {
                                            sequence,
                                            operation: Operation::Read { length },
                                            ..
                                        },
                                    ) => {
                                        if !command_is_current(&command).await {
                                            reject_command(command, Error::NotReady);
                                            continue;
                                        }
                                        let mut data = CdcData::empty();
                                        let result = match with_timeout(
                                            TRANSFER_TIMEOUT,
                                            cdc.read(&mut data.bytes[..usize::from(length)]),
                                        )
                                        .await
                                        {
                                            Ok(Ok(count)) => {
                                                data.len = count as u8;
                                                if record_rx_if_current(
                                                    session_generation,
                                                    Phase::CdcReady,
                                                    count,
                                                )
                                                .await
                                                {
                                                    Ok(data)
                                                } else {
                                                    Err(Error::NotReady)
                                                }
                                            }
                                            Ok(Err(CdcAcmError::Transfer(
                                                PipeError::Disconnected,
                                            ))) => Err(Error::NotReady),
                                            Ok(Err(_)) => Err(fail_current_session(
                                                session_generation,
                                                Phase::CdcReady,
                                                Phase::CdcError,
                                                Error::Transfer,
                                            )
                                            .await),
                                            Err(_) => Err(record_current_session_error(
                                                session_generation,
                                                Phase::CdcReady,
                                                Error::Timeout,
                                            )
                                            .await),
                                        };
                                        send_reply(Reply {
                                            sequence,
                                            result: ReplyResult::Read(result),
                                        });
                                    }
                                    Either::Second(
                                        command @ Command {
                                            sequence,
                                            operation: Operation::Write(data),
                                            ..
                                        },
                                    ) => {
                                        if !command_is_current(&command).await {
                                            reject_command(command, Error::NotReady);
                                            continue;
                                        }
                                        let result = match with_timeout(
                                            TRANSFER_TIMEOUT,
                                            cdc.write(data.as_bytes()),
                                        )
                                        .await
                                        {
                                            Ok(Ok(count)) => {
                                                if record_tx_if_current(
                                                    session_generation,
                                                    Phase::CdcReady,
                                                    count,
                                                )
                                                .await
                                                {
                                                    Ok(count as u8)
                                                } else {
                                                    Err(Error::NotReady)
                                                }
                                            }
                                            Ok(Err(CdcAcmError::Transfer(
                                                PipeError::Disconnected,
                                            ))) => Err(Error::NotReady),
                                            Ok(Err(_)) => Err(fail_current_session(
                                                session_generation,
                                                Phase::CdcReady,
                                                Phase::CdcError,
                                                Error::Transfer,
                                            )
                                            .await),
                                            Err(_) => Err(record_current_session_error(
                                                session_generation,
                                                Phase::CdcReady,
                                                Error::Timeout,
                                            )
                                            .await),
                                        };
                                        send_reply(Reply {
                                            sequence,
                                            result: ReplyResult::Write(result),
                                        });
                                    }
                                    Either::Second(
                                        command @ Command {
                                            sequence,
                                            operation: Operation::Exchange { write, read_length },
                                            ..
                                        },
                                    ) => {
                                        if !command_is_current(&command).await {
                                            reject_command(command, Error::NotReady);
                                            continue;
                                        }
                                        // Keep the first bulk-IN poll adjacent to
                                        // bulk-OUT in this sole pipe-owning task.
                                        // Once data starts, collect subsequent USB
                                        // packets until the CDC stream is briefly
                                        // idle or the caller's fixed buffer is full.
                                        let result = match with_timeout(
                                            TRANSFER_TIMEOUT,
                                            cdc.write(write.as_bytes()),
                                        )
                                        .await
                                        {
                                            Ok(Ok(count)) => {
                                                if !record_tx_if_current(
                                                    session_generation,
                                                    Phase::CdcReady,
                                                    count,
                                                )
                                                .await
                                                {
                                                    Err(Error::NotReady)
                                                } else {
                                                    let mut data = CdcData::empty();
                                                    match with_timeout(
                                                        EXCHANGE_FIRST_RESPONSE_TIMEOUT,
                                                        cdc.read(
                                                            &mut data.bytes
                                                                [..usize::from(read_length)],
                                                        ),
                                                    )
                                                    .await
                                                    {
                                                        Ok(Ok(0)) => Err(fail_current_session(
                                                            session_generation,
                                                            Phase::CdcReady,
                                                            Phase::CdcError,
                                                            Error::Transfer,
                                                        )
                                                        .await),
                                                        Ok(Ok(count)) => {
                                                            data.len = count as u8;
                                                            if !record_rx_if_current(
                                                                session_generation,
                                                                Phase::CdcReady,
                                                                count,
                                                            )
                                                            .await
                                                            {
                                                                Err(Error::NotReady)
                                                            } else {
                                                                let mut exchange_result = Ok(data);
                                                                while usize::from(data.len)
                                                                    < usize::from(read_length)
                                                                {
                                                                    let start =
                                                                        usize::from(data.len);
                                                                    let end =
                                                                        usize::from(read_length);
                                                                    match with_timeout(
                                                                        EXCHANGE_IDLE_TIMEOUT,
                                                                        cdc.read(
                                                                            &mut data.bytes
                                                                                [start..end],
                                                                        ),
                                                                    )
                                                                    .await
                                                                    {
                                                                        Ok(Ok(0)) => {
                                                                            exchange_result = Err(
                                                                                fail_current_session(
                                                                                    session_generation,
                                                                                    Phase::CdcReady,
                                                                                    Phase::CdcError,
                                                                                    Error::Transfer,
                                                                                )
                                                                                .await,
                                                                            );
                                                                            break;
                                                                        }
                                                                        Ok(Ok(count)) => {
                                                                            debug_assert!(
                                                                                count
                                                                                    <= end - start
                                                                            );
                                                                            data.len += count as u8;
                                                                            if record_rx_if_current(
                                                                                session_generation,
                                                                                Phase::CdcReady,
                                                                                count,
                                                                            )
                                                                            .await
                                                                            {
                                                                                exchange_result =
                                                                                    Ok(data);
                                                                            } else {
                                                                                exchange_result = Err(
                                                                                    Error::NotReady,
                                                                                );
                                                                                break;
                                                                            }
                                                                        }
                                                                        Ok(Err(
                                                                            CdcAcmError::Transfer(
                                                                                PipeError::Disconnected,
                                                                            ),
                                                                        )) => {
                                                                            exchange_result = Err(
                                                                                Error::NotReady,
                                                                            );
                                                                            break;
                                                                        }
                                                                        Ok(Err(_)) => {
                                                                            exchange_result = Err(
                                                                                fail_current_session(
                                                                                    session_generation,
                                                                                    Phase::CdcReady,
                                                                                    Phase::CdcError,
                                                                                    Error::Transfer,
                                                                                )
                                                                                .await,
                                                                            );
                                                                            break;
                                                                        }
                                                                        Err(_) => {
                                                                            if !session_is_current(
                                                                                session_generation,
                                                                                Phase::CdcReady,
                                                                            )
                                                                            .await
                                                                            {
                                                                                exchange_result =
                                                                                    Err(
                                                                                        Error::NotReady,
                                                                                    );
                                                                            }
                                                                            break;
                                                                        }
                                                                    }
                                                                }
                                                                exchange_result
                                                            }
                                                        }
                                                        Ok(Err(CdcAcmError::Transfer(
                                                            PipeError::Disconnected,
                                                        ))) => Err(Error::NotReady),
                                                        Ok(Err(_)) => Err(fail_current_session(
                                                            session_generation,
                                                            Phase::CdcReady,
                                                            Phase::CdcError,
                                                            Error::Transfer,
                                                        )
                                                        .await),
                                                        Err(_) => {
                                                            Err(record_current_session_error(
                                                                session_generation,
                                                                Phase::CdcReady,
                                                                Error::Timeout,
                                                            )
                                                            .await)
                                                        }
                                                    }
                                                }
                                            }
                                            Ok(Err(CdcAcmError::Transfer(
                                                PipeError::Disconnected,
                                            ))) => Err(Error::NotReady),
                                            Ok(Err(_)) => Err(fail_current_session(
                                                session_generation,
                                                Phase::CdcReady,
                                                Phase::CdcError,
                                                Error::Transfer,
                                            )
                                            .await),
                                            Err(_) => Err(record_current_session_error(
                                                session_generation,
                                                Phase::CdcReady,
                                                Error::Timeout,
                                            )
                                            .await),
                                        };
                                        send_reply(Reply {
                                            sequence,
                                            result: ReplyResult::Exchange(result),
                                        });
                                    }
                                    Either::Second(command) => {
                                        reject_command(command, Error::NotReady);
                                    }
                                }
                            }
                        }
                    } else {
                        if set_error_phase_if_current(
                            session_generation,
                            Phase::Enumerating,
                            Phase::CdcError,
                        )
                        .await
                        {
                            warn!("PIO USB CDC-ACM standard controls failed");
                            loop {
                                match select(
                                    controller.wait_for_device_event(),
                                    HOST_COMMANDS.receive(),
                                )
                                .await
                                {
                                    Either::First(
                                        DeviceEvent::Disconnected | DeviceEvent::Overcurrent,
                                    ) => break,
                                    Either::First(_) => {}
                                    Either::Second(command) => {
                                        reject_command(command, Error::NotReady);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    if set_error_phase_if_current(
                        session_generation,
                        Phase::Enumerating,
                        Phase::CdcError,
                    )
                    .await
                    {
                        warn!("PIO USB device has no usable CDC-ACM function");
                        loop {
                            match select(
                                controller.wait_for_device_event(),
                                HOST_COMMANDS.receive(),
                            )
                            .await
                            {
                                Either::First(
                                    DeviceEvent::Disconnected | DeviceEvent::Overcurrent,
                                ) => {
                                    break;
                                }
                                Either::First(_) => {}
                                Either::Second(command) => {
                                    reject_command(command, Error::NotReady);
                                }
                            }
                        }
                    }
                }
            }

            bus_handle.free_address(address);
            set_waiting_if_current(session_generation).await;
            info!("PIO USB root address {} released", address);
        }
    };

    // Both futures borrow task-local host state and run for the task lifetime.
    join(root_port_monitor(&host_state), application).await;
}

#[embassy_executor::task]
pub(crate) async fn usb_host_task(hardware: Hardware) {
    run(hardware).await;
}
