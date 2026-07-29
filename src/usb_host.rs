//! PIO USB root-port manager for the Feather RP2040 USB Host profile.
//!
//! This task is the sole owner of enumeration, device addresses, and CDC-ACM
//! class pipes. SCPI exchanges owned fixed-size messages with it; GP13 remains
//! exclusively owned by the board's existing network `StatusIndicator`.

use defmt::{info, warn};
use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_rp::Peri;
use embassy_rp::bind_interrupts;
use embassy_rp::dma::InterruptHandler as DmaInterruptHandler;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIN_16, PIN_17, PIN_18, PIO0, PIO1};
use embassy_rp::pio::InterruptHandler as PioInterruptHandler;
use embassy_rp_pio_usb_host::cdc_acm::{CdcAcmError, allocate_from_enumeration};
use embassy_rp_pio_usb_host::host::{DeviceEvent, PipeError, Speed};
use embassy_rp_pio_usb_host::pio_host::PioHostState;
use embassy_rp_pio_usb_host::pio_host::rp2040::Rp2040PioEngine;
use embassy_rp_pio_usb_host::usb::CdcLineCoding;
use embassy_rp_pio_usb_host::{AttachDetector, BusEvent, DeviceSpeed};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Ticker, Timer, with_timeout};
use embassy_usb_host::{BusRoute, BusState};

const ATTACH_DEBOUNCE_SAMPLES: u16 = 100;
const CONFIG_DESCRIPTOR_CAPACITY: usize = 512;
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
    UnsupportedSpeed,
    EnumerationError,
    CdcError,
}

impl Phase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PowerOff => "POWER_OFF",
            Self::Waiting => "WAITING",
            Self::Resetting => "RESETTING",
            Self::Enumerating => "ENUMERATING",
            Self::CdcReady => "CDC_READY",
            Self::UnsupportedSpeed => "UNSUPPORTED_SPEED",
            Self::EnumerationError => "ENUMERATION_ERROR",
            Self::CdcError => "CDC_ERROR",
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
        }
    }

    fn clear_device(&mut self, phase: Phase) {
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
    NotReady,
    Timeout,
    Transfer,
}

#[derive(Clone, Copy)]
enum Operation {
    Read { length: u8 },
    Write(CdcData),
    Exchange { write: CdcData, read_length: u8 },
}

#[derive(Clone, Copy)]
struct Command {
    sequence: u32,
    operation: Operation,
}

#[derive(Clone, Copy)]
enum ReplyResult {
    Read(Result<CdcData, Error>),
    Write(Result<u8, Error>),
    Exchange(Result<CdcData, Error>),
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
    if status().await.phase != Phase::CdcReady {
        return Err(Error::NotReady);
    }
    let reply_timeout = match operation {
        Operation::Exchange { .. } => EXCHANGE_COMMAND_TIMEOUT,
        Operation::Read { .. } | Operation::Write(_) => COMMAND_TIMEOUT,
    };

    let _guard = HOST_COMMAND_LOCK.lock().await;
    let sequence = {
        let mut next = HOST_COMMAND_SEQUENCE.lock().await;
        *next = next.wrapping_add(1);
        *next
    };
    while HOST_REPLIES.try_receive().is_ok() {}
    HOST_COMMANDS
        .send(Command {
            sequence,
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
        ReplyResult::Read(_) | ReplyResult::Exchange(_) => Err(Error::Transfer),
    }
}

pub(crate) async fn cdc_read(length: u8) -> Result<CdcData, Error> {
    if length == 0 || usize::from(length) > CDC_MAX_TRANSFER {
        return Err(Error::InvalidLength);
    }

    match send_command(Operation::Read { length }).await? {
        ReplyResult::Read(result) => result,
        ReplyResult::Write(_) | ReplyResult::Exchange(_) => Err(Error::Transfer),
    }
}

pub(crate) async fn cdc_exchange_hex(value: &str, read_length: u8) -> Result<CdcData, Error> {
    if read_length == 0 || usize::from(read_length) > CDC_MAX_TRANSFER {
        return Err(Error::InvalidLength);
    }

    let write = parse_hex(value)?;
    match send_command(Operation::Exchange { write, read_length }).await? {
        ReplyResult::Exchange(result) => result,
        ReplyResult::Read(_) | ReplyResult::Write(_) => Err(Error::Transfer),
    }
}

async fn set_waiting() {
    HOST_STATE.lock().await.clear_device(Phase::Waiting);
}

async fn set_speed_phase(speed: HostSpeed, phase: Phase) {
    let mut state = HOST_STATE.lock().await;
    state.phase = phase;
    state.speed = Some(speed);
    state.address = 0;
    state.vendor_id = 0;
    state.product_id = 0;
}

async fn set_error_phase(phase: Phase) {
    let mut state = HOST_STATE.lock().await;
    state.phase = phase;
    state.error_count = state.error_count.wrapping_add(1);
}

async fn record_error() {
    let mut state = HOST_STATE.lock().await;
    state.error_count = state.error_count.wrapping_add(1);
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
    };
    send_reply(Reply {
        sequence: command.sequence,
        result,
    });
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
                BusEvent::Attached(DeviceSpeed::Full) => {
                    set_speed_phase(HostSpeed::Full, Phase::Resetting).await;
                    info!("full-speed PIO USB root device attached");
                    match host_state.reset_and_report_connected(Speed::Full).await {
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
                BusEvent::Attached(DeviceSpeed::Low) => {
                    set_speed_phase(HostSpeed::Low, Phase::UnsupportedSpeed).await;
                    warn!("low-speed root device cannot expose CDC-ACM bulk endpoints");
                    if connected && host_state.report_disconnected_if_not_resetting() {
                        connected = false;
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

            set_speed_phase(HostSpeed::Full, Phase::Enumerating).await;
            let mut configuration = [0_u8; CONFIG_DESCRIPTOR_CAPACITY];
            let (enumeration, configuration_len) = match bus_handle
                .enumerate(BusRoute::Direct(speed), &mut configuration)
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    set_error_phase(Phase::EnumerationError).await;
                    warn!("PIO USB device enumeration failed");

                    loop {
                        match select(controller.wait_for_device_event(), HOST_COMMANDS.receive())
                            .await
                        {
                            Either::First(DeviceEvent::Disconnected | DeviceEvent::Overcurrent) => {
                                break;
                            }
                            Either::First(_) => {}
                            Either::Second(command) => {
                                reject_command(command, Error::NotReady);
                            }
                        }
                    }

                    // embassy-usb-host 0.1.0 can retain an address lease on an
                    // enumeration error. This root-only backend has no hubs,
                    // so releasing the complete address space after physical
                    // detach is safe.
                    for address in 1_u8..=127 {
                        bus_handle.free_address(address);
                    }
                    set_waiting().await;
                    continue;
                }
            };

            let address = enumeration.device_address;
            {
                let mut state = HOST_STATE.lock().await;
                state.address = address;
                state.vendor_id = enumeration.device_desc.vendor_id;
                state.product_id = enumeration.device_desc.product_id;
            }

            match allocate_from_enumeration(
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
                        HOST_STATE.lock().await.phase = Phase::CdcReady;
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
                                Either::Second(Command {
                                    sequence,
                                    operation: Operation::Read { length },
                                }) => {
                                    let mut data = CdcData::empty();
                                    let result = match with_timeout(
                                        TRANSFER_TIMEOUT,
                                        cdc.read(&mut data.bytes[..usize::from(length)]),
                                    )
                                    .await
                                    {
                                        Ok(Ok(count)) => {
                                            data.len = count as u8;
                                            let mut state = HOST_STATE.lock().await;
                                            state.rx_bytes =
                                                state.rx_bytes.wrapping_add(count as u32);
                                            Ok(data)
                                        }
                                        Ok(Err(CdcAcmError::Transfer(PipeError::Disconnected))) => {
                                            Err(Error::NotReady)
                                        }
                                        Ok(Err(_)) => {
                                            set_error_phase(Phase::CdcError).await;
                                            Err(Error::Transfer)
                                        }
                                        Err(_) => {
                                            record_error().await;
                                            Err(Error::Timeout)
                                        }
                                    };
                                    send_reply(Reply {
                                        sequence,
                                        result: ReplyResult::Read(result),
                                    });
                                }
                                Either::Second(Command {
                                    sequence,
                                    operation: Operation::Write(data),
                                }) => {
                                    let result = match with_timeout(
                                        TRANSFER_TIMEOUT,
                                        cdc.write(data.as_bytes()),
                                    )
                                    .await
                                    {
                                        Ok(Ok(count)) => {
                                            let mut state = HOST_STATE.lock().await;
                                            state.tx_bytes =
                                                state.tx_bytes.wrapping_add(count as u32);
                                            Ok(count as u8)
                                        }
                                        Ok(Err(CdcAcmError::Transfer(PipeError::Disconnected))) => {
                                            Err(Error::NotReady)
                                        }
                                        Ok(Err(_)) => {
                                            set_error_phase(Phase::CdcError).await;
                                            Err(Error::Transfer)
                                        }
                                        Err(_) => {
                                            record_error().await;
                                            Err(Error::Timeout)
                                        }
                                    };
                                    send_reply(Reply {
                                        sequence,
                                        result: ReplyResult::Write(result),
                                    });
                                }
                                Either::Second(Command {
                                    sequence,
                                    operation: Operation::Exchange { write, read_length },
                                }) => {
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
                                            let mut state = HOST_STATE.lock().await;
                                            state.tx_bytes =
                                                state.tx_bytes.wrapping_add(count as u32);
                                            drop(state);

                                            let mut data = CdcData::empty();
                                            match with_timeout(
                                                EXCHANGE_FIRST_RESPONSE_TIMEOUT,
                                                cdc.read(
                                                    &mut data.bytes[..usize::from(read_length)],
                                                ),
                                            )
                                            .await
                                            {
                                                Ok(Ok(0)) => {
                                                    set_error_phase(Phase::CdcError).await;
                                                    Err(Error::Transfer)
                                                }
                                                Ok(Ok(count)) => {
                                                    data.len = count as u8;
                                                    let mut state = HOST_STATE.lock().await;
                                                    state.rx_bytes =
                                                        state.rx_bytes.wrapping_add(count as u32);
                                                    drop(state);

                                                    let mut exchange_result = Ok(data);
                                                    while usize::from(data.len)
                                                        < usize::from(read_length)
                                                    {
                                                        let start = usize::from(data.len);
                                                        let end = usize::from(read_length);
                                                        match with_timeout(
                                                            EXCHANGE_IDLE_TIMEOUT,
                                                            cdc.read(&mut data.bytes[start..end]),
                                                        )
                                                        .await
                                                        {
                                                            Ok(Ok(0)) => {
                                                                set_error_phase(Phase::CdcError)
                                                                    .await;
                                                                exchange_result =
                                                                    Err(Error::Transfer);
                                                                break;
                                                            }
                                                            Ok(Ok(count)) => {
                                                                debug_assert!(count <= end - start);
                                                                data.len += count as u8;
                                                                let mut state =
                                                                    HOST_STATE.lock().await;
                                                                state.rx_bytes = state
                                                                    .rx_bytes
                                                                    .wrapping_add(count as u32);
                                                                exchange_result = Ok(data);
                                                            }
                                                            Ok(Err(CdcAcmError::Transfer(
                                                                PipeError::Disconnected,
                                                            ))) => {
                                                                exchange_result =
                                                                    Err(Error::NotReady);
                                                                break;
                                                            }
                                                            Ok(Err(_)) => {
                                                                set_error_phase(Phase::CdcError)
                                                                    .await;
                                                                exchange_result =
                                                                    Err(Error::Transfer);
                                                                break;
                                                            }
                                                            Err(_) => break,
                                                        }
                                                    }
                                                    exchange_result
                                                }
                                                Ok(Err(CdcAcmError::Transfer(
                                                    PipeError::Disconnected,
                                                ))) => Err(Error::NotReady),
                                                Ok(Err(_)) => {
                                                    set_error_phase(Phase::CdcError).await;
                                                    Err(Error::Transfer)
                                                }
                                                Err(_) => {
                                                    record_error().await;
                                                    Err(Error::Timeout)
                                                }
                                            }
                                        }
                                        Ok(Err(CdcAcmError::Transfer(PipeError::Disconnected))) => {
                                            Err(Error::NotReady)
                                        }
                                        Ok(Err(_)) => {
                                            set_error_phase(Phase::CdcError).await;
                                            Err(Error::Transfer)
                                        }
                                        Err(_) => {
                                            record_error().await;
                                            Err(Error::Timeout)
                                        }
                                    };
                                    send_reply(Reply {
                                        sequence,
                                        result: ReplyResult::Exchange(result),
                                    });
                                }
                            }
                        }
                    } else {
                        set_error_phase(Phase::CdcError).await;
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
                Err(_) => {
                    set_error_phase(Phase::CdcError).await;
                    warn!("PIO USB device has no usable CDC-ACM function");
                    loop {
                        match select(controller.wait_for_device_event(), HOST_COMMANDS.receive())
                            .await
                        {
                            Either::First(DeviceEvent::Disconnected | DeviceEvent::Overcurrent) => {
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

            bus_handle.free_address(address);
            set_waiting().await;
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
