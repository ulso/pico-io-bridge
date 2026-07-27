use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::I2c;

pub(crate) const ADDRESS: u8 = 0x0b;
pub(crate) const EXPECTED_VERSION: u16 = 0x2717;

const INIT_RSOC: u8 = 0x07;
const CELL_VOLTAGE: u8 = 0x09;
const APA: u8 = 0x0b;
const CELL_ITE: u8 = 0x0f;
const IC_VERSION: u8 = 0x11;
const BATTERY_PROFILE: u8 = 0x12;
const POWER_MODE: u8 = 0x15;

const POWER_OPERATE: u16 = 0x0001;
const POWER_SLEEP: u16 = 0x0002;
const BATTERY_PROFILE_4_2V: u16 = 0x0001;
const INITIALIZE_RSOC: u16 = 0xaa55;

#[derive(Clone, Copy)]
pub(crate) enum Error<E> {
    I2c(E),
    Crc,
    InvalidIdentity,
    InvalidCapacity,
}

pub(crate) async fn initialize<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    if address != ADDRESS {
        return Err(Error::InvalidIdentity);
    }
    let version = read_word(bus, address, IC_VERSION).await?;
    if version != EXPECTED_VERSION {
        return Err(Error::InvalidIdentity);
    }
    write_word(bus, address, POWER_MODE, POWER_OPERATE).await
}

pub(crate) async fn sleep<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    write_word(bus, address, POWER_MODE, POWER_SLEEP).await
}

pub(crate) async fn set_capacity<I2C>(
    bus: &mut I2C,
    address: u8,
    capacity_mah: u16,
) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    let apa = match capacity_mah {
        100 => 0x08,
        200 => 0x0b,
        500 => 0x10,
        1000 => 0x19,
        2000 => 0x2d,
        3000 => 0x36,
        _ => return Err(Error::InvalidCapacity),
    };

    write_word(bus, address, APA, apa).await?;
    write_word(bus, address, BATTERY_PROFILE, BATTERY_PROFILE_4_2V).await?;
    write_word(bus, address, INIT_RSOC, INITIALIZE_RSOC).await?;
    Timer::after(Duration::from_millis(100)).await;
    Ok(())
}

pub(crate) async fn read_voltage_mv<I2C>(
    bus: &mut I2C,
    address: u8,
) -> Result<u16, Error<I2C::Error>>
where
    I2C: I2c,
{
    read_word(bus, address, CELL_VOLTAGE).await
}

pub(crate) async fn read_soc_tenths<I2C>(
    bus: &mut I2C,
    address: u8,
) -> Result<u16, Error<I2C::Error>>
where
    I2C: I2c,
{
    read_word(bus, address, CELL_ITE).await
}

async fn read_word<I2C>(bus: &mut I2C, address: u8, register: u8) -> Result<u16, Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut response = [0; 3];
    bus.write_read(address, &[register], &mut response)
        .await
        .map_err(Error::I2c)?;

    let crc_data = [
        address << 1,
        register,
        (address << 1) | 1,
        response[0],
        response[1],
    ];
    if crc8(&crc_data) != response[2] {
        return Err(Error::Crc);
    }
    Ok(u16::from_le_bytes([response[0], response[1]]))
}

async fn write_word<I2C>(
    bus: &mut I2C,
    address: u8,
    register: u8,
    value: u16,
) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    let value = value.to_le_bytes();
    let crc_data = [address << 1, register, value[0], value[1]];
    let data = [register, value[0], value[1], crc8(&crc_data)];
    bus.write(address, &data).await.map_err(Error::I2c)
}

fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0;
    for byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x07
            };
        }
    }
    crc
}
