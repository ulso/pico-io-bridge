use core::fmt::Write;

use defmt::*;
use embassy_futures::select::{Either, select};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIN_8, PIN_14, PIN_15, PIN_19, SPI1};
use embassy_rp::spi::{Config as SpiConfig, Spi};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::pubsub::PubSubChannel;
use embassy_time::{Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use heapless::String;
use mcp25xx::bitrates::clock_16mhz::{
    CNF_100K_BPS, CNF_125K_BPS, CNF_200K_BPS, CNF_250K_BPS, CNF_500K_BPS, CNF_1000K_BPS,
};
use mcp25xx::embedded_can::{ExtendedId, Frame, Id, StandardId};
use mcp25xx::registers::{OperationMode, RXB0CTRL, RXB1CTRL, RXM};
use mcp25xx::{CanFrame, Config as McpConfig, MCP25xx};

const CAN_DEFAULT_BITRATE: u32 = 500_000;
const CAN_SPI_FREQUENCY: u32 = 1_000_000;
const CAN_EVENT_SUBSCRIBERS: usize = crate::HTTP_SOCKETS;

static CAN_STATE: Mutex<CriticalSectionRawMutex, CanState> = Mutex::new(CanState::stopped());
static CAN_COMMANDS: Channel<CriticalSectionRawMutex, CanCommand, 8> = Channel::new();
static CAN_REPLIES: Channel<CriticalSectionRawMutex, CanReply, 8> = Channel::new();
static CAN_CMD_LOCK: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());
pub(crate) static CAN_EVENTS: PubSubChannel<
    CriticalSectionRawMutex,
    CanEvent,
    16,
    CAN_EVENT_SUBSCRIBERS,
    1,
> = PubSubChannel::new();

#[derive(Clone, Copy)]
struct CanState {
    ready: bool,
    state: CanBusState,
    bitrate: u32,
    mode: CanMode,
    tx_err: u8,
    rx_err: u8,
    tx_count: u32,
    rx_count: u32,
}

impl CanState {
    const fn stopped() -> Self {
        Self {
            ready: false,
            state: CanBusState::Stopped,
            bitrate: CAN_DEFAULT_BITRATE,
            mode: CanMode::Normal,
            tx_err: 0,
            rx_err: 0,
            tx_count: 0,
            rx_count: 0,
        }
    }
}

#[derive(Clone, Copy)]
enum CanBusState {
    Stopped,
    Error,
    Active,
}

#[derive(Clone, Copy)]
enum CanMode {
    Normal,
    ListenOnly,
    Loopback,
}

#[derive(Clone, Copy)]
pub(crate) struct CanTx {
    id: u32,
    ext: bool,
    rtr: bool,
    dlc: u8,
    data: [u8; 8],
}

#[derive(Clone, Copy)]
enum CanCommand {
    Status,
    ConfigGet,
    ConfigSet { bitrate: u32, mode: CanMode },
    Tx(CanTx),
}

#[derive(Clone, Copy)]
enum CanReply {
    Status(CanState),
    TxOk(CanTx),
    Error {
        code: &'static str,
        message: &'static str,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum CanEvent {
    Rx(CanTx),
}

fn can_mode_name(mode: CanMode) -> &'static str {
    match mode {
        CanMode::Normal => "normal",
        CanMode::ListenOnly => "listen-only",
        CanMode::Loopback => "loopback",
    }
}

fn can_state_name(state: CanBusState) -> &'static str {
    match state {
        CanBusState::Stopped => "stopped",
        CanBusState::Error => "error",
        CanBusState::Active => "active",
    }
}

fn can_operation_mode(mode: CanMode) -> OperationMode {
    match mode {
        CanMode::Normal => OperationMode::NormalOperation,
        CanMode::ListenOnly => OperationMode::ListenOnly,
        CanMode::Loopback => OperationMode::Loopback,
    }
}

fn can_cnf_for_bitrate(bitrate: u32) -> Option<mcp25xx::registers::CNF> {
    match bitrate {
        1_000_000 => Some(CNF_1000K_BPS),
        500_000 => Some(CNF_500K_BPS),
        250_000 => Some(CNF_250K_BPS),
        200_000 => Some(CNF_200K_BPS),
        125_000 => Some(CNF_125K_BPS),
        100_000 => Some(CNF_100K_BPS),
        _ => None,
    }
}

fn parse_bool_field(text: &str, key: &str) -> Option<bool> {
    let idx = text.find(key)?;
    let rest = &text[idx + key.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();

    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn parse_u32_field(text: &str, key: &str) -> Option<u32> {
    let idx = text.find(key)?;
    let rest = &text[idx + key.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    let mut end = 0;

    for byte in value.as_bytes() {
        if byte.is_ascii_digit() {
            end += 1;
        } else {
            break;
        }
    }

    if end == 0 {
        None
    } else {
        value[..end].parse().ok()
    }
}

fn parse_str_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let idx = text.find(key)?;
    let rest = &text[idx + key.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(&value[..end])
}

fn parse_data_field(text: &str, out: &mut [u8; 8]) -> Option<u8> {
    let idx = text.find("\"data\"")?;
    let rest = &text[idx..];
    let start = rest.find('[')?;
    let end = rest[start + 1..].find(']')? + start + 1;
    let mut count = 0;

    for part in rest[start + 1..end].split(',') {
        let value = part.trim();
        if value.is_empty() {
            continue;
        }
        if count == out.len() {
            return None;
        }
        let parsed = value.parse::<u8>().ok()?;
        out[count] = parsed;
        count += 1;
    }

    Some(count as u8)
}

fn parse_can_mode(text: &str) -> Option<CanMode> {
    match parse_str_field(text, "\"mode\"")? {
        "normal" => Some(CanMode::Normal),
        "listen-only" | "listen_only" => Some(CanMode::ListenOnly),
        "loopback" => Some(CanMode::Loopback),
        _ => None,
    }
}

fn parse_can_tx(text: &str) -> Option<CanTx> {
    let id = parse_u32_field(text, "\"id\"")?;
    let ext = parse_bool_field(text, "\"ext\"").unwrap_or(id > 0x7ff);
    let rtr = parse_bool_field(text, "\"rtr\"").unwrap_or(false);
    let mut data = [0; 8];
    let data_len = parse_data_field(text, &mut data).unwrap_or(0);
    let dlc = parse_u32_field(text, "\"dlc\"")
        .map(|v| v as u8)
        .unwrap_or(data_len);

    if dlc > 8 || (!rtr && data_len != dlc) {
        return None;
    }

    Some(CanTx {
        id,
        ext,
        rtr,
        dlc,
        data,
    })
}

fn can_tx_to_frame(tx: CanTx) -> Option<CanFrame> {
    let id = if tx.ext {
        Id::Extended(ExtendedId::new(tx.id)?)
    } else {
        Id::Standard(StandardId::new(tx.id as u16)?)
    };

    if tx.rtr {
        CanFrame::new_remote(id, tx.dlc as usize)
    } else {
        CanFrame::new(id, &tx.data[..tx.dlc as usize])
    }
}

fn frame_to_can_tx(frame: &CanFrame) -> CanTx {
    let (id, ext) = match frame.id() {
        Id::Standard(id) => (id.as_raw() as u32, false),
        Id::Extended(id) => (id.as_raw(), true),
    };
    let mut data = [0; 8];
    let frame_data = frame.data();
    data[..frame_data.len()].copy_from_slice(frame_data);

    CanTx {
        id,
        ext,
        rtr: frame.is_remote_frame(),
        dlc: frame.dlc() as u8,
        data,
    }
}

fn write_can_status_json(out: &mut String<256>, status: CanState) {
    let _ = core::write!(
        out,
        "{{\"type\":\"can.status\",\"ok\":true,\"bus\":0,\"ready\":{},\"state\":\"{}\",\"bitrate\":{},\"mode\":\"{}\",\"txErr\":{},\"rxErr\":{},\"txCount\":{},\"rxCount\":{},\"txQueueUsed\":0,\"txQueueFree\":8,\"rxQueueUsed\":0,\"rxQueueFree\":16}}",
        if status.ready { "true" } else { "false" },
        can_state_name(status.state),
        status.bitrate,
        can_mode_name(status.mode),
        status.tx_err,
        status.rx_err,
        status.tx_count,
        status.rx_count
    );
}

fn write_can_error_json(out: &mut String<256>, code: &str, message: &str) {
    let _ = core::write!(
        out,
        "{{\"type\":\"error\",\"ok\":false,\"code\":\"{}\",\"message\":\"{}\"}}",
        code,
        message
    );
}

pub(crate) fn write_can_frame_json(out: &mut String<256>, ty: &str, ok: bool, tx: CanTx) {
    let _ = core::write!(
        out,
        "{{\"type\":\"{}\",\"ok\":{},\"bus\":0,\"id\":{},\"ext\":{},\"rtr\":{},\"dlc\":{},\"data\":[",
        ty,
        if ok { "true" } else { "false" },
        tx.id,
        if tx.ext { "true" } else { "false" },
        if tx.rtr { "true" } else { "false" },
        tx.dlc
    );

    if !tx.rtr {
        for i in 0..tx.dlc as usize {
            if i > 0 {
                let _ = core::write!(out, ",");
            }
            let _ = core::write!(out, "{}", tx.data[i]);
        }
    }

    let _ = core::write!(out, "]}}");
}

async fn send_can_command(command: CanCommand) -> CanReply {
    let _lock = CAN_CMD_LOCK.lock().await;

    while CAN_REPLIES.try_receive().is_ok() {}

    CAN_COMMANDS.send(command).await;

    match select(CAN_REPLIES.receive(), Timer::after(Duration::from_secs(2))).await {
        Either::First(reply) => reply,
        Either::Second(()) => CanReply::Error {
            code: "can_timeout",
            message: "CAN controller did not answer",
        },
    }
}

pub(crate) async fn handle_can_ws_text(payload: &[u8], out: &mut String<256>) {
    let Ok(text) = core::str::from_utf8(payload) else {
        write_can_error_json(out, "invalid_json", "WebSocket payload must be UTF-8 JSON");
        return;
    };

    let command = if text.contains("can.status") {
        Some(CanCommand::Status)
    } else if text.contains("can.config.get") {
        Some(CanCommand::ConfigGet)
    } else if text.contains("can.config.set") {
        let bitrate = parse_u32_field(text, "\"bitrate\"").unwrap_or(CAN_DEFAULT_BITRATE);
        let Some(mode) = parse_can_mode(text).or(Some(CanMode::Normal)) else {
            write_can_error_json(out, "invalid_config", "Invalid CAN mode");
            return;
        };
        Some(CanCommand::ConfigSet { bitrate, mode })
    } else if text.contains("can.tx") || (text.contains("\"id\"") && text.contains("\"dlc\"")) {
        match parse_can_tx(text) {
            Some(tx) => Some(CanCommand::Tx(tx)),
            None => {
                write_can_error_json(out, "invalid_frame", "Invalid CAN frame");
                return;
            }
        }
    } else {
        None
    };

    let Some(command) = command else {
        write_can_error_json(
            out,
            "unsupported_type",
            "Supported messages: can.status, can.config.get, can.config.set, can.tx",
        );
        return;
    };

    match send_can_command(command).await {
        CanReply::Status(status) => write_can_status_json(out, status),
        CanReply::TxOk(tx) => write_can_frame_json(out, "can.tx", true, tx),
        CanReply::Error { code, message } => write_can_error_json(out, code, message),
    }
}

#[embassy_executor::task]
pub(crate) async fn can_task(
    spi_peri: embassy_rp::Peri<'static, SPI1>,
    sck: embassy_rp::Peri<'static, PIN_14>,
    mosi: embassy_rp::Peri<'static, PIN_15>,
    miso: embassy_rp::Peri<'static, PIN_8>,
    cs_pin: embassy_rp::Peri<'static, PIN_19>,
) {
    let mut spi_config = SpiConfig::default();
    spi_config.frequency = CAN_SPI_FREQUENCY;

    let spi = Spi::new_blocking(spi_peri, sck, mosi, miso, spi_config);
    let cs = Output::new(cs_pin, Level::High);
    let spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();
    let mut mcp = MCP25xx { spi: spi_device };
    let mut status = CanState::stopped();
    let can_events = CAN_EVENTS.publisher().unwrap();

    match apply_can_config(&mut mcp, CAN_DEFAULT_BITRATE, CanMode::Normal) {
        Ok(()) => {
            status.ready = true;
            status.state = CanBusState::Active;
            info!("MCP25xx CAN ready: 500 kbit/s, normal mode");
        }
        Err(()) => {
            status.ready = false;
            status.state = CanBusState::Error;
            warn!("MCP25xx CAN init failed");
        }
    }

    *CAN_STATE.lock().await = status;

    loop {
        while let Ok(command) = CAN_COMMANDS.try_receive() {
            let reply = handle_can_command(&mut mcp, &mut status, command);
            *CAN_STATE.lock().await = status;
            CAN_REPLIES.send(reply).await;
        }

        if status.ready {
            match mcp25xx::embedded_can::nb::Can::receive(&mut mcp) {
                Ok(frame) => {
                    status.rx_count = status.rx_count.wrapping_add(1);
                    *CAN_STATE.lock().await = status;
                    can_events.publish_immediate(CanEvent::Rx(frame_to_can_tx(&frame)));
                }
                Err(nb::Error::WouldBlock) => {}
                Err(_) => {
                    status.ready = false;
                    status.state = CanBusState::Error;
                    *CAN_STATE.lock().await = status;
                    warn!("MCP25xx CAN receive failed");
                }
            }
        }

        Timer::after(Duration::from_millis(2)).await;
    }
}

fn apply_can_config<SPI: embedded_hal_1::spi::SpiDevice>(
    mcp: &mut MCP25xx<SPI>,
    bitrate: u32,
    mode: CanMode,
) -> Result<(), ()> {
    let cnf = can_cnf_for_bitrate(bitrate).ok_or(())?;
    let config = McpConfig::default()
        .mode(can_operation_mode(mode))
        .bitrate(cnf)
        .receive_buffer_0(RXB0CTRL::default().with_rxm(RXM::ReceiveAny))
        .receive_buffer_1(RXB1CTRL::default().with_rxm(RXM::ReceiveAny));

    mcp.apply_config(&config).map_err(|_| ())
}

fn handle_can_command<SPI: embedded_hal_1::spi::SpiDevice>(
    mcp: &mut MCP25xx<SPI>,
    status: &mut CanState,
    command: CanCommand,
) -> CanReply {
    match command {
        CanCommand::Status | CanCommand::ConfigGet => CanReply::Status(*status),
        CanCommand::ConfigSet { bitrate, mode } => {
            if can_cnf_for_bitrate(bitrate).is_none() {
                return CanReply::Error {
                    code: "unsupported_bitrate",
                    message: "Supported bitrates: 100000, 125000, 200000, 250000, 500000, 1000000",
                };
            }

            match apply_can_config(mcp, bitrate, mode) {
                Ok(()) => {
                    status.ready = true;
                    status.state = CanBusState::Active;
                    status.bitrate = bitrate;
                    status.mode = mode;
                    CanReply::Status(*status)
                }
                Err(()) => {
                    status.ready = false;
                    status.state = CanBusState::Error;
                    CanReply::Error {
                        code: "can_config_failed",
                        message: "Failed to configure MCP25xx",
                    }
                }
            }
        }
        CanCommand::Tx(tx) => {
            if !status.ready {
                return CanReply::Error {
                    code: "can_not_ready",
                    message: "CAN controller is not ready",
                };
            }

            let Some(frame) = can_tx_to_frame(tx) else {
                return CanReply::Error {
                    code: "invalid_frame",
                    message: "Invalid CAN frame",
                };
            };

            match mcp25xx::embedded_can::nb::Can::transmit(mcp, &frame) {
                Ok(None) => {
                    status.tx_count = status.tx_count.wrapping_add(1);
                    CanReply::TxOk(tx)
                }
                Ok(Some(_)) | Err(nb::Error::WouldBlock) => CanReply::Error {
                    code: "tx_busy",
                    message: "No MCP25xx TX buffer is available",
                },
                Err(_) => {
                    status.ready = false;
                    status.state = CanBusState::Error;
                    CanReply::Error {
                        code: "can_tx_failed",
                        message: "Failed to transmit CAN frame",
                    }
                }
            }
        }
    }
}
