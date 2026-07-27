use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::I2c;

pub(crate) const FIRST_ADDRESS: u8 = 0x36;
pub(crate) const LAST_ADDRESS: u8 = 0x3d;

const STATUS_BASE: u8 = 0x00;
const GPIO_BASE: u8 = 0x01;
const ENCODER_BASE: u8 = 0x11;

const STATUS_HW_ID: u8 = 0x01;
const STATUS_OPTIONS: u8 = 0x03;
const STATUS_SOFTWARE_RESET: u8 = 0x7f;
const GPIO_DIR_CLEAR: u8 = 0x03;
const GPIO_BULK: u8 = 0x04;
const GPIO_BULK_SET: u8 = 0x05;
const GPIO_PULL_ENABLE_SET: u8 = 0x0b;
const GPIO_PULL_ENABLE_CLEAR: u8 = 0x0c;
const ENCODER_POSITION: u8 = 0x30;
const ENCODER_DELTA: u8 = 0x40;

const SOFTWARE_RESET_COMMAND: u8 = 0xff;
const BUTTON_PIN: u8 = 24;
const BUTTON_MASK: u32 = 1 << BUTTON_PIN;
const REGISTER_READ_DELAY: Duration = Duration::from_micros(250);
const RESET_DELAY: Duration = Duration::from_millis(10);
const IDENTITY_RETRY_DELAY: Duration = Duration::from_millis(10);
const IDENTITY_RETRIES: usize = 10;

#[derive(Clone, Copy)]
pub(crate) enum Error<E> {
    I2c(E),
    InvalidIdentity,
}

pub(crate) async fn initialize<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    if !(FIRST_ADDRESS..=LAST_ADDRESS).contains(&address) {
        return Err(Error::InvalidIdentity);
    }

    write(
        bus,
        address,
        STATUS_BASE,
        STATUS_SOFTWARE_RESET,
        &[SOFTWARE_RESET_COMMAND],
    )
    .await?;
    Timer::after(RESET_DELAY).await;

    wait_for_identity(bus, address).await?;

    let mut options = [0; 4];
    read(bus, address, STATUS_BASE, STATUS_OPTIONS, &mut options).await?;
    if u32::from_be_bytes(options) & (1 << ENCODER_BASE) == 0 {
        return Err(Error::InvalidIdentity);
    }

    let button_mask = BUTTON_MASK.to_be_bytes();
    write(bus, address, GPIO_BASE, GPIO_DIR_CLEAR, &button_mask).await?;
    write(bus, address, GPIO_BASE, GPIO_PULL_ENABLE_SET, &button_mask).await?;
    write(bus, address, GPIO_BASE, GPIO_BULK_SET, &button_mask).await?;

    let _ = position(bus, address).await?;
    let _ = delta(bus, address).await?;
    let _ = button_pressed(bus, address).await?;
    Ok(())
}

async fn wait_for_identity<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut last_i2c_error = None;
    let mut received_response = false;

    for attempt in 0..IDENTITY_RETRIES {
        let mut hardware_id = [0];
        match read(bus, address, STATUS_BASE, STATUS_HW_ID, &mut hardware_id).await {
            Ok(()) if hardware_id[0] == 0x55 || (0x84..=0x89).contains(&hardware_id[0]) => {
                return Ok(());
            }
            Ok(()) => received_response = true,
            Err(Error::I2c(error)) => last_i2c_error = Some(error),
            Err(Error::InvalidIdentity) => return Err(Error::InvalidIdentity),
        }

        if attempt + 1 < IDENTITY_RETRIES {
            Timer::after(IDENTITY_RETRY_DELAY).await;
        }
    }

    if received_response {
        Err(Error::InvalidIdentity)
    } else if let Some(error) = last_i2c_error {
        Err(Error::I2c(error))
    } else {
        Err(Error::InvalidIdentity)
    }
}

pub(crate) async fn deinitialize<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    write(
        bus,
        address,
        GPIO_BASE,
        GPIO_PULL_ENABLE_CLEAR,
        &BUTTON_MASK.to_be_bytes(),
    )
    .await
}

pub(crate) async fn position<I2C>(bus: &mut I2C, address: u8) -> Result<i32, Error<I2C::Error>>
where
    I2C: I2c,
{
    read_i32(bus, address, ENCODER_POSITION).await
}

pub(crate) async fn set_position<I2C>(
    bus: &mut I2C,
    address: u8,
    position: i32,
) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    write(
        bus,
        address,
        ENCODER_BASE,
        ENCODER_POSITION,
        &position.to_be_bytes(),
    )
    .await
}

pub(crate) async fn delta<I2C>(bus: &mut I2C, address: u8) -> Result<i32, Error<I2C::Error>>
where
    I2C: I2c,
{
    read_i32(bus, address, ENCODER_DELTA).await
}

pub(crate) async fn button_pressed<I2C>(
    bus: &mut I2C,
    address: u8,
) -> Result<bool, Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut pins = [0; 4];
    read(bus, address, GPIO_BASE, GPIO_BULK, &mut pins).await?;
    Ok(u32::from_be_bytes(pins) & BUTTON_MASK == 0)
}

async fn read_i32<I2C>(bus: &mut I2C, address: u8, function: u8) -> Result<i32, Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut bytes = [0; 4];
    read(bus, address, ENCODER_BASE, function, &mut bytes).await?;
    Ok(i32::from_be_bytes(bytes))
}

async fn read<I2C>(
    bus: &mut I2C,
    address: u8,
    module: u8,
    function: u8,
    output: &mut [u8],
) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    bus.write(address, &[module, function])
        .await
        .map_err(Error::I2c)?;
    Timer::after(REGISTER_READ_DELAY).await;
    bus.read(address, output).await.map_err(Error::I2c)
}

async fn write<I2C>(
    bus: &mut I2C,
    address: u8,
    module: u8,
    function: u8,
    data: &[u8],
) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut bytes = [0; 6];
    bytes[0] = module;
    bytes[1] = function;
    bytes[2..2 + data.len()].copy_from_slice(data);
    bus.write(address, &bytes[..2 + data.len()])
        .await
        .map_err(Error::I2c)
}
