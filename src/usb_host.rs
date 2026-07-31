//! PIO USB root-port manager for the Feather RP2040 USB Host profile.
//!
//! This task is the sole owner of enumeration, device addresses, CDC-ACM
//! pipes, and P8055 HID pipes. SCPI exchanges owned fixed-size messages with
//! it; GP13 remains exclusively owned by the board's existing
//! `StatusIndicator`.

use defmt::{info, warn};
use embassy_futures::join::join;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::Stack;
use embassy_net::tcp::{TcpReader, TcpSocket, TcpWriter};
use embassy_rp::Peri;
use embassy_rp::bind_interrupts;
use embassy_rp::dma::InterruptHandler as DmaInterruptHandler;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIN_16, PIN_17, PIN_18, PIO0, PIO1};
use embassy_rp::pio::InterruptHandler as PioInterruptHandler;
use embassy_rp_pio_usb_host::cdc_acm::{
    CdcAcmCreateError, CdcAcmError, CdcAcmHost,
    allocate_from_enumeration as allocate_cdc_from_enumeration,
};
use embassy_rp_pio_usb_host::ftdi::{
    FTDI_VENDOR_ID, FtdiError, FtdiHost,
    allocate_from_enumeration as allocate_ftdi_from_enumeration,
};
use embassy_rp_pio_usb_host::hid::{
    HidError, allocate_from_enumeration as allocate_hid_from_enumeration,
};
use embassy_rp_pio_usb_host::host::{
    DeviceEvent, PipeError, Speed, UsbHostController, UsbPipe, pipe,
};
use embassy_rp_pio_usb_host::pio_host::rp2040::{
    BadResponseDiagnostic, BadResponseSite, HandshakeFailure, Rp2040PioEngine,
};
use embassy_rp_pio_usb_host::pio_host::{PioHostState, snapshot_in_pipe_progress_diagnostics};
use embassy_rp_pio_usb_host::usb::{CdcLineCoding, ConfigurationError};
use embassy_rp_pio_usb_host::{AttachDetector, BusEvent, DeviceSpeed};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer, with_timeout};
use embassy_usb_host::{BusController, BusRoute, BusState, EnumerationError};

use crate::p8055;

const ATTACH_DEBOUNCE_SAMPLES: u16 = 100;
const CONFIG_DESCRIPTOR_CAPACITY: usize = 512;
const REPORT_DESCRIPTOR_CAPACITY: usize = 256;
const COMMAND_CAPACITY: usize = 4;
const BRIDGE_CHANNEL_CAPACITY: usize = 4;
const BRIDGE_SOCKET_BUFFER_SIZE: usize = 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(4);
const EXCHANGE_COMMAND_TIMEOUT: Duration = Duration::from_secs(18);
const CLASS_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2);
const EXCHANGE_FIRST_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_IDLE_TIMEOUT: Duration = Duration::from_millis(50);
const DIAGNOSTIC_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(100);
const DIAGNOSTIC_PAYLOAD_PREFIX_CAPACITY: usize = 8;
const ENUMERATION_RESET_RETRIES: u8 = 2;
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
    FtdiReady,
    P8055Ready,
    UnsupportedSpeed,
    UnsupportedDevice,
    EnumerationError,
    CdcError,
    FtdiError,
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
            Self::FtdiReady => "FTDI_READY",
            Self::P8055Ready => "P8055_READY",
            Self::UnsupportedSpeed => "UNSUPPORTED_SPEED",
            Self::UnsupportedDevice => "UNSUPPORTED_DEVICE",
            Self::EnumerationError => "ENUMERATION_ERROR",
            Self::CdcError => "CDC_ERROR",
            Self::FtdiError => "FTDI_ERROR",
            Self::P8055Error => "P8055_ERROR",
        }
    }

    const fn is_serial_ready(self) -> bool {
        matches!(self, Self::CdcReady | Self::FtdiReady)
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

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum EnumerationOrigin {
    None,
    Reset,
    Se1,
    Enumerate,
}

impl EnumerationOrigin {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Reset => "RESET",
            Self::Se1 => "SE1",
            Self::Enumerate => "ENUMERATE",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum EnumerationErrorKind {
    None,
    BufferOverflow,
    BadResponse,
    Babble,
    DataToggleError,
    Canceled,
    Stall,
    Timeout,
    Disconnected,
    InvalidDescriptor,
    ConfigBufferTooSmall,
    NoPipe,
    RequestFailed,
    Other,
}

impl EnumerationErrorKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::BufferOverflow => "BUFFER_OVERFLOW",
            Self::BadResponse => "BAD_RESPONSE",
            Self::Babble => "BABBLE",
            Self::DataToggleError => "DATA_TOGGLE_ERROR",
            Self::Canceled => "CANCELED",
            Self::Stall => "STALL",
            Self::Timeout => "TIMEOUT",
            Self::Disconnected => "DISCONNECTED",
            Self::InvalidDescriptor => "INVALID_DESCRIPTOR",
            Self::ConfigBufferTooSmall => "CONFIG_BUFFER_TOO_SMALL",
            Self::NoPipe => "NO_PIPE",
            Self::RequestFailed => "REQUEST_FAILED",
            Self::Other => "OTHER",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct EnumerationDiagnostic {
    attempts: u32,
    failures: u32,
    origin: EnumerationOrigin,
    error: EnumerationErrorKind,
    site: Option<BadResponseSite>,
    handshake: Option<HandshakeFailure>,
    setup_attempts: u32,
    setup: [u8; 8],
}

impl EnumerationDiagnostic {
    const fn none() -> Self {
        Self {
            attempts: 0,
            failures: 0,
            origin: EnumerationOrigin::None,
            error: EnumerationErrorKind::None,
            site: None,
            handshake: None,
            setup_attempts: 0,
            setup: [0; 8],
        }
    }

    const fn new(
        origin: EnumerationOrigin,
        error: EnumerationErrorKind,
        bad_response: Option<BadResponseDiagnostic>,
    ) -> Self {
        match bad_response {
            Some(diagnostic) => Self {
                attempts: 0,
                failures: 0,
                origin,
                error,
                site: Some(diagnostic.site),
                handshake: diagnostic.handshake_failure,
                setup_attempts: diagnostic.setup_attempts,
                setup: diagnostic.setup,
            },
            None => Self {
                attempts: 0,
                failures: 0,
                origin,
                error,
                site: None,
                handshake: None,
                setup_attempts: 0,
                setup: [0; 8],
            },
        }
    }

    fn begin_attempt(&mut self) {
        self.attempts = self.attempts.wrapping_add(1);
    }

    fn record_failure(&mut self, diagnostic: Self) {
        self.failures = self.failures.wrapping_add(1);
        self.origin = diagnostic.origin;
        self.error = diagnostic.error;
        self.site = diagnostic.site;
        self.handshake = diagnostic.handshake;
        self.setup_attempts = diagnostic.setup_attempts;
        self.setup = diagnostic.setup;
    }

    pub(crate) const fn attempts(self) -> u32 {
        self.attempts
    }

    pub(crate) const fn failures(self) -> u32 {
        self.failures
    }

    pub(crate) const fn origin(self) -> &'static str {
        self.origin.as_str()
    }

    pub(crate) const fn error(self) -> &'static str {
        self.error.as_str()
    }

    pub(crate) const fn site(self) -> &'static str {
        match self.site {
            Some(BadResponseSite::ControlInContract) => "CONTROL_IN_CONTRACT",
            Some(BadResponseSite::ControlInSetup) => "CONTROL_IN_SETUP",
            Some(BadResponseSite::ControlInData) => "CONTROL_IN_DATA",
            Some(BadResponseSite::ControlInStatus) => "CONTROL_IN_STATUS",
            Some(BadResponseSite::ControlOutContract) => "CONTROL_OUT_CONTRACT",
            Some(BadResponseSite::ControlOutSetup) => "CONTROL_OUT_SETUP",
            Some(BadResponseSite::ControlOutData) => "CONTROL_OUT_DATA",
            Some(BadResponseSite::ControlOutStatus) => "CONTROL_OUT_STATUS",
            None => "NONE",
        }
    }

    pub(crate) const fn handshake(self) -> &'static str {
        match self.handshake {
            Some(HandshakeFailure::RxDecoderError) => "RX_DECODER_ERROR",
            Some(HandshakeFailure::FalseStart) => "FALSE_START",
            Some(HandshakeFailure::IncompletePacket) => "INCOMPLETE_PACKET",
            Some(HandshakeFailure::WrongLength) => "WRONG_LENGTH",
            Some(HandshakeFailure::InvalidSync) => "INVALID_SYNC",
            Some(HandshakeFailure::InvalidPidComplement) => "INVALID_PID_COMPLEMENT",
            Some(HandshakeFailure::UnexpectedPid) => "UNEXPECTED_PID",
            Some(HandshakeFailure::Unknown) => "UNKNOWN",
            None => "NONE",
        }
    }

    pub(crate) const fn setup_attempts(self) -> u32 {
        self.setup_attempts
    }

    pub(crate) const fn setup(self) -> [u8; 8] {
        self.setup
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
    pub(crate) bridge_connected: bool,
    pub(crate) unexpected_toggle_count: u32,
    pub(crate) accepted_zlp_count: u32,
    pub(crate) latest_expected_pid: Option<u8>,
    pub(crate) latest_actual_pid: Option<u8>,
    pub(crate) latest_payload_len: Option<u8>,
    pub(crate) latest_payload_prefix_len: u8,
    pub(crate) latest_payload_prefix: [u8; DIAGNOSTIC_PAYLOAD_PREFIX_CAPACITY],
    pub(crate) bridge_out_transfers: u32,
    pub(crate) latest_bridge_out_len: u8,
    pub(crate) bridge_in_starts: u32,
    pub(crate) bridge_in_completions: u32,
    pub(crate) bridge_in_cancellations: u32,
    pub(crate) bridge_in_forwards: u32,
    pub(crate) bridge_last_outcome: u8,
    pub(crate) bridge_tcp_end: u8,
    pub(crate) pipe_in_starts: u32,
    pub(crate) pipe_in_deadline_ready: u32,
    pub(crate) pipe_in_engine_acquired: u32,
    pub(crate) pipe_in_engine_returned: u32,
    pub(crate) pipe_in_service_returned: u32,
    pub(crate) wire_in_attempts: u32,
    pub(crate) wire_in_data_accepted: u32,
    pub(crate) wire_in_nak: u32,
    pub(crate) wire_in_no_response: u32,
    pub(crate) wire_in_invalid_or_stall: u32,
    enumeration_diagnostic: EnumerationDiagnostic,
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
            bridge_connected: false,
            unexpected_toggle_count: 0,
            accepted_zlp_count: 0,
            latest_expected_pid: None,
            latest_actual_pid: None,
            latest_payload_len: None,
            latest_payload_prefix_len: 0,
            latest_payload_prefix: [0; DIAGNOSTIC_PAYLOAD_PREFIX_CAPACITY],
            bridge_out_transfers: 0,
            latest_bridge_out_len: 0,
            bridge_in_starts: 0,
            bridge_in_completions: 0,
            bridge_in_cancellations: 0,
            bridge_in_forwards: 0,
            bridge_last_outcome: 0,
            bridge_tcp_end: 0,
            pipe_in_starts: 0,
            pipe_in_deadline_ready: 0,
            pipe_in_engine_acquired: 0,
            pipe_in_engine_returned: 0,
            pipe_in_service_returned: 0,
            wire_in_attempts: 0,
            wire_in_data_accepted: 0,
            wire_in_nak: 0,
            wire_in_no_response: 0,
            wire_in_invalid_or_stall: 0,
            enumeration_diagnostic: EnumerationDiagnostic::none(),
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
        self.bridge_connected = false;
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

struct CdcRxBuffer {
    start: u8,
    end: u8,
    bytes: [u8; CDC_MAX_TRANSFER],
}

impl CdcRxBuffer {
    const fn empty() -> Self {
        Self {
            start: 0,
            end: 0,
            bytes: [0; CDC_MAX_TRANSFER],
        }
    }

    fn copy_into(&mut self, destination: &mut [u8]) -> usize {
        let available = usize::from(self.end - self.start);
        let count = available.min(destination.len());
        let start = usize::from(self.start);
        destination[..count].copy_from_slice(&self.bytes[start..start + count]);
        self.start += count as u8;
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
        count
    }

    fn load(&mut self, packet: &[u8]) {
        debug_assert_eq!(self.start, self.end);
        debug_assert!(packet.len() <= self.bytes.len());
        self.bytes[..packet.len()].copy_from_slice(packet);
        self.start = 0;
        self.end = packet.len() as u8;
    }

    fn take(&mut self) -> Option<CdcData> {
        if self.start == self.end {
            return None;
        }
        let mut data = CdcData::empty();
        data.len = self.end - self.start;
        let start = usize::from(self.start);
        data.bytes[..usize::from(data.len)]
            .copy_from_slice(&self.bytes[start..usize::from(self.end)]);
        self.start = 0;
        self.end = 0;
        Some(data)
    }
}

struct ManagedCdcRead {
    copied: usize,
    received: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Error {
    InvalidLength,
    InvalidHex,
    InvalidParameter,
    DataStale,
    NotReady,
    ResourceBusy,
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
    BridgeOpen,
    BridgeClose { session: u32 },
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
            Self::BridgeOpen | Self::BridgeClose { .. } => Phase::CdcReady,
        }
    }

    const fn is_cdc_data(self) -> bool {
        matches!(
            self,
            Self::Read { .. } | Self::Write(_) | Self::Exchange { .. }
        )
    }

    const fn is_serial_operation(self) -> bool {
        self.is_cdc_data() || matches!(self, Self::BridgeOpen | Self::BridgeClose { .. })
    }

    fn is_ready_in(self, phase: Phase) -> bool {
        if self.is_serial_operation() {
            phase.is_serial_ready()
        } else {
            phase == self.ready_phase()
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
            | Self::P8055GetDebounce { .. }
            | Self::BridgeOpen
            | Self::BridgeClose { .. } => COMMAND_TIMEOUT,
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
    BridgeOpen(Result<u32, Error>),
    BridgeClose(Result<(), Error>),
}

#[derive(Clone, Copy)]
struct Reply {
    sequence: u32,
    result: ReplyResult,
}

#[derive(Clone, Copy)]
struct BridgeFrame {
    session: u32,
    data: CdcData,
}

#[derive(Clone, Copy)]
enum BridgeEvent {
    Closed { session: u32 },
}

static HOST_STATE: Mutex<CriticalSectionRawMutex, Status> = Mutex::new(Status::power_off());
static HOST_COMMANDS: Channel<CriticalSectionRawMutex, Command, COMMAND_CAPACITY> = Channel::new();
static HOST_REPLIES: Channel<CriticalSectionRawMutex, Reply, COMMAND_CAPACITY> = Channel::new();
static HOST_COMMAND_LOCK: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());
static HOST_COMMAND_SEQUENCE: Mutex<CriticalSectionRawMutex, u32> = Mutex::new(0);
static BRIDGE_TO_USB: Channel<CriticalSectionRawMutex, BridgeFrame, BRIDGE_CHANNEL_CAPACITY> =
    Channel::new();
static USB_TO_BRIDGE: Channel<CriticalSectionRawMutex, BridgeFrame, BRIDGE_CHANNEL_CAPACITY> =
    Channel::new();
static BRIDGE_EVENT: Signal<CriticalSectionRawMutex, BridgeEvent> = Signal::new();

pub(crate) async fn status() -> Status {
    let progress = snapshot_in_pipe_progress_diagnostics();
    let mut status = *HOST_STATE.lock().await;
    status.pipe_in_starts = progress.starts;
    status.pipe_in_deadline_ready = progress.deadline_ready;
    status.pipe_in_engine_acquired = progress.engine_acquired;
    status.pipe_in_engine_returned = progress.engine_returned;
    status.pipe_in_service_returned = progress.service_returned;
    status
}

pub(crate) async fn enumeration_diagnostic() -> EnumerationDiagnostic {
    HOST_STATE.lock().await.enumeration_diagnostic
}

fn pipe_enumeration_error(error: PipeError) -> EnumerationErrorKind {
    match error {
        PipeError::BufferOverflow => EnumerationErrorKind::BufferOverflow,
        PipeError::BadResponse => EnumerationErrorKind::BadResponse,
        PipeError::Babble => EnumerationErrorKind::Babble,
        PipeError::DataToggleError => EnumerationErrorKind::DataToggleError,
        PipeError::Canceled => EnumerationErrorKind::Canceled,
        PipeError::Stall => EnumerationErrorKind::Stall,
        PipeError::Timeout => EnumerationErrorKind::Timeout,
        PipeError::Disconnected => EnumerationErrorKind::Disconnected,
        _ => EnumerationErrorKind::Other,
    }
}

fn enumeration_error(error: &EnumerationError) -> EnumerationErrorKind {
    match error {
        EnumerationError::Transfer(error) => pipe_enumeration_error(*error),
        EnumerationError::InvalidDescriptor => EnumerationErrorKind::InvalidDescriptor,
        EnumerationError::ConfigBufferTooSmall(_) => EnumerationErrorKind::ConfigBufferTooSmall,
        EnumerationError::NoPipe => EnumerationErrorKind::NoPipe,
        EnumerationError::RequestFailed => EnumerationErrorKind::RequestFailed,
    }
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
    if !operation.is_ready_in(state.phase) {
        return Err(Error::NotReady);
    }
    if state.bridge_connected && operation.is_cdc_data() {
        return Err(Error::ResourceBusy);
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

async fn bridge_open() -> Result<u32, Error> {
    match send_command(Operation::BridgeOpen).await? {
        ReplyResult::BridgeOpen(result) => result,
        _ => Err(Error::Transfer),
    }
}

async fn bridge_close(session: u32) -> Result<(), Error> {
    match send_command(Operation::BridgeClose { session }).await? {
        ReplyResult::BridgeClose(result) => result,
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
    state.rx_bytes = 0;
    state.tx_bytes = 0;
    state.error_count = 0;
    state.bridge_connected = false;
    state.unexpected_toggle_count = 0;
    state.accepted_zlp_count = 0;
    state.latest_expected_pid = None;
    state.latest_actual_pid = None;
    state.latest_payload_len = None;
    state.latest_payload_prefix_len = 0;
    state.latest_payload_prefix = [0; DIAGNOSTIC_PAYLOAD_PREFIX_CAPACITY];
    state.bridge_out_transfers = 0;
    state.latest_bridge_out_len = 0;
    state.bridge_in_starts = 0;
    state.bridge_in_completions = 0;
    state.bridge_in_cancellations = 0;
    state.bridge_in_forwards = 0;
    state.bridge_last_outcome = 0;
    state.bridge_tcp_end = 0;
    state.pipe_in_starts = 0;
    state.pipe_in_deadline_ready = 0;
    state.pipe_in_engine_acquired = 0;
    state.pipe_in_engine_returned = 0;
    state.pipe_in_service_returned = 0;
    state.wire_in_attempts = 0;
    state.wire_in_data_accepted = 0;
    state.wire_in_nak = 0;
    state.wire_in_no_response = 0;
    state.wire_in_invalid_or_stall = 0;
    state.enumeration_diagnostic.begin_attempt();
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
    state.bridge_connected = false;
}

async fn set_enumeration_error(diagnostic: EnumerationDiagnostic) {
    let mut state = HOST_STATE.lock().await;
    state.phase = Phase::EnumerationError;
    state.error_count = state.error_count.wrapping_add(1);
    state.bridge_connected = false;
    state.enumeration_diagnostic.record_failure(diagnostic);
}

async fn set_enumeration_error_if_current(
    generation: u32,
    diagnostic: EnumerationDiagnostic,
) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != Phase::Enumerating {
        return false;
    }
    state.phase = Phase::EnumerationError;
    state.error_count = state.error_count.wrapping_add(1);
    state.bridge_connected = false;
    state.enumeration_diagnostic.record_failure(diagnostic);
    true
}

async fn set_error_phase_if_current(generation: u32, expected: Phase, phase: Phase) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != expected {
        return false;
    }
    state.phase = phase;
    state.error_count = state.error_count.wrapping_add(1);
    state.bridge_connected = false;
    true
}

async fn set_bridge_connected_if_current(
    generation: u32,
    ready_phase: Phase,
    expected: bool,
    connected: bool,
) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation
        || state.phase != ready_phase
        || state.bridge_connected != expected
    {
        return false;
    }
    state.bridge_connected = connected;
    if connected {
        state.bridge_in_starts = 0;
        state.bridge_in_completions = 0;
        state.bridge_in_cancellations = 0;
        state.bridge_in_forwards = 0;
        state.bridge_last_outcome = 0;
        state.bridge_tcp_end = 0;
    }
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

async fn record_bridge_tx_if_current(generation: u32, ready_phase: Phase, count: usize) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != ready_phase {
        return false;
    }
    state.tx_bytes = state.tx_bytes.wrapping_add(count as u32);
    state.bridge_out_transfers = state.bridge_out_transfers.wrapping_add(1);
    state.latest_bridge_out_len = count as u8;
    true
}

async fn record_bridge_in_start_if_current(generation: u32, ready_phase: Phase) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != ready_phase || !state.bridge_connected {
        return false;
    }
    state.bridge_in_starts = state.bridge_in_starts.wrapping_add(1);
    true
}

async fn record_bridge_in_cycle_if_current(
    generation: u32,
    ready_phase: Phase,
    received: usize,
    forwarded: bool,
    start_next: bool,
) -> bool {
    let mut state = HOST_STATE.lock().await;
    if state.generation != generation || state.phase != ready_phase || !state.bridge_connected {
        return false;
    }
    state.bridge_in_completions = state.bridge_in_completions.wrapping_add(1);
    state.rx_bytes = state.rx_bytes.wrapping_add(received as u32);
    if forwarded {
        state.bridge_in_forwards = state.bridge_in_forwards.wrapping_add(1);
    }
    if start_next {
        state.bridge_in_starts = state.bridge_in_starts.wrapping_add(1);
    }
    true
}

async fn record_bridge_outcome_if_current(generation: u32, ready_phase: Phase, outcome: u8) {
    let mut state = HOST_STATE.lock().await;
    if state.generation == generation && state.phase == ready_phase {
        let pending = state
            .bridge_in_starts
            .saturating_sub(state.bridge_in_completions);
        state.bridge_in_cancellations = state.bridge_in_cancellations.wrapping_add(pending);
        state.bridge_last_outcome = outcome;
    }
}

async fn record_bridge_tcp_end_if_connected(outcome: u8) {
    let mut state = HOST_STATE.lock().await;
    if state.phase.is_serial_ready() && state.bridge_connected {
        state.bridge_tcp_end = outcome;
    }
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
        Operation::BridgeOpen => ReplyResult::BridgeOpen(Err(error)),
        Operation::BridgeClose { .. } => ReplyResult::BridgeClose(Err(error)),
    };
    send_reply(Reply {
        sequence: command.sequence,
        result,
    });
}

async fn command_is_current(command: &Command) -> bool {
    let state = status().await;
    state.generation == command.generation && command.operation.is_ready_in(state.phase)
}

async fn session_is_current(generation: u32, expected: Phase) -> bool {
    let state = status().await;
    state.generation == generation && state.phase == expected
}

enum BridgeIoEnd {
    Closed { sequence: u32 },
    Failed,
}

enum BridgeRunOutcome {
    Closed { sequence: u32 },
    Disconnected,
    Failed,
}

#[derive(Clone, Copy)]
enum SerialWireFormat {
    Cdc,
    Ftdi,
}

async fn managed_cdc_read<C, I, O>(
    cdc: &mut CdcAcmHost<C, I, O>,
    buffered: &mut CdcRxBuffer,
    destination: &mut [u8],
) -> Result<ManagedCdcRead, CdcAcmError>
where
    C: UsbPipe<pipe::Control, pipe::InOut>,
    I: UsbPipe<pipe::Bulk, pipe::In>,
    O: UsbPipe<pipe::Bulk, pipe::Out>,
{
    if buffered.start != buffered.end {
        return Ok(ManagedCdcRead {
            copied: buffered.copy_into(destination),
            received: 0,
        });
    }

    // Always drain a complete full-speed packet from CdcAcmHost. Any bytes
    // beyond the SCPI caller's requested length live here instead of in the
    // class driver's private buffer, so a later bridge split cannot lose them.
    let mut packet = [0_u8; CDC_MAX_TRANSFER];
    let received = cdc.read(&mut packet).await?;
    buffered.load(&packet[..received]);
    Ok(ManagedCdcRead {
        copied: buffered.copy_into(destination),
        received,
    })
}

async fn bridge_wait_for_disconnect<'d, C>(controller: &mut BusController<'d, C>)
where
    C: UsbHostController<'d>,
{
    loop {
        match controller.wait_for_device_event().await {
            DeviceEvent::Disconnected | DeviceEvent::Overcurrent => return,
            _ => {}
        }
    }
}

async fn bridge_write_frame<O>(
    bulk_out: &mut O,
    generation: u32,
    ready_phase: Phase,
    error_phase: Phase,
    frame: BridgeFrame,
) -> Result<(), BridgeIoEnd>
where
    O: UsbPipe<pipe::Bulk, pipe::Out>,
{
    let count = frame.data.as_bytes().len();
    match with_timeout(
        TRANSFER_TIMEOUT,
        bulk_out.request_out(frame.data.as_bytes(), false),
    )
    .await
    {
        Ok(Ok(())) => {
            if record_bridge_tx_if_current(generation, ready_phase, count).await {
                Ok(())
            } else {
                Err(BridgeIoEnd::Failed)
            }
        }
        Ok(Err(PipeError::Disconnected)) => Err(BridgeIoEnd::Failed),
        Ok(Err(_)) => {
            fail_current_session(generation, ready_phase, error_phase, Error::Transfer).await;
            Err(BridgeIoEnd::Failed)
        }
        Err(_) => {
            fail_current_session(generation, ready_phase, error_phase, Error::Timeout).await;
            Err(BridgeIoEnd::Failed)
        }
    }
}

async fn handle_bridge_command(command: Command, session: u32) -> Option<BridgeIoEnd> {
    if !command_is_current(&command).await {
        reject_command(command, Error::NotReady);
        return None;
    }

    match command.operation {
        Operation::BridgeClose {
            session: close_session,
        } if close_session == session => Some(BridgeIoEnd::Closed {
            sequence: command.sequence,
        }),
        Operation::BridgeOpen | Operation::BridgeClose { .. } => {
            reject_command(command, Error::ResourceBusy);
            None
        }
        operation if operation.is_cdc_data() => {
            reject_command(command, Error::ResourceBusy);
            None
        }
        _ => {
            reject_command(command, Error::NotReady);
            None
        }
    }
}

async fn bridge_usb_in<I>(
    bulk_in: &mut I,
    session: u32,
    generation: u32,
    packet_size: usize,
    ready_phase: Phase,
    error_phase: Phase,
    wire_format: SerialWireFormat,
) -> BridgeIoEnd
where
    I: UsbPipe<pipe::Bulk, pipe::In>,
{
    if !record_bridge_in_start_if_current(generation, ready_phase).await {
        return BridgeIoEnd::Failed;
    }

    loop {
        let mut output = CdcData::empty();
        let mut first = true;

        loop {
            let mut packet = [0_u8; CDC_MAX_TRANSFER];
            let request = bulk_in.request_in(&mut packet[..packet_size]);
            let result = if first {
                Ok(request.await)
            } else {
                with_timeout(EXCHANGE_IDLE_TIMEOUT, request).await
            };

            let received = match result {
                Err(_) => break,
                Ok(Ok(count)) if count <= packet_size => count,
                Ok(Ok(_)) => {
                    let _ =
                        record_bridge_in_cycle_if_current(generation, ready_phase, 0, false, false)
                            .await;
                    fail_current_session(generation, ready_phase, error_phase, Error::Transfer)
                        .await;
                    return BridgeIoEnd::Failed;
                }
                Ok(Err(PipeError::Disconnected)) => {
                    let _ =
                        record_bridge_in_cycle_if_current(generation, ready_phase, 0, false, false)
                            .await;
                    return BridgeIoEnd::Failed;
                }
                Ok(Err(_)) => {
                    let _ =
                        record_bridge_in_cycle_if_current(generation, ready_phase, 0, false, false)
                            .await;
                    fail_current_session(generation, ready_phase, error_phase, Error::Transfer)
                        .await;
                    return BridgeIoEnd::Failed;
                }
            };

            let (payload_start, count) = match wire_format {
                SerialWireFormat::Cdc => (0, received),
                SerialWireFormat::Ftdi if received >= 2 => (2, received - 2),
                SerialWireFormat::Ftdi => (received, 0),
            };
            first = false;
            if !record_bridge_in_cycle_if_current(generation, ready_phase, count, count != 0, true)
                .await
            {
                return BridgeIoEnd::Failed;
            }
            if count == 0 {
                break;
            }

            let start = usize::from(output.len);
            if start + count > output.bytes.len() {
                let frame = BridgeFrame {
                    session,
                    data: output,
                };
                if USB_TO_BRIDGE.try_send(frame).is_err() {
                    USB_TO_BRIDGE.send(frame).await;
                }
                output = CdcData::empty();
            }

            let start = usize::from(output.len);
            output.bytes[start..start + count]
                .copy_from_slice(&packet[payload_start..payload_start + count]);
            output.len += count as u8;
            if usize::from(output.len) == output.bytes.len() {
                break;
            }
        }

        if output.len != 0 {
            let frame = BridgeFrame {
                session,
                data: output,
            };
            if USB_TO_BRIDGE.try_send(frame).is_err() {
                USB_TO_BRIDGE.send(frame).await;
            }
        }
    }
}

async fn bridge_usb_out<O>(
    bulk_out: &mut O,
    session: u32,
    generation: u32,
    ready_phase: Phase,
    error_phase: Phase,
) -> BridgeIoEnd
where
    O: UsbPipe<pipe::Bulk, pipe::Out>,
{
    loop {
        let frame = BRIDGE_TO_USB.receive().await;
        if frame.session != session {
            continue;
        }
        if let Err(end) =
            bridge_write_frame(bulk_out, generation, ready_phase, error_phase, frame).await
        {
            return end;
        }
    }
}

async fn bridge_commands(session: u32) -> BridgeIoEnd {
    loop {
        if let Some(end) = handle_bridge_command(HOST_COMMANDS.receive().await, session).await {
            return end;
        }
    }
}

async fn bridge_usb_io<I, O>(
    bulk_in: &mut I,
    bulk_out: &mut O,
    session: u32,
    generation: u32,
    packet_size: usize,
    initial: Option<CdcData>,
    ready_phase: Phase,
    error_phase: Phase,
    wire_format: SerialWireFormat,
) -> BridgeIoEnd
where
    I: UsbPipe<pipe::Bulk, pipe::In>,
    O: UsbPipe<pipe::Bulk, pipe::Out>,
{
    if let Some(data) = initial {
        USB_TO_BRIDGE.send(BridgeFrame { session, data }).await;
    }

    // Match the proven SCPI exchange for the first raw frame: do not issue
    // any speculative bulk-IN token before the first command has completed
    // its bulk-OUT transaction.
    loop {
        match select(BRIDGE_TO_USB.receive(), HOST_COMMANDS.receive()).await {
            Either::First(frame) if frame.session == session => {
                if let Err(end) =
                    bridge_write_frame(bulk_out, generation, ready_phase, error_phase, frame).await
                {
                    return end;
                }
                break;
            }
            Either::First(_) => {}
            Either::Second(command) => {
                if let Some(end) = handle_bridge_command(command, session).await {
                    return end;
                }
            }
        }
    }

    // Keep the bulk-IN future alive while independent OUT and manager-command
    // loops wait concurrently. The shared host engine serializes their actual
    // wire transactions without repeatedly cancelling the dormant IN retry.
    match select3(
        bridge_usb_out(bulk_out, session, generation, ready_phase, error_phase),
        bridge_commands(session),
        bridge_usb_in(
            bulk_in,
            session,
            generation,
            packet_size,
            ready_phase,
            error_phase,
            wire_format,
        ),
    )
    .await
    {
        Either3::First(end) | Either3::Second(end) | Either3::Third(end) => end,
    }
}

async fn run_bridge<'d, H, C, I, O>(
    controller: &mut BusController<'d, H>,
    cdc: CdcAcmHost<C, I, O>,
    buffered: &mut CdcRxBuffer,
    session: u32,
    generation: u32,
) -> (CdcAcmHost<C, I, O>, BridgeRunOutcome)
where
    H: UsbHostController<'d>,
    C: UsbPipe<pipe::Control, pipe::InOut>,
    I: UsbPipe<pipe::Bulk, pipe::In>,
    O: UsbPipe<pipe::Bulk, pipe::Out>,
{
    let packet_size = usize::from(cdc.function().bulk_in_endpoint.max_packet_size);
    let initial = buffered.take();
    let (function, control, mut bulk_in, mut bulk_out) = cdc.into_parts();

    let outcome = match select(
        bridge_wait_for_disconnect(controller),
        bridge_usb_io(
            &mut bulk_in,
            &mut bulk_out,
            session,
            generation,
            packet_size,
            initial,
            Phase::CdcReady,
            Phase::CdcError,
            SerialWireFormat::Cdc,
        ),
    )
    .await
    {
        Either::First(()) => BridgeRunOutcome::Disconnected,
        Either::Second(BridgeIoEnd::Closed { sequence }) => BridgeRunOutcome::Closed { sequence },
        Either::Second(BridgeIoEnd::Failed) => BridgeRunOutcome::Failed,
    };

    let outcome_code = match &outcome {
        BridgeRunOutcome::Closed { .. } => 1,
        BridgeRunOutcome::Disconnected => 2,
        BridgeRunOutcome::Failed => 3,
    };
    record_bridge_outcome_if_current(generation, Phase::CdcReady, outcome_code).await;
    let released = set_bridge_connected_if_current(generation, Phase::CdcReady, true, false).await;
    if let BridgeRunOutcome::Closed { sequence } = outcome {
        send_reply(Reply {
            sequence,
            result: ReplyResult::BridgeClose(if released {
                Ok(())
            } else {
                Err(Error::NotReady)
            }),
        });
    } else {
        BRIDGE_EVENT.signal(BridgeEvent::Closed { session });
    }

    (
        CdcAcmHost::new(function, control, bulk_in, bulk_out),
        outcome,
    )
}

async fn run_ftdi_bridge<'d, H, C, I, O>(
    controller: &mut BusController<'d, H>,
    ftdi: FtdiHost<C, I, O>,
    session: u32,
    generation: u32,
) -> (FtdiHost<C, I, O>, BridgeRunOutcome)
where
    H: UsbHostController<'d>,
    C: UsbPipe<pipe::Control, pipe::InOut>,
    I: UsbPipe<pipe::Bulk, pipe::In>,
    O: UsbPipe<pipe::Bulk, pipe::Out>,
{
    let packet_size = usize::from(ftdi.interface().bulk_in_endpoint.max_packet_size);
    let (device, interface, control, mut bulk_in, mut bulk_out) = ftdi.into_parts();
    let outcome = match select(
        bridge_wait_for_disconnect(controller),
        bridge_usb_io(
            &mut bulk_in,
            &mut bulk_out,
            session,
            generation,
            packet_size,
            None,
            Phase::FtdiReady,
            Phase::FtdiError,
            SerialWireFormat::Ftdi,
        ),
    )
    .await
    {
        Either::First(()) => BridgeRunOutcome::Disconnected,
        Either::Second(BridgeIoEnd::Closed { sequence }) => BridgeRunOutcome::Closed { sequence },
        Either::Second(BridgeIoEnd::Failed) => BridgeRunOutcome::Failed,
    };

    let outcome_code = match &outcome {
        BridgeRunOutcome::Closed { .. } => 1,
        BridgeRunOutcome::Disconnected => 2,
        BridgeRunOutcome::Failed => 3,
    };
    record_bridge_outcome_if_current(generation, Phase::FtdiReady, outcome_code).await;
    let released = set_bridge_connected_if_current(generation, Phase::FtdiReady, true, false).await;
    if let BridgeRunOutcome::Closed { sequence } = outcome {
        send_reply(Reply {
            sequence,
            result: ReplyResult::BridgeClose(if released {
                Ok(())
            } else {
                Err(Error::NotReady)
            }),
        });
    } else {
        BRIDGE_EVENT.signal(BridgeEvent::Closed { session });
    }

    (
        FtdiHost::new(device, interface, control, bulk_in, bulk_out),
        outcome,
    )
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
    let mut next_diagnostic_snapshot = Instant::now() + DIAGNOSTIC_SNAPSHOT_INTERVAL;
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
                        Err(error) => {
                            connected = false;
                            set_enumeration_error(EnumerationDiagnostic::new(
                                EnumerationOrigin::Reset,
                                pipe_enumeration_error(error),
                                None,
                            ))
                            .await;
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
                    set_enumeration_error(EnumerationDiagnostic::new(
                        EnumerationOrigin::Se1,
                        EnumerationErrorKind::None,
                        None,
                    ))
                    .await;
                    warn!("invalid SE1 state on PIO USB root port");
                }
            }
        }

        if connected {
            let _ = host_state.service_frame().await;
        }

        if Instant::now() >= next_diagnostic_snapshot {
            let diagnostics = host_state.snapshot_in_transaction_diagnostics().await;
            let mut status = HOST_STATE.lock().await;
            status.wire_in_attempts = diagnostics.attempt_count;
            status.wire_in_data_accepted = diagnostics.accepted_data_count;
            status.wire_in_nak = diagnostics.nak_count;
            status.wire_in_no_response = diagnostics.no_response_count;
            status.wire_in_invalid_or_stall = diagnostics.invalid_or_stall_count;
            status.unexpected_toggle_count = diagnostics.unexpected_toggle_count;
            status.accepted_zlp_count = diagnostics.accepted_zlp_count;
            match diagnostics.latest_unexpected_toggle {
                Some(latest) => {
                    status.latest_expected_pid = Some(latest.expected_pid);
                    status.latest_actual_pid = Some(latest.actual_pid);
                    status.latest_payload_len = Some(latest.payload_len);
                    status.latest_payload_prefix_len = latest.payload_prefix_len;
                    status.latest_payload_prefix = latest.payload_prefix;
                }
                None => {
                    status.latest_expected_pid = None;
                    status.latest_actual_pid = None;
                    status.latest_payload_len = None;
                    status.latest_payload_prefix_len = 0;
                    status.latest_payload_prefix = [0; DIAGNOSTIC_PAYLOAD_PREFIX_CAPACITY];
                }
            }
            next_diagnostic_snapshot = Instant::now() + DIAGNOSTIC_SNAPSHOT_INTERVAL;
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

    let application_host_state = &host_state;
    let application = async move {
        let _vbus_enable = vbus_enable;
        let mut next_bridge_session = 0_u32;

        'device: loop {
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
            let Some(mut session_generation) = begin_enumeration(host_speed).await else {
                continue;
            };
            let mut configuration = [0_u8; CONFIG_DESCRIPTOR_CAPACITY];
            let mut reset_retries_remaining = ENUMERATION_RESET_RETRIES;
            let (enumeration, configuration_len) = 'enumeration: loop {
                application_host_state.clear_bad_response_diagnostic().await;
                match bus_handle
                    .enumerate(BusRoute::Direct(speed), &mut configuration)
                    .await
                {
                    Ok(result) => break 'enumeration result,
                    Err(error) => {
                        let bad_response =
                            application_host_state.take_bad_response_diagnostic().await;
                        if !set_enumeration_error_if_current(
                            session_generation,
                            EnumerationDiagnostic::new(
                                EnumerationOrigin::Enumerate,
                                enumeration_error(&error),
                                bad_response,
                            ),
                        )
                        .await
                        {
                            continue 'device;
                        }

                        // The failed enumeration has dropped its logical
                        // control pipe. This root-only backend has no hubs, so
                        // reclaim the complete address pool before a fresh
                        // root-port reset.
                        for address in 1_u8..=127 {
                            bus_handle.free_address(address);
                        }

                        if reset_retries_remaining == 0 {
                            warn!("PIO USB device enumeration failed");
                            wait_for_removal(&mut controller).await;
                            set_waiting_if_current(session_generation).await;
                            continue 'device;
                        }

                        reset_retries_remaining -= 1;
                        warn!("retrying PIO USB enumeration after root-port reset");
                        set_resetting(host_speed).await;
                        controller.controller_mut().bus_reset().await;
                        let Some(next_generation) = begin_enumeration(host_speed).await else {
                            continue 'device;
                        };
                        session_generation = next_generation;
                    }
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

            if enumeration.device_desc.vendor_id == FTDI_VENDOR_ID {
                match allocate_ftdi_from_enumeration(
                    &bus_handle,
                    &configuration[..configuration_len],
                    &enumeration,
                ) {
                    Ok(mut ftdi) => {
                        let controls_ready = matches!(
                            with_timeout(CLASS_CONTROL_TIMEOUT, async {
                                ftdi.configure_8n1(115_200).await?;
                                ftdi.set_dtr_rts(true, true).await?;
                                Ok::<(), FtdiError>(())
                            })
                            .await,
                            Ok(Ok(()))
                        );

                        if controls_ready {
                            ftdi.reset_data_toggles();
                            if set_phase_if_current(
                                session_generation,
                                Phase::Enumerating,
                                Phase::FtdiReady,
                            )
                            .await
                            {
                                info!(
                                    "PIO USB FTDI UART ready at address {}, PID={=u16:04x}, baud={}",
                                    address,
                                    enumeration.device_desc.product_id,
                                    ftdi.baud_rate()
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
                                                operation: Operation::BridgeOpen,
                                                ..
                                            },
                                        ) => {
                                            if !command_is_current(&command).await {
                                                reject_command(command, Error::NotReady);
                                                continue;
                                            }
                                            if !set_bridge_connected_if_current(
                                                session_generation,
                                                Phase::FtdiReady,
                                                false,
                                                true,
                                            )
                                            .await
                                            {
                                                reject_command(command, Error::ResourceBusy);
                                                continue;
                                            }

                                            next_bridge_session =
                                                next_bridge_session.wrapping_add(1);
                                            let bridge_session = next_bridge_session;
                                            send_reply(Reply {
                                                sequence,
                                                result: ReplyResult::BridgeOpen(Ok(bridge_session)),
                                            });

                                            let (restored_ftdi, outcome) = run_ftdi_bridge(
                                                &mut controller,
                                                ftdi,
                                                bridge_session,
                                                session_generation,
                                            )
                                            .await;
                                            ftdi = restored_ftdi;

                                            match outcome {
                                                BridgeRunOutcome::Closed { .. } => {
                                                    info!("raw FTDI serial bridge released");
                                                }
                                                BridgeRunOutcome::Disconnected => break,
                                                BridgeRunOutcome::Failed => {
                                                    warn!("raw FTDI serial bridge transfer failed");
                                                    wait_for_removal(&mut controller).await;
                                                    break;
                                                }
                                            }
                                        }
                                        Either::Second(command) => {
                                            reject_command(command, Error::NotReady);
                                        }
                                    }
                                }
                            }
                        } else if set_error_phase_if_current(
                            session_generation,
                            Phase::Enumerating,
                            Phase::FtdiError,
                        )
                        .await
                        {
                            warn!("PIO USB FTDI UART initialization failed");
                            wait_for_removal(&mut controller).await;
                        }
                    }
                    Err(_) => {
                        if set_error_phase_if_current(
                            session_generation,
                            Phase::Enumerating,
                            Phase::FtdiError,
                        )
                        .await
                        {
                            warn!("PIO USB FTDI discovery or pipe allocation failed");
                            wait_for_removal(&mut controller).await;
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
                        let mut cdc_rx_buffer = CdcRxBuffer::empty();
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
                                            operation: Operation::BridgeOpen,
                                            ..
                                        },
                                    ) => {
                                        if !command_is_current(&command).await {
                                            reject_command(command, Error::NotReady);
                                            continue;
                                        }
                                        if !set_bridge_connected_if_current(
                                            session_generation,
                                            Phase::CdcReady,
                                            false,
                                            true,
                                        )
                                        .await
                                        {
                                            reject_command(command, Error::ResourceBusy);
                                            continue;
                                        }

                                        next_bridge_session = next_bridge_session.wrapping_add(1);
                                        let bridge_session = next_bridge_session;
                                        send_reply(Reply {
                                            sequence,
                                            result: ReplyResult::BridgeOpen(Ok(bridge_session)),
                                        });

                                        let (restored_cdc, outcome) = run_bridge(
                                            &mut controller,
                                            cdc,
                                            &mut cdc_rx_buffer,
                                            bridge_session,
                                            session_generation,
                                        )
                                        .await;
                                        cdc = restored_cdc;

                                        match outcome {
                                            BridgeRunOutcome::Closed { .. } => {
                                                info!("raw USB serial bridge released");
                                            }
                                            BridgeRunOutcome::Disconnected => break,
                                            BridgeRunOutcome::Failed => {
                                                warn!("raw USB serial bridge transfer failed");
                                                wait_for_removal(&mut controller).await;
                                                break;
                                            }
                                        }
                                    }
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
                                            managed_cdc_read(
                                                &mut cdc,
                                                &mut cdc_rx_buffer,
                                                &mut data.bytes[..usize::from(length)],
                                            ),
                                        )
                                        .await
                                        {
                                            Ok(Ok(read)) => {
                                                data.len = read.copied as u8;
                                                if read.received == 0
                                                    || record_rx_if_current(
                                                        session_generation,
                                                        Phase::CdcReady,
                                                        read.received,
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
                                                        managed_cdc_read(
                                                            &mut cdc,
                                                            &mut cdc_rx_buffer,
                                                            &mut data.bytes
                                                                [..usize::from(read_length)],
                                                        ),
                                                    )
                                                    .await
                                                    {
                                                        Ok(Ok(read)) => {
                                                            data.len = read.copied as u8;
                                                            if read.copied == 0 {
                                                                Err(fail_current_session(
                                                                    session_generation,
                                                                    Phase::CdcReady,
                                                                    Phase::CdcError,
                                                                    Error::Transfer,
                                                                )
                                                                .await)
                                                            } else if read.received != 0
                                                                && !record_rx_if_current(
                                                                    session_generation,
                                                                    Phase::CdcReady,
                                                                    read.received,
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
                                                                        managed_cdc_read(
                                                                            &mut cdc,
                                                                            &mut cdc_rx_buffer,
                                                                            &mut data.bytes
                                                                                [start..end],
                                                                        ),
                                                                    )
                                                                    .await
                                                                    {
                                                                        Ok(Ok(read))
                                                                            if read.copied == 0 =>
                                                                        {
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
                                                                        Ok(Ok(read)) => {
                                                                            debug_assert!(
                                                                                read.copied
                                                                                    <= end - start
                                                                            );
                                                                            data.len +=
                                                                                read.copied as u8;
                                                                            if read.received == 0
                                                                                || record_rx_if_current(
                                                                                    session_generation,
                                                                                    Phase::CdcReady,
                                                                                    read.received,
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
                Err(CdcAcmCreateError::Configuration(
                    ConfigurationError::MissingControlInterface,
                )) => {
                    if set_phase_if_current(
                        session_generation,
                        Phase::Enumerating,
                        Phase::UnsupportedDevice,
                    )
                    .await
                    {
                        warn!("full-speed USB device has no CDC-ACM function");
                        wait_for_removal(&mut controller).await;
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
                        warn!("PIO USB CDC-ACM discovery or pipe allocation failed");
                        wait_for_removal(&mut controller).await;
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

async fn tcp_to_bridge(reader: &mut TcpReader<'_>, session: u32) -> Result<(), ()> {
    loop {
        let mut data = CdcData::empty();
        let count = reader.read(&mut data.bytes).await.map_err(|_| ())?;
        if count == 0 {
            return Ok(());
        }
        data.len = count as u8;
        BRIDGE_TO_USB.send(BridgeFrame { session, data }).await;
    }
}

async fn bridge_to_tcp(writer: &mut TcpWriter<'_>, session: u32) -> Result<(), ()> {
    loop {
        let frame = USB_TO_BRIDGE.receive().await;
        if frame.session != session {
            continue;
        }

        let mut bytes = frame.data.as_bytes();
        while !bytes.is_empty() {
            let count = writer.write(bytes).await.map_err(|_| ())?;
            if count == 0 {
                return Err(());
            }
            bytes = &bytes[count..];
        }
    }
}

async fn bridge_closed(session: u32) {
    loop {
        let BridgeEvent::Closed {
            session: closed_session,
        } = BRIDGE_EVENT.wait().await;
        if closed_session == session {
            return;
        }
    }
}

async fn discard_closed_bridge_output(session: u32) {
    loop {
        let frame = USB_TO_BRIDGE.receive().await;
        if frame.session != session {
            continue;
        }
    }
}

#[embassy_executor::task]
pub(crate) async fn usb_serial_task(stack: Stack<'static>) {
    let mut rx_buffer = [0; BRIDGE_SOCKET_BUFFER_SIZE];
    let mut tx_buffer = [0; BRIDGE_SOCKET_BUFFER_SIZE];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

    loop {
        socket.set_timeout(None);
        socket.set_keep_alive(Some(Duration::from_secs(10)));
        socket.set_nagle_enabled(false);

        if socket.accept(crate::USB_SERIAL_PORT).await.is_ok() {
            BRIDGE_TO_USB.clear();
            USB_TO_BRIDGE.clear();
            BRIDGE_EVENT.reset();

            match bridge_open().await {
                Ok(session) => {
                    info!("raw USB serial TCP client connected");
                    {
                        let (mut reader, mut writer) = socket.split();
                        let end = select3(
                            tcp_to_bridge(&mut reader, session),
                            bridge_to_tcp(&mut writer, session),
                            bridge_closed(session),
                        )
                        .await;
                        let end_code = match end {
                            Either3::First(Ok(())) => 1,
                            Either3::First(Err(())) => 2,
                            Either3::Second(Ok(())) => 3,
                            Either3::Second(Err(())) => 4,
                            Either3::Third(()) => 5,
                        };
                        record_bridge_tcp_end_if_connected(end_code).await;
                    }
                    // Frames not yet started belong to the TCP peer that just
                    // closed. Dropping them here also bounds BridgeClose to
                    // at most the one bulk-OUT transaction already in flight.
                    BRIDGE_TO_USB.clear();
                    // Keep draining frames after the TCP peer has gone away.
                    // This lets the host-side bulk-IN loop finish forwarding
                    // an already accepted packet before it acknowledges the
                    // close, instead of cancelling that USB transaction.
                    let _ = select3(
                        bridge_close(session),
                        discard_closed_bridge_output(session),
                        bridge_closed(session),
                    )
                    .await;
                    info!("raw USB serial TCP client disconnected");
                }
                Err(_) => {
                    warn!("raw USB serial TCP client rejected: CDC-ACM unavailable");
                }
            }
        }

        BRIDGE_TO_USB.clear();
        USB_TO_BRIDGE.clear();
        socket.abort();
        let _ = socket.flush().await;
        Timer::after(Duration::from_millis(20)).await;
    }
}
