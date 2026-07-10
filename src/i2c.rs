use core::fmt::Write;

use defmt::*;
use embassy_futures::select::{Either, select};
use embassy_rp::i2c::{AbortReason, Async, Error as I2cError, I2c};
use embassy_rp::peripherals::I2C1;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use heapless::String;

use crate::json;

pub(crate) const I2C_FREQUENCY: u32 = 400_000;
pub(crate) const I2C_MAX_TRANSFER: usize = 64;
const I2C_COMMAND_CAPACITY: usize = 4;
const I2C_SCAN_FIRST: u8 = 0x08;
const I2C_SCAN_LAST: u8 = 0x77;
const I2C_SCAN_CAPACITY: usize = (I2C_SCAN_LAST - I2C_SCAN_FIRST + 1) as usize;

static I2C_COMMANDS: Channel<CriticalSectionRawMutex, I2cCommand, I2C_COMMAND_CAPACITY> =
    Channel::new();
static I2C_REPLIES: Channel<CriticalSectionRawMutex, I2cReply, I2C_COMMAND_CAPACITY> =
    Channel::new();
static I2C_CMD_LOCK: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());

#[derive(Clone, Copy)]
struct I2cState {
    ready: bool,
    frequency: u32,
    transaction_count: u32,
    error_count: u32,
}

impl I2cState {
    const fn ready() -> Self {
        Self {
            ready: true,
            frequency: I2C_FREQUENCY,
            transaction_count: 0,
            error_count: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct I2cBuffer {
    len: u8,
    data: [u8; I2C_MAX_TRANSFER],
}

impl I2cBuffer {
    const fn empty() -> Self {
        Self {
            len: 0,
            data: [0; I2C_MAX_TRANSFER],
        }
    }
}

#[derive(Clone, Copy)]
enum I2cCommand {
    Status,
    Scan,
    Read {
        address: u8,
        len: u8,
    },
    Write {
        address: u8,
        data: I2cBuffer,
    },
    WriteRead {
        address: u8,
        write: I2cBuffer,
        read_len: u8,
    },
}

#[derive(Clone, Copy)]
enum I2cReply {
    Status(I2cState),
    Scan {
        count: u8,
        addresses: [u8; I2C_SCAN_CAPACITY],
    },
    Read {
        address: u8,
        data: I2cBuffer,
    },
    Write {
        address: u8,
        written: u8,
    },
    WriteRead {
        address: u8,
        written: u8,
        data: I2cBuffer,
    },
    Error {
        code: &'static str,
        message: &'static str,
    },
}

fn valid_address(address: u32) -> Option<u8> {
    let address = u8::try_from(address).ok()?;
    (I2C_SCAN_FIRST..=I2C_SCAN_LAST)
        .contains(&address)
        .then_some(address)
}

fn parse_buffer(text: &str, key: &str) -> Option<I2cBuffer> {
    let mut buffer = I2cBuffer::empty();
    buffer.len = u8::try_from(json::parse_u8_array(text, key, &mut buffer.data)?).ok()?;
    Some(buffer)
}

fn parse_command(text: &str) -> Result<I2cCommand, (&'static str, &'static str)> {
    if json::parse_u32_field(text, "\"bus\"").unwrap_or(0) != 0 {
        return Err(("invalid_bus", "Only I2C bus 0 is available"));
    }
    if text.contains("i2c.status") {
        return Ok(I2cCommand::Status);
    }
    if text.contains("i2c.scan") {
        return Ok(I2cCommand::Scan);
    }

    let write_read = text.contains("i2c.write_read");
    let write = !write_read && text.contains("i2c.write");
    let read = text.contains("i2c.read");
    if !write_read && !write && !read {
        return Err((
            "unsupported_type",
            "Supported messages: i2c.status, i2c.scan, i2c.read, i2c.write, i2c.write_read",
        ));
    }

    let address = json::parse_u32_field(text, "\"address\"")
        .and_then(valid_address)
        .ok_or((
            "invalid_address",
            "I2C address must be between 0x08 and 0x77",
        ))?;

    if write_read {
        let write = parse_buffer(text, "\"write\"")
            .or_else(|| parse_buffer(text, "\"data\""))
            .ok_or(("invalid_data", "write must contain 1 to 64 bytes"))?;
        let read_len = json::parse_u32_field(text, "\"readLength\"")
            .and_then(|len| u8::try_from(len).ok())
            .filter(|len| (1..=I2C_MAX_TRANSFER as u8).contains(len))
            .ok_or(("invalid_length", "readLength must be between 1 and 64"))?;
        if write.len == 0 {
            return Err(("invalid_data", "write must contain 1 to 64 bytes"));
        }
        Ok(I2cCommand::WriteRead {
            address,
            write,
            read_len,
        })
    } else if write {
        let data = parse_buffer(text, "\"data\"")
            .ok_or(("invalid_data", "data must contain 1 to 64 bytes"))?;
        if data.len == 0 {
            return Err(("invalid_data", "data must contain 1 to 64 bytes"));
        }
        Ok(I2cCommand::Write { address, data })
    } else {
        let len = json::parse_u32_field(text, "\"length\"")
            .and_then(|len| u8::try_from(len).ok())
            .filter(|len| (1..=I2C_MAX_TRANSFER as u8).contains(len))
            .ok_or(("invalid_length", "length must be between 1 and 64"))?;
        Ok(I2cCommand::Read { address, len })
    }
}

fn write_data(out: &mut String<512>, data: &I2cBuffer) {
    for (index, byte) in data.data[..data.len as usize].iter().enumerate() {
        if index > 0 {
            let _ = core::write!(out, ",");
        }
        let _ = core::write!(out, "{}", byte);
    }
}

fn write_reply(out: &mut String<512>, reply: I2cReply) {
    match reply {
        I2cReply::Status(state) => {
            let _ = core::write!(
                out,
                "{{\"type\":\"i2c.status\",\"ok\":true,\"bus\":0,\"ready\":{},\"frequency\":{},\"transactionCount\":{},\"errorCount\":{},\"maxTransfer\":{}}}",
                if state.ready { "true" } else { "false" },
                state.frequency,
                state.transaction_count,
                state.error_count,
                I2C_MAX_TRANSFER
            );
        }
        I2cReply::Scan { count, addresses } => {
            let _ = core::write!(
                out,
                "{{\"type\":\"i2c.scan\",\"ok\":true,\"bus\":0,\"probe\":\"read\",\"addresses\":["
            );
            for (index, address) in addresses[..count as usize].iter().enumerate() {
                if index > 0 {
                    let _ = core::write!(out, ",");
                }
                let _ = core::write!(out, "{}", address);
            }
            let _ = core::write!(out, "]}}");
        }
        I2cReply::Read { address, data } => {
            let _ = core::write!(
                out,
                "{{\"type\":\"i2c.read\",\"ok\":true,\"bus\":0,\"address\":{},\"length\":{},\"data\":[",
                address,
                data.len
            );
            write_data(out, &data);
            let _ = core::write!(out, "]}}");
        }
        I2cReply::Write { address, written } => {
            let _ = core::write!(
                out,
                "{{\"type\":\"i2c.write\",\"ok\":true,\"bus\":0,\"address\":{},\"written\":{}}}",
                address,
                written
            );
        }
        I2cReply::WriteRead {
            address,
            written,
            data,
        } => {
            let _ = core::write!(
                out,
                "{{\"type\":\"i2c.write_read\",\"ok\":true,\"bus\":0,\"address\":{},\"written\":{},\"length\":{},\"data\":[",
                address,
                written,
                data.len
            );
            write_data(out, &data);
            let _ = core::write!(out, "]}}");
        }
        I2cReply::Error { code, message } => json::write_error(out, code, message),
    }
}

async fn send_command(command: I2cCommand) -> I2cReply {
    let _lock = I2C_CMD_LOCK.lock().await;

    while I2C_REPLIES.try_receive().is_ok() {}

    I2C_COMMANDS.send(command).await;
    match select(I2C_REPLIES.receive(), Timer::after(Duration::from_secs(2))).await {
        Either::First(reply) => reply,
        Either::Second(()) => I2cReply::Error {
            code: "i2c_timeout",
            message: "I2C controller did not answer",
        },
    }
}

pub(crate) async fn handle_i2c_ws_text(payload: &[u8], out: &mut String<512>) {
    let Ok(text) = core::str::from_utf8(payload) else {
        json::write_error(out, "invalid_json", "WebSocket payload must be UTF-8 JSON");
        return;
    };

    match parse_command(text) {
        Ok(command) => write_reply(out, send_command(command).await),
        Err((code, message)) => json::write_error(out, code, message),
    }
}

fn operation_error(error: I2cError) -> I2cReply {
    match error {
        I2cError::Abort(AbortReason::NoAcknowledge) => I2cReply::Error {
            code: "i2c_nack",
            message: "I2C device did not acknowledge the transaction",
        },
        _ => I2cReply::Error {
            code: "i2c_bus_error",
            message: "I2C transaction failed",
        },
    }
}

async fn execute_command(
    bus: &mut I2c<'static, I2C1, Async>,
    state: &mut I2cState,
    command: I2cCommand,
) -> I2cReply {
    match command {
        I2cCommand::Status => I2cReply::Status(*state),
        I2cCommand::Scan => {
            let mut addresses = [0; I2C_SCAN_CAPACITY];
            let mut count = 0;
            let mut probe = [0u8; 1];

            for address in I2C_SCAN_FIRST..=I2C_SCAN_LAST {
                if bus.read_async(address, &mut probe).await.is_ok() {
                    addresses[count] = address;
                    count += 1;
                }
            }
            state.transaction_count = state.transaction_count.wrapping_add(1);
            I2cReply::Scan {
                count: count as u8,
                addresses,
            }
        }
        I2cCommand::Read { address, len } => {
            let mut data = I2cBuffer::empty();
            data.len = len;
            state.transaction_count = state.transaction_count.wrapping_add(1);
            match bus
                .read_async(address, &mut data.data[..len as usize])
                .await
            {
                Ok(()) => I2cReply::Read { address, data },
                Err(error) => {
                    state.error_count = state.error_count.wrapping_add(1);
                    operation_error(error)
                }
            }
        }
        I2cCommand::Write { address, data } => {
            state.transaction_count = state.transaction_count.wrapping_add(1);
            match bus
                .write_async(address, data.data[..data.len as usize].iter().copied())
                .await
            {
                Ok(()) => I2cReply::Write {
                    address,
                    written: data.len,
                },
                Err(error) => {
                    state.error_count = state.error_count.wrapping_add(1);
                    operation_error(error)
                }
            }
        }
        I2cCommand::WriteRead {
            address,
            write,
            read_len,
        } => {
            let mut data = I2cBuffer::empty();
            data.len = read_len;
            state.transaction_count = state.transaction_count.wrapping_add(1);
            match bus
                .write_read_async(
                    address,
                    write.data[..write.len as usize].iter().copied(),
                    &mut data.data[..read_len as usize],
                )
                .await
            {
                Ok(()) => I2cReply::WriteRead {
                    address,
                    written: write.len,
                    data,
                },
                Err(error) => {
                    state.error_count = state.error_count.wrapping_add(1);
                    operation_error(error)
                }
            }
        }
    }
}

#[embassy_executor::task]
pub(crate) async fn i2c_task(mut bus: I2c<'static, I2C1, Async>) {
    let mut state = I2cState::ready();
    info!("STEMMA QT I2C ready: 400 kHz, SCL GP3, SDA GP2");

    loop {
        let command = I2C_COMMANDS.receive().await;
        let reply = execute_command(&mut bus, &mut state, command).await;
        I2C_REPLIES.send(reply).await;
    }
}
