use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::{Error as _, ErrorKind, I2c};

pub(crate) const PRIMARY_ADDRESS: u8 = 0x4a;
pub(crate) const SECONDARY_ADDRESS: u8 = 0x4b;

const CHANNEL_EXECUTABLE: u8 = 1;
const CHANNEL_CONTROL: u8 = 2;
const CHANNEL_REPORTS: u8 = 3;

const EXECUTABLE_RESET: u8 = 1;
const REPORT_PRODUCT_ID_RESPONSE: u8 = 0xf8;
const REPORT_PRODUCT_ID_REQUEST: u8 = 0xf9;
const REPORT_BASE_TIMESTAMP: u8 = 0xfb;
const REPORT_SET_FEATURE: u8 = 0xfd;

const REPORT_ACCELEROMETER: u8 = 0x01;
const REPORT_GYROSCOPE: u8 = 0x02;
const REPORT_MAGNETIC_FIELD: u8 = 0x03;
const REPORT_ROTATION_VECTOR: u8 = 0x05;

const REPORT_INTERVAL_US: u32 = 100_000;
const RESET_DELAY: Duration = Duration::from_millis(300);
const REPORT_START_DELAY: Duration = Duration::from_millis(150);
const EMPTY_POLL_DELAY: Duration = Duration::from_millis(5);
const PRODUCT_RESPONSE_ATTEMPTS: usize = 40;
const MEASUREMENT_ATTEMPTS: usize = 100;
const STARTUP_PACKET_LIMIT: usize = 24;

const I2C_TRANSFER_SIZE: usize = 32;
const I2C_CARGO_SIZE: usize = I2C_TRANSFER_SIZE - 4;
const PACKET_CAPACITY: usize = 272;

#[derive(Clone, Copy)]
pub(crate) enum Error<E> {
    I2c(E),
    InvalidAddress,
    InvalidIdentity,
    Protocol,
    Timeout,
}

#[derive(Clone, Copy)]
pub(crate) struct Vector3 {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
    pub(crate) accuracy: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct Quaternion {
    pub(crate) i: f32,
    pub(crate) j: f32,
    pub(crate) k: f32,
    pub(crate) real: f32,
    pub(crate) accuracy_radians: f32,
    pub(crate) accuracy: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct Measurement {
    pub(crate) acceleration: Vector3,
    pub(crate) gyroscope: Vector3,
    pub(crate) magnetic_field: Vector3,
    pub(crate) rotation: Quaternion,
}

impl Measurement {
    pub(crate) const fn invalid() -> Self {
        let vector = Vector3 {
            x: f32::NAN,
            y: f32::NAN,
            z: f32::NAN,
            accuracy: 0,
        };
        Self {
            acceleration: vector,
            gyroscope: vector,
            magnetic_field: vector,
            rotation: Quaternion {
                i: f32::NAN,
                j: f32::NAN,
                k: f32::NAN,
                real: f32::NAN,
                accuracy_radians: f32::NAN,
                accuracy: 0,
            },
        }
    }
}

struct Packet {
    channel: u8,
    payload: [u8; PACKET_CAPACITY],
    len: usize,
    truncated: bool,
}

impl Packet {
    const fn empty() -> Self {
        Self {
            channel: 0,
            payload: [0; PACKET_CAPACITY],
            len: 0,
            truncated: false,
        }
    }
}

#[derive(Default)]
struct PartialMeasurement {
    acceleration: Option<Vector3>,
    gyroscope: Option<Vector3>,
    magnetic_field: Option<Vector3>,
    rotation: Option<Quaternion>,
}

impl PartialMeasurement {
    fn complete(self) -> Option<Measurement> {
        Some(Measurement {
            acceleration: self.acceleration?,
            gyroscope: self.gyroscope?,
            magnetic_field: self.magnetic_field?,
            rotation: self.rotation?,
        })
    }

    fn is_complete(&self) -> bool {
        self.acceleration.is_some()
            && self.gyroscope.is_some()
            && self.magnetic_field.is_some()
            && self.rotation.is_some()
    }
}

pub(crate) async fn initialize<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    validate_address(address)?;
    reset(bus, address).await?;
    Timer::after(RESET_DELAY).await;
    drain_startup_packets(bus, address).await?;

    let mut control_sequence = 0;
    send_packet(
        bus,
        address,
        CHANNEL_CONTROL,
        &mut control_sequence,
        &[REPORT_PRODUCT_ID_REQUEST, 0],
    )
    .await?;
    wait_for_product_id(bus, address).await?;

    for report in [
        REPORT_ACCELEROMETER,
        REPORT_GYROSCOPE,
        REPORT_MAGNETIC_FIELD,
        REPORT_ROTATION_VECTOR,
    ] {
        enable_report(bus, address, &mut control_sequence, report).await?;
    }

    Timer::after(REPORT_START_DELAY).await;
    let _ = read_measurement(bus, address).await?;
    Ok(())
}

pub(crate) async fn deinitialize<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    reset(bus, address).await
}

pub(crate) async fn read_measurement<I2C>(
    bus: &mut I2C,
    address: u8,
) -> Result<Measurement, Error<I2C::Error>>
where
    I2C: I2c,
{
    validate_address(address)?;
    let mut measurement = PartialMeasurement::default();

    for _ in 0..MEASUREMENT_ATTEMPTS {
        match receive_packet(bus, address).await? {
            Some(packet) => {
                parse_sensor_packet(&packet, &mut measurement)?;
                if measurement.is_complete() {
                    return measurement.complete().ok_or(Error::Protocol);
                }
            }
            None => Timer::after(EMPTY_POLL_DELAY).await,
        }
    }

    measurement.complete().ok_or(Error::Timeout)
}

fn validate_address<E>(address: u8) -> Result<(), Error<E>> {
    if address == PRIMARY_ADDRESS || address == SECONDARY_ADDRESS {
        Ok(())
    } else {
        Err(Error::InvalidAddress)
    }
}

async fn reset<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    bus.write(address, &[5, 0, CHANNEL_EXECUTABLE, 0, EXECUTABLE_RESET])
        .await
        .map_err(Error::I2c)
}

async fn drain_startup_packets<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    for _ in 0..STARTUP_PACKET_LIMIT {
        if receive_packet(bus, address).await?.is_none() {
            break;
        }
    }
    Ok(())
}

async fn wait_for_product_id<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    for _ in 0..PRODUCT_RESPONSE_ATTEMPTS {
        match receive_packet(bus, address).await? {
            Some(packet)
                if packet.channel == CHANNEL_CONTROL
                    && packet.len >= 14
                    && packet.payload[0] == REPORT_PRODUCT_ID_RESPONSE =>
            {
                return Ok(());
            }
            Some(_) => {}
            None => Timer::after(EMPTY_POLL_DELAY).await,
        }
    }
    Err(Error::InvalidIdentity)
}

async fn enable_report<I2C>(
    bus: &mut I2C,
    address: u8,
    sequence: &mut u8,
    report: u8,
) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut payload = [0; 17];
    payload[0] = REPORT_SET_FEATURE;
    payload[1] = report;
    payload[5..9].copy_from_slice(&REPORT_INTERVAL_US.to_le_bytes());
    send_packet(bus, address, CHANNEL_CONTROL, sequence, &payload).await
}

async fn send_packet<I2C>(
    bus: &mut I2C,
    address: u8,
    channel: u8,
    sequence: &mut u8,
    payload: &[u8],
) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    let packet_len = payload.len() + 4;
    let mut packet = [0; 32];
    packet[..2].copy_from_slice(&(packet_len as u16).to_le_bytes());
    packet[2] = channel;
    packet[3] = *sequence;
    packet[4..packet_len].copy_from_slice(payload);
    *sequence = sequence.wrapping_add(1);
    bus.write(address, &packet[..packet_len])
        .await
        .map_err(Error::I2c)
}

async fn receive_packet<I2C>(
    bus: &mut I2C,
    address: u8,
) -> Result<Option<Packet>, Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut header = [0; 4];
    match bus.read(address, &mut header).await {
        Ok(()) => {}
        Err(error) if matches!(error.kind(), ErrorKind::NoAcknowledge(_)) => return Ok(None),
        Err(error) => return Err(Error::I2c(error)),
    }

    let packet_len = usize::from(u16::from_le_bytes([header[0], header[1]]) & 0x7fff);
    if packet_len == 0 {
        return Ok(None);
    }
    if packet_len < 4 {
        return Err(Error::Protocol);
    }

    let mut packet = Packet::empty();
    packet.channel = header[2];
    let mut cargo_remaining = packet_len - 4;
    let mut stored = 0;

    while cargo_remaining > 0 {
        let cargo_len = cargo_remaining.min(I2C_CARGO_SIZE);
        let mut transfer = [0; I2C_TRANSFER_SIZE];
        bus.read(address, &mut transfer[..cargo_len + 4])
            .await
            .map_err(Error::I2c)?;

        let copy_len = cargo_len.min(PACKET_CAPACITY.saturating_sub(stored));
        packet.payload[stored..stored + copy_len].copy_from_slice(&transfer[4..4 + copy_len]);
        stored += copy_len;
        cargo_remaining -= cargo_len;
    }

    packet.len = stored;
    packet.truncated = packet_len - 4 > PACKET_CAPACITY;
    Ok(Some(packet))
}

fn parse_sensor_packet<E>(
    packet: &Packet,
    measurement: &mut PartialMeasurement,
) -> Result<(), Error<E>> {
    if packet.channel != CHANNEL_REPORTS
        || packet.len < 5
        || packet.payload[0] != REPORT_BASE_TIMESTAMP
    {
        return Ok(());
    }

    let mut offset = 5;
    while offset < packet.len {
        let report = packet.payload[offset];
        let report_len = match report {
            REPORT_ACCELEROMETER | REPORT_GYROSCOPE | REPORT_MAGNETIC_FIELD => 10,
            REPORT_ROTATION_VECTOR => 14,
            _ => break,
        };
        if offset + report_len > packet.len {
            if packet.truncated {
                break;
            }
            return Err(Error::Protocol);
        }

        match report {
            REPORT_ACCELEROMETER => {
                measurement.acceleration = Some(parse_vector(&packet.payload[offset..], 8));
            }
            REPORT_GYROSCOPE => {
                measurement.gyroscope = Some(parse_vector(&packet.payload[offset..], 9));
            }
            REPORT_MAGNETIC_FIELD => {
                measurement.magnetic_field = Some(parse_vector(&packet.payload[offset..], 4));
            }
            REPORT_ROTATION_VECTOR => {
                measurement.rotation = Some(parse_quaternion(&packet.payload[offset..]));
            }
            _ => {}
        }
        offset += report_len;
    }
    Ok(())
}

fn parse_vector(report: &[u8], q_point: u8) -> Vector3 {
    Vector3 {
        x: q_to_f32(i16::from_le_bytes([report[4], report[5]]), q_point),
        y: q_to_f32(i16::from_le_bytes([report[6], report[7]]), q_point),
        z: q_to_f32(i16::from_le_bytes([report[8], report[9]]), q_point),
        accuracy: report[2] & 0x03,
    }
}

fn parse_quaternion(report: &[u8]) -> Quaternion {
    Quaternion {
        i: q_to_f32(i16::from_le_bytes([report[4], report[5]]), 14),
        j: q_to_f32(i16::from_le_bytes([report[6], report[7]]), 14),
        k: q_to_f32(i16::from_le_bytes([report[8], report[9]]), 14),
        real: q_to_f32(i16::from_le_bytes([report[10], report[11]]), 14),
        accuracy_radians: q_to_f32(i16::from_le_bytes([report[12], report[13]]), 12),
        accuracy: report[2] & 0x03,
    }
}

fn q_to_f32(value: i16, q_point: u8) -> f32 {
    f32::from(value) / (1u32 << q_point) as f32
}
