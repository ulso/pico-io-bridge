use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::I2c;

const TEMPERATURE: u8 = 0x00;
const CONFIGURATION: u8 = 0x01;
const SAMPLE_PERIOD: u8 = 0x04;

const SHUTDOWN: u8 = 1 << 0;
const DEFAULT_CONVERSION_TIME: Duration = Duration::from_millis(100);

pub(crate) async fn initialize<I2C>(bus: &mut I2C, address: u8) -> Result<(), I2C::Error>
where
    I2C: I2c,
{
    let configuration = read_register(bus, address, CONFIGURATION).await?;
    bus.write(address, &[CONFIGURATION, configuration & !SHUTDOWN])
        .await?;

    // PCT2075-specific T_IDLE is readable and defaults to a 100 ms period.
    let _ = read_register(bus, address, SAMPLE_PERIOD).await?;
    Timer::after(DEFAULT_CONVERSION_TIME).await;
    let _ = read_temperature_eighths(bus, address).await?;
    Ok(())
}

pub(crate) async fn sleep<I2C>(bus: &mut I2C, address: u8) -> Result<(), I2C::Error>
where
    I2C: I2c,
{
    let configuration = read_register(bus, address, CONFIGURATION).await?;
    bus.write(address, &[CONFIGURATION, configuration | SHUTDOWN])
        .await
}

pub(crate) async fn read_temperature_eighths<I2C>(
    bus: &mut I2C,
    address: u8,
) -> Result<i16, I2C::Error>
where
    I2C: I2c,
{
    let mut bytes = [0; 2];
    bus.write_read(address, &[TEMPERATURE], &mut bytes).await?;
    Ok(i16::from_be_bytes(bytes) >> 5)
}

async fn read_register<I2C>(bus: &mut I2C, address: u8, register: u8) -> Result<u8, I2C::Error>
where
    I2C: I2c,
{
    let mut value = [0];
    bus.write_read(address, &[register], &mut value).await?;
    Ok(value[0])
}
