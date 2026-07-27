use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::I2c;

pub(crate) const PIXEL_COUNT: usize = 64;

const POWER_CONTROL: u8 = 0x00;
const RESET: u8 = 0x01;
const FRAME_RATE: u8 = 0x02;
const STATUS: u8 = 0x04;
const STATUS_CLEAR: u8 = 0x05;
const PIXEL_START: u8 = 0x80;

const NORMAL_MODE: u8 = 0x00;
const INITIAL_RESET: u8 = 0x3f;
const TEN_FRAMES_PER_SECOND: u8 = 0x00;
const CLEAR_ALL_STATUS: u8 = 0x0e;
const PIXEL_TEMPERATURE_OVERFLOW: u8 = 1 << 2;

pub(crate) async fn initialize<I2C>(bus: &mut I2C, address: u8) -> Result<(), I2C::Error>
where
    I2C: I2c,
{
    write_register(bus, address, POWER_CONTROL, NORMAL_MODE).await?;
    Timer::after(Duration::from_millis(50)).await;
    write_register(bus, address, RESET, INITIAL_RESET).await?;
    Timer::after(Duration::from_millis(2)).await;
    write_register(bus, address, FRAME_RATE, TEN_FRAMES_PER_SECOND).await?;
    write_register(bus, address, STATUS_CLEAR, CLEAR_ALL_STATUS).await?;

    // These readable configuration registers also make DEV:ADD fail on a NACK.
    let mut configuration = [0; 2];
    bus.write_read(address, &[POWER_CONTROL], &mut configuration)
        .await?;
    Ok(())
}

pub(crate) async fn sleep<I2C>(bus: &mut I2C, address: u8) -> Result<(), I2C::Error>
where
    I2C: I2c,
{
    write_register(bus, address, POWER_CONTROL, 0x10).await
}

pub(crate) async fn read_frame<I2C>(
    bus: &mut I2C,
    address: u8,
) -> Result<[i16; PIXEL_COUNT], I2C::Error>
where
    I2C: I2c,
{
    let mut status = [0];
    bus.write_read(address, &[STATUS], &mut status).await?;

    let mut bytes = [0; PIXEL_COUNT * 2];
    bus.write_read(address, &[PIXEL_START], &mut bytes).await?;

    if status[0] & PIXEL_TEMPERATURE_OVERFLOW != 0 {
        write_register(bus, address, STATUS_CLEAR, PIXEL_TEMPERATURE_OVERFLOW).await?;
    }

    let mut frame = [0; PIXEL_COUNT];
    for (pixel, bytes) in frame.iter_mut().zip(bytes.chunks_exact(2)) {
        let raw = u16::from_le_bytes([bytes[0], bytes[1]]) & 0x0fff;
        *pixel = ((raw << 4) as i16) >> 4;
    }
    Ok(frame)
}

async fn write_register<I2C>(
    bus: &mut I2C,
    address: u8,
    register: u8,
    value: u8,
) -> Result<(), I2C::Error>
where
    I2C: I2c,
{
    bus.write(address, &[register, value]).await
}
