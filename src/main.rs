//! USB CDC-NCM network interface.

#![no_std]
#![no_main]

use core::fmt::Write;
use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config as NetConfig, Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4,
};
use embassy_rp::bind_interrupts;
use embassy_rp::flash::Flash;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIN_8, PIN_14, PIN_15, PIN_19, SPI1, USB};
use embassy_rp::spi::{Config as SpiConfig, Spi};
use embassy_rp::uart::{Config as UartConfig, UartTx};
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use embassy_usb::class::cdc_ncm;
use embassy_usb::class::cdc_ncm::embassy_net::{Device as NcmDevice, State as NcmNetState};
use embassy_usb::{Builder, Config as UsbConfig, UsbDevice};
#[cfg(feature = "mdns")]
use embedded_alloc::LlffHeap as Heap;
use embedded_hal_bus::spi::ExclusiveDevice;
use heapless::String;
use mcp25xx::bitrates::clock_16mhz::{
    CNF_100K_BPS, CNF_125K_BPS, CNF_200K_BPS, CNF_250K_BPS, CNF_500K_BPS, CNF_1000K_BPS,
};
use mcp25xx::embedded_can::{ExtendedId, Frame, Id, StandardId};
use mcp25xx::registers::{OperationMode, RXB0CTRL, RXB1CTRL, RXM};
use mcp25xx::{CanFrame, Config as McpConfig, MCP25xx};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

#[cfg(feature = "mdns")]
use core::{convert::Infallible, net::Ipv4Addr};
#[cfg(feature = "mdns")]
use embassy_net::IpAddress;
#[cfg(feature = "mdns")]
use embassy_net::udp::{PacketMetadata, UdpSocket};
#[cfg(feature = "mdns")]
use hick_embassy::MdnsState;
#[cfg(feature = "mdns")]
use mdns_proto::{EndpointConfig, Name, ServiceRecords, ServiceSpec};
#[cfg(feature = "mdns")]
use rand_core::TryRng;

const MTU: usize = 1514;
const USB_MAX_PACKET_SIZE: u16 = 64;
const FLASH_SIZE: usize = 2 * 1024 * 1024;
const HTTP_PORT: u16 = 80;
const CDC_NCM_LINK_UP_TIMEOUT: Duration = Duration::from_secs(6);
const CDC_NCM_LINK_UP_RESET_MISSES: u8 = 2;
#[cfg(feature = "mdns")]
const HEAP_SIZE: usize = 32768;

#[cfg(feature = "mdns")]
#[global_allocator]
static HEAP: Heap = Heap::empty();

const DEVICE_IPV4_PREFIX_LEN: u8 = 16;
const DEVICE_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const HOST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
const CAN_DEFAULT_BITRATE: u32 = 500_000;
const CAN_SPI_FREQUENCY: u32 = 1_000_000;

static CAN_STATE: Mutex<CriticalSectionRawMutex, CanState> = Mutex::new(CanState::stopped());
static CAN_COMMANDS: Channel<CriticalSectionRawMutex, CanCommand, 8> = Channel::new();
static CAN_REPLIES: Channel<CriticalSectionRawMutex, CanReply, 8> = Channel::new();
static CAN_EVENTS: Channel<CriticalSectionRawMutex, CanEvent, 16> = Channel::new();

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
struct CanTx {
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
enum CanEvent {
    Rx(CanTx),
}

#[cfg(feature = "mdns")]
struct MdnsRng(u64);

#[cfg(feature = "mdns")]
impl MdnsRng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[cfg(feature = "mdns")]
impl TryRng for MdnsRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(self.next() as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(self.next())
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        for chunk in dst.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }

        Ok(())
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;

    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    hash
}

fn link_local_from_seed(seed: &[u8]) -> [u8; 4] {
    let hash = fnv1a64(seed);
    let host = (hash % (254 * 256)) as u16;

    [169, 254, 1 + (host / 256) as u8, (host & 0xff) as u8]
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0 = 0x6745_2301u32;
    let mut h1 = 0xefcd_ab89u32;
    let mut h2 = 0x98ba_dcfeu32;
    let mut h3 = 0x1032_5476u32;
    let mut h4 = 0xc3d2_e1f0u32;
    let bit_len = (data.len() as u64) * 8;
    let mut offset = 0;

    while offset + 64 <= data.len() {
        sha1_block(
            &data[offset..offset + 64],
            &mut h0,
            &mut h1,
            &mut h2,
            &mut h3,
            &mut h4,
        );
        offset += 64;
    }

    let mut block = [0u8; 128];
    let rem = &data[offset..];
    block[..rem.len()].copy_from_slice(rem);
    block[rem.len()] = 0x80;
    let total = if rem.len() + 1 + 8 <= 64 { 64 } else { 128 };
    block[total - 8..total].copy_from_slice(&bit_len.to_be_bytes());

    sha1_block(&block[..64], &mut h0, &mut h1, &mut h2, &mut h3, &mut h4);
    if total == 128 {
        sha1_block(&block[64..128], &mut h0, &mut h1, &mut h2, &mut h3, &mut h4);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn sha1_block(block: &[u8], h0: &mut u32, h1: &mut u32, h2: &mut u32, h3: &mut u32, h4: &mut u32) {
    let mut w = [0u32; 80];

    for i in 0..16 {
        let j = i * 4;
        w[i] = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }

    let mut a = *h0;
    let mut b = *h1;
    let mut c = *h2;
    let mut d = *h3;
    let mut e = *h4;

    for (i, word) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
            20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }

    *h0 = h0.wrapping_add(a);
    *h1 = h1.wrapping_add(b);
    *h2 = h2.wrapping_add(c);
    *h3 = h3.wrapping_add(d);
    *h4 = h4.wrapping_add(e);
}

fn base64_20(input: &[u8; 20], out: &mut [u8; 28]) {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut i = 0;
    let mut j = 0;

    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | input[i + 2] as u32;
        out[j] = B64[((n >> 18) & 0x3f) as usize];
        out[j + 1] = B64[((n >> 12) & 0x3f) as usize];
        out[j + 2] = B64[((n >> 6) & 0x3f) as usize];
        out[j + 3] = B64[(n & 0x3f) as usize];
        i += 3;
        j += 4;
    }

    let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
    out[j] = B64[((n >> 18) & 0x3f) as usize];
    out[j + 1] = B64[((n >> 12) & 0x3f) as usize];
    out[j + 2] = B64[((n >> 6) & 0x3f) as usize];
    out[j + 3] = b'=';
}

fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    for line in request.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case(name) {
                return Some(value.trim());
            }
        }
    }

    None
}

fn websocket_accept_key(key: &str, out: &mut [u8; 28]) {
    const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut input = [0u8; 96];
    let key_bytes = key.as_bytes();
    let key_len = key_bytes.len().min(input.len() - GUID.len());

    input[..key_len].copy_from_slice(&key_bytes[..key_len]);
    input[key_len..key_len + GUID.len()].copy_from_slice(GUID);

    let digest = sha1(&input[..key_len + GUID.len()]);
    base64_20(&digest, out);
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

fn write_can_frame_json(out: &mut String<256>, ty: &str, ok: bool, tx: CanTx) {
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
    CAN_COMMANDS.send(command).await;

    match select(CAN_REPLIES.receive(), Timer::after(Duration::from_secs(2))).await {
        Either::First(reply) => reply,
        Either::Second(()) => CanReply::Error {
            code: "can_timeout",
            message: "CAN controller did not answer",
        },
    }
}

async fn handle_can_ws_text(payload: &[u8], out: &mut String<256>) {
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

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

#[embassy_executor::task]
async fn usb_task(mut usb: UsbDevice<'static, Driver<'static, USB>>) {
    usb.run().await;
}

#[embassy_executor::task]
async fn ncm_task(runner: cdc_ncm::embassy_net::Runner<'static, Driver<'static, USB>, MTU>) {
    runner.run().await;
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, NcmDevice<'static, MTU>>) {
    runner.run().await;
}

#[embassy_executor::task]
async fn can_task(
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
                    let _ = CAN_EVENTS.try_send(CanEvent::Rx(frame_to_can_tx(&frame)));
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

async fn write_all(
    socket: &mut TcpSocket<'_>,
    mut data: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        data = &data[written..];
    }

    Ok(())
}

async fn serve_http_connection(
    socket: &mut TcpSocket<'_>,
    rx_buf: &mut [u8],
) -> Result<(), embassy_net::tcp::Error> {
    let mut len = 0;

    loop {
        let n = socket.read(&mut rx_buf[len..]).await?;
        if n == 0 {
            return Ok(());
        }
        len += n;

        if rx_buf[..len].windows(4).any(|w| w == b"\r\n\r\n") || len == rx_buf.len() {
            break;
        }
    }

    let Ok(request) = core::str::from_utf8(&rx_buf[..len]) else {
        write_all(
            socket,
            b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .await?;
        return Ok(());
    };

    if request.starts_with("GET /can ") || request.starts_with("GET /ws ") {
        if let Some(key) = header_value(request, "Sec-WebSocket-Key") {
            let mut accept = [0u8; 28];
            websocket_accept_key(key, &mut accept);

            write_all(socket, b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ").await?;
            write_all(socket, &accept).await?;
            write_all(socket, b"\r\n\r\n").await?;
            websocket_loop(socket, rx_buf).await?;
        } else {
            write_all(
                socket,
                b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            )
            .await?;
        }
    } else if request.starts_with("GET /api/status ") {
        const BODY: &[u8] =
            br#"{"device":"pico-can-bridge-rs","network":"cdc-ncm","websocket":"/can"}"#;
        write_all(socket, b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 70\r\n\r\n").await?;
        write_all(socket, BODY).await?;
    } else {
        const BODY: &[u8] = b"pico-can-bridge-rs\n\n/api/status\n/can\n/ws\n";
        write_all(socket, b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: 41\r\n\r\n").await?;
        write_all(socket, BODY).await?;
    }

    Ok(())
}

async fn websocket_loop(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<(), embassy_net::tcp::Error> {
    const READY: &[u8] = b"{\"type\":\"hello\",\"ok\":true,\"endpoint\":\"/can\"}";
    let mut response = String::<256>::new();

    websocket_send_text(socket, READY).await?;

    loop {
        let n = match select(socket.read(buf), CAN_EVENTS.receive()).await {
            Either::First(result) => result?,
            Either::Second(CanEvent::Rx(frame)) => {
                response.clear();
                write_can_frame_json(&mut response, "can.rx", true, frame);
                websocket_send_text(socket, response.as_bytes()).await?;
                continue;
            }
        };

        if n < 2 {
            return Ok(());
        }

        let opcode = buf[0] & 0x0f;
        let masked = (buf[1] & 0x80) != 0;
        let payload_len = (buf[1] & 0x7f) as usize;
        if payload_len > 125 || n < 2 + if masked { 4 } else { 0 } + payload_len {
            return Ok(());
        }

        let payload_start = 2 + if masked { 4 } else { 0 };
        let payload_end = payload_start + payload_len;

        if masked {
            let mask = [buf[2], buf[3], buf[4], buf[5]];
            for i in 0..payload_len {
                buf[payload_start + i] ^= mask[i % 4];
            }
        }

        match opcode {
            0x8 => {
                socket.write(&[0x88, 0x00]).await?;
                return Ok(());
            }
            0x9 => {
                socket.write(&[0x8a, payload_len as u8]).await?;
                if payload_len > 0 {
                    let payload = &buf[payload_start..payload_end];
                    write_all(socket, payload).await?;
                }
            }
            0x1 => {
                response.clear();
                handle_can_ws_text(&buf[payload_start..payload_end], &mut response).await;
                websocket_send_text(socket, response.as_bytes()).await?;
            }
            0x2 => {
                const RESPONSE: &[u8] = b"{\"type\":\"error\",\"ok\":false,\"code\":\"unsupported_type\",\"message\":\"binary CAN messages are not supported yet\"}";
                websocket_send_text(socket, RESPONSE).await?;
            }
            _ => {}
        }
    }
}

async fn websocket_send_text(
    socket: &mut TcpSocket<'_>,
    payload: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    if payload.len() < 126 {
        socket.write(&[0x81, payload.len() as u8]).await?;
    } else {
        let len = payload.len() as u16;
        socket
            .write(&[0x81, 126, (len >> 8) as u8, len as u8])
            .await?;
    }

    write_all(socket, payload).await
}

#[embassy_executor::task]
async fn http_task(stack: Stack<'static>) {
    static RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static REQUEST_BUF: StaticCell<[u8; 1024]> = StaticCell::new();

    let rx_buf = RX_BUF.init([0; 2048]);
    let tx_buf = TX_BUF.init([0; 2048]);
    let request_buf = REQUEST_BUF.init([0; 1024]);
    let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);

    loop {
        socket.set_timeout(Some(Duration::from_secs(10)));
        socket.set_nagle_enabled(false);

        if socket.accept(HTTP_PORT).await.is_ok() {
            info!("HTTP client connected");
            match serve_http_connection(&mut socket, request_buf).await {
                Ok(()) => {
                    socket.close();
                    let _ = socket.flush().await;
                }
                Err(_) => {
                    socket.abort();
                    let _ = socket.flush().await;
                }
            }
        } else {
            socket.abort();
            let _ = socket.flush().await;
        }

        Timer::after(Duration::from_millis(50)).await;
    }
}

#[cfg(feature = "mdns")]
#[embassy_executor::task]
async fn mdns_task(stack: Stack<'static>, state: &'static MdnsState<MdnsRng>) {
    static RX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static SCRATCH: StaticCell<[u8; 2048]> = StaticCell::new();

    let rx_meta = RX_META.init([PacketMetadata::EMPTY; 4]);
    let rx_buf = RX_BUF.init([0; 2048]);
    let tx_meta = TX_META.init([PacketMetadata::EMPTY; 4]);
    let tx_buf = TX_BUF.init([0; 2048]);
    let scratch = SCRATCH.init([0; 2048]);

    stack
        .join_multicast_group(IpAddress::v4(224, 0, 0, 251))
        .unwrap();

    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    socket.bind(5353).unwrap();

    info!("mDNS responder ready: pico-can-bridge.local");
    state.run(Some(&mut socket), None, scratch).await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    #[cfg(feature = "mdns")]
    {
        static HEAP_MEM: StaticCell<[u8; HEAP_SIZE]> = StaticCell::new();
        let heap_mem = HEAP_MEM.init([0; HEAP_SIZE]);
        unsafe {
            HEAP.init(heap_mem.as_ptr() as usize, HEAP_SIZE);
        }
    }

    let mut uart = UartTx::new_blocking(p.UART0, p.PIN_0, UartConfig::default());
    uart.blocking_write(b"pico-can-bridge-rs boot\r\n").unwrap();

    let mut flash = Flash::<_, _, FLASH_SIZE>::new_blocking(p.FLASH);
    let mut flash_uid = [0; 16];
    if flash.blocking_unique_id(&mut flash_uid).is_err() {
        flash_uid = [
            b'p', b'i', b'c', b'o', b'-', b'c', b'a', b'n', b'-', b'b', b'r', b'i', b'd', b'g',
            b'e', 0,
        ];
        warn!("flash unique ID read failed, using fallback link-local seed");
    }

    let device_ipv4_octets = link_local_from_seed(&flash_uid);
    let device_ipv4 = Ipv4Address::new(
        device_ipv4_octets[0],
        device_ipv4_octets[1],
        device_ipv4_octets[2],
        device_ipv4_octets[3],
    );
    let usb_driver = Driver::new(p.USB, Irqs);

    let mut usb_config = UsbConfig::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("pico-can-bridge-rs");
    usb_config.product = Some("Pico CAN Bridge CDC-NCM");
    usb_config.serial_number = Some("0001");
    usb_config.device_class = cdc_ncm::USB_CLASS_CDC;
    usb_config.device_sub_class = 0x00;
    usb_config.device_protocol = 0x00;
    usb_config.composite_with_iads = false;

    static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESCRIPTOR: StaticCell<[u8; 128]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();

    let mut builder = Builder::new(
        usb_driver,
        usb_config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        MSOS_DESCRIPTOR.init([0; 128]),
        CONTROL_BUF.init([0; 128]),
    );

    static NCM_STATE: StaticCell<cdc_ncm::State<'static>> = StaticCell::new();
    let ncm = cdc_ncm::CdcNcmClass::new(
        &mut builder,
        NCM_STATE.init(cdc_ncm::State::new()),
        HOST_MAC,
        USB_MAX_PACKET_SIZE,
    );

    static NCM_NET_STATE: StaticCell<NcmNetState<MTU, 4, 4>> = StaticCell::new();
    let (ncm_runner, ncm_device) =
        ncm.into_embassy_net_device(NCM_NET_STATE.init(NcmNetState::new()), DEVICE_MAC);

    let net_config = NetConfig::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(device_ipv4, DEVICE_IPV4_PREFIX_LEN),
        gateway: None,
        dns_servers: Default::default(),
    });

    static NET_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
    let (stack, net_runner) = embassy_net::new(
        ncm_device,
        net_config,
        NET_RESOURCES.init(StackResources::new()),
        0x1234_5678,
    );

    let usb = builder.build();

    spawner.spawn(usb_task(usb).unwrap());
    spawner.spawn(ncm_task(ncm_runner).unwrap());
    spawner.spawn(net_task(net_runner).unwrap());
    spawner.spawn(http_task(stack).unwrap());
    spawner.spawn(can_task(p.SPI1, p.PIN_14, p.PIN_15, p.PIN_8, p.PIN_19).unwrap());

    info!(
        "USB CDC-NCM ready, IPv4 169.254.{}.{}/16, device MAC={=[u8]:02x}, host hint MAC={=[u8]:02x}",
        device_ipv4_octets[2], device_ipv4_octets[3], DEVICE_MAC, HOST_MAC
    );
    uart.blocking_write(b"USB CDC-NCM ready, IPv4 link-local from flash UID\r\n")
        .unwrap();
    uart.blocking_write(b"CAN task starting, SPI1 SCK GP14 MOSI GP15 MISO GP8 CS GP19\r\n")
        .unwrap();

    #[cfg(feature = "mdns")]
    let mut mdns_started = false;
    let mut link_watchdog_misses = 0;

    loop {
        match select(stack.wait_link_up(), Timer::after(CDC_NCM_LINK_UP_TIMEOUT)).await {
            Either::First(()) => {
                link_watchdog_misses = 0;
            }
            Either::Second(()) => {
                link_watchdog_misses += 1;
                warn!("CDC-NCM did not reach link-up before watchdog timeout");
                uart.blocking_write(b"CDC-NCM link watchdog timeout\r\n")
                    .unwrap();
                if link_watchdog_misses >= CDC_NCM_LINK_UP_RESET_MISSES {
                    warn!("CDC-NCM watchdog requesting system reset");
                    uart.blocking_write(b"CDC-NCM watchdog reset\r\n").unwrap();
                    cortex_m::peripheral::SCB::sys_reset();
                }
                continue;
            }
        }

        info!(
            "CDC-NCM link up, IPv4 address 169.254.{}.{}/16",
            device_ipv4_octets[2], device_ipv4_octets[3]
        );
        uart.blocking_write(b"CDC-NCM link up\r\n").unwrap();

        #[cfg(feature = "mdns")]
        if !mdns_started {
            uart.blocking_write(b"mDNS starting\r\n").unwrap();

            static MDNS_STATE: StaticCell<MdnsState<MdnsRng>> = StaticCell::new();

            let mdns = MDNS_STATE.init(MdnsState::new(
                EndpointConfig::new(),
                MdnsRng::new(0x7069_636f_6361_6e01),
            ));

            let mut records = ServiceRecords::new(
                Name::try_from_str("_http._tcp.local.").unwrap(),
                Name::try_from_str("Pico CAN Bridge._http._tcp.local.").unwrap(),
                Name::try_from_str("pico-can-bridge.local.").unwrap(),
                HTTP_PORT,
                120,
            );
            records.add_a(Ipv4Addr::new(
                device_ipv4_octets[0],
                device_ipv4_octets[1],
                device_ipv4_octets[2],
                device_ipv4_octets[3],
            ));

            match mdns.register_service(ServiceSpec::new(records)) {
                Ok(_) => {
                    spawner.spawn(mdns_task(stack, mdns).unwrap());
                    mdns_started = true;
                    uart.blocking_write(b"mDNS ready, host pico-can-bridge.local\r\n")
                        .unwrap();
                }
                Err(_) => {
                    warn!("mDNS service registration failed");
                    uart.blocking_write(b"mDNS registration failed\r\n")
                        .unwrap();
                }
            }
        }

        stack.wait_link_down().await;
        info!("CDC-NCM link down");
        uart.blocking_write(b"CDC-NCM link down\r\n").unwrap();
        Timer::after(Duration::from_millis(100)).await;
    }
}
