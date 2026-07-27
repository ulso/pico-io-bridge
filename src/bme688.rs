use embassy_time::{Duration, Timer};
use embedded_hal_async::i2c::I2c;

pub(crate) const PRIMARY_ADDRESS: u8 = 0x76;
pub(crate) const SECONDARY_ADDRESS: u8 = 0x77;

const CHIP_ID: u8 = 0x61;
const HIGH_GAS_VARIANT: u8 = 0x01;

const FIELD0: u8 = 0x1d;
const CTRL_GAS_0: u8 = 0x70;
const CTRL_GAS_1: u8 = 0x71;
const CTRL_HUM: u8 = 0x72;
const CTRL_MEAS: u8 = 0x74;
const CONFIG: u8 = 0x75;
const RES_HEAT0: u8 = 0x5a;
const GAS_WAIT0: u8 = 0x64;
const CHIP_ID_REGISTER: u8 = 0xd0;
const SOFT_RESET: u8 = 0xe0;
const VARIANT_ID: u8 = 0xf0;

const SOFT_RESET_COMMAND: u8 = 0xb6;
const FORCED_MODE: u8 = 0x01;
const MODE_MASK: u8 = 0x03;
const HEATER_DISABLE: u8 = 1 << 3;
const RUN_GAS_HIGH: u8 = 2 << 4;
const RUN_GAS_MASK: u8 = 0x30;
const NEW_DATA: u8 = 1 << 7;
const GAS_VALID: u8 = 1 << 5;
const HEATER_STABLE: u8 = 1 << 4;

const HEATER_TARGET_C: u16 = 300;
const HEATER_DURATION_MS: u16 = 100;
const MEASUREMENT_DELAY: Duration = Duration::from_millis(150);
const POLL_DELAY: Duration = Duration::from_millis(10);
const POLL_ATTEMPTS: usize = 5;

#[derive(Clone, Copy)]
pub(crate) struct Calibration {
    par_h1: u16,
    par_h2: u16,
    par_h3: i8,
    par_h4: i8,
    par_h5: i8,
    par_h6: u8,
    par_h7: i8,
    par_gh1: i8,
    par_gh2: i16,
    par_gh3: i8,
    par_t1: u16,
    par_t2: i16,
    par_t3: i8,
    par_p1: u16,
    par_p2: i16,
    par_p3: i8,
    par_p4: i16,
    par_p5: i16,
    par_p6: i8,
    par_p7: i8,
    par_p8: i16,
    par_p9: i16,
    par_p10: u8,
    res_heat_range: u8,
    res_heat_val: i8,
}

#[derive(Clone, Copy)]
pub(crate) struct Measurement {
    pub(crate) temperature_c: f32,
    pub(crate) pressure_pa: f32,
    pub(crate) humidity_percent: f32,
    pub(crate) gas_resistance_ohm: f32,
}

#[derive(Clone, Copy)]
pub(crate) enum Error<E> {
    I2c(E),
    InvalidIdentity,
    MeasurementInvalid,
}

pub(crate) async fn initialize<I2C>(
    bus: &mut I2C,
    address: u8,
) -> Result<Calibration, Error<I2C::Error>>
where
    I2C: I2c,
{
    if address != PRIMARY_ADDRESS && address != SECONDARY_ADDRESS {
        return Err(Error::InvalidIdentity);
    }

    write_register(bus, address, SOFT_RESET, SOFT_RESET_COMMAND).await?;
    Timer::after(Duration::from_millis(10)).await;

    if read_register(bus, address, CHIP_ID_REGISTER).await? != CHIP_ID
        || read_register(bus, address, VARIANT_ID).await? != HIGH_GAS_VARIANT
    {
        return Err(Error::InvalidIdentity);
    }

    let calibration = read_calibration(bus, address).await?;

    // Humidity x2, temperature x2, pressure x4 and IIR filter coefficient 3.
    write_register(bus, address, CTRL_HUM, 0x02).await?;
    write_register(bus, address, CTRL_MEAS, 0x4c).await?;
    write_register(bus, address, CONFIG, 0x08).await?;

    write_register(
        bus,
        address,
        RES_HEAT0,
        calculate_heater_resistance(HEATER_TARGET_C, &calibration),
    )
    .await?;
    write_register(
        bus,
        address,
        GAS_WAIT0,
        encode_heater_duration(HEATER_DURATION_MS),
    )
    .await?;

    let heater_control = read_register(bus, address, CTRL_GAS_0).await? & !HEATER_DISABLE;
    write_register(bus, address, CTRL_GAS_0, heater_control).await?;
    let gas_control =
        (read_register(bus, address, CTRL_GAS_1).await? & !(RUN_GAS_MASK | 0x0f)) | RUN_GAS_HIGH;
    write_register(bus, address, CTRL_GAS_1, gas_control).await?;

    // The first heater cycle after a cold start commonly lacks HEATER_STABLE.
    match measure(bus, address, calibration).await {
        Ok(_) | Err(Error::MeasurementInvalid) => {}
        Err(error) => return Err(error),
    }

    Ok(calibration)
}

pub(crate) async fn sleep<I2C>(bus: &mut I2C, address: u8) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    let mode = read_register(bus, address, CTRL_MEAS).await? & !MODE_MASK;
    write_register(bus, address, CTRL_MEAS, mode).await?;

    let gas_control = read_register(bus, address, CTRL_GAS_1).await? & !RUN_GAS_MASK;
    write_register(bus, address, CTRL_GAS_1, gas_control).await?;
    let heater_control = read_register(bus, address, CTRL_GAS_0).await? | HEATER_DISABLE;
    write_register(bus, address, CTRL_GAS_0, heater_control).await
}

pub(crate) async fn measure<I2C>(
    bus: &mut I2C,
    address: u8,
    calibration: Calibration,
) -> Result<Measurement, Error<I2C::Error>>
where
    I2C: I2c,
{
    let mode = (read_register(bus, address, CTRL_MEAS).await? & !MODE_MASK) | FORCED_MODE;
    write_register(bus, address, CTRL_MEAS, mode).await?;
    Timer::after(MEASUREMENT_DELAY).await;

    for _ in 0..POLL_ATTEMPTS {
        let mut field = [0; 17];
        bus.write_read(address, &[FIELD0], &mut field)
            .await
            .map_err(Error::I2c)?;

        if field[0] & NEW_DATA == 0 {
            Timer::after(POLL_DELAY).await;
            continue;
        }
        if field[16] & (GAS_VALID | HEATER_STABLE) != (GAS_VALID | HEATER_STABLE) {
            return Err(Error::MeasurementInvalid);
        }

        let pressure_adc =
            (u32::from(field[2]) << 12) | (u32::from(field[3]) << 4) | u32::from(field[4] >> 4);
        let temperature_adc =
            (u32::from(field[5]) << 12) | (u32::from(field[6]) << 4) | u32::from(field[7] >> 4);
        let humidity_adc = u16::from_be_bytes([field[8], field[9]]);
        let gas_adc = (u16::from(field[15]) << 2) | u16::from(field[16] >> 6);
        let gas_range = field[16] & 0x0f;

        let (temperature_c, t_fine) = compensate_temperature(temperature_adc, &calibration);
        let pressure_pa = compensate_pressure(pressure_adc, t_fine, &calibration);
        let humidity_percent = compensate_humidity(humidity_adc, t_fine, &calibration);
        let gas_resistance_ohm = compensate_gas_resistance(gas_adc, gas_range);

        if !temperature_c.is_finite()
            || !pressure_pa.is_finite()
            || pressure_pa <= 0.0
            || !humidity_percent.is_finite()
            || !gas_resistance_ohm.is_finite()
            || gas_resistance_ohm <= 0.0
        {
            return Err(Error::MeasurementInvalid);
        }

        return Ok(Measurement {
            temperature_c,
            pressure_pa,
            humidity_percent,
            gas_resistance_ohm,
        });
    }

    Err(Error::MeasurementInvalid)
}

async fn read_calibration<I2C>(bus: &mut I2C, address: u8) -> Result<Calibration, Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut coefficients = [0; 42];
    bus.write_read(address, &[0x8a], &mut coefficients[..23])
        .await
        .map_err(Error::I2c)?;
    bus.write_read(address, &[0xe1], &mut coefficients[23..37])
        .await
        .map_err(Error::I2c)?;
    bus.write_read(address, &[0x00], &mut coefficients[37..])
        .await
        .map_err(Error::I2c)?;

    Ok(Calibration {
        par_t1: u16::from_le_bytes([coefficients[31], coefficients[32]]),
        par_t2: i16::from_le_bytes([coefficients[0], coefficients[1]]),
        par_t3: coefficients[2] as i8,
        par_p1: u16::from_le_bytes([coefficients[4], coefficients[5]]),
        par_p2: i16::from_le_bytes([coefficients[6], coefficients[7]]),
        par_p3: coefficients[8] as i8,
        par_p4: i16::from_le_bytes([coefficients[10], coefficients[11]]),
        par_p5: i16::from_le_bytes([coefficients[12], coefficients[13]]),
        par_p6: coefficients[15] as i8,
        par_p7: coefficients[14] as i8,
        par_p8: i16::from_le_bytes([coefficients[18], coefficients[19]]),
        par_p9: i16::from_le_bytes([coefficients[20], coefficients[21]]),
        par_p10: coefficients[22],
        par_h1: (u16::from(coefficients[25]) << 4) | u16::from(coefficients[24] & 0x0f),
        par_h2: (u16::from(coefficients[23]) << 4) | u16::from(coefficients[24] >> 4),
        par_h3: coefficients[26] as i8,
        par_h4: coefficients[27] as i8,
        par_h5: coefficients[28] as i8,
        par_h6: coefficients[29],
        par_h7: coefficients[30] as i8,
        par_gh1: coefficients[35] as i8,
        par_gh2: i16::from_le_bytes([coefficients[33], coefficients[34]]),
        par_gh3: coefficients[36] as i8,
        res_heat_range: (coefficients[39] & 0x30) >> 4,
        res_heat_val: coefficients[37] as i8,
    })
}

// Floating-point compensation equations from the Bosch BME688 data sheet.
fn compensate_temperature(adc: u32, calibration: &Calibration) -> (f32, f32) {
    let adc = adc as f32;
    let var1 =
        (adc / 16384.0 - f32::from(calibration.par_t1) / 1024.0) * f32::from(calibration.par_t2);
    let delta = adc / 131072.0 - f32::from(calibration.par_t1) / 8192.0;
    let var2 = delta * delta * (f32::from(calibration.par_t3) * 16.0);
    let t_fine = var1 + var2;
    (t_fine / 5120.0, t_fine)
}

fn compensate_pressure(adc: u32, t_fine: f32, calibration: &Calibration) -> f32 {
    let mut var1 = t_fine / 2.0 - 64000.0;
    let mut var2 = var1 * var1 * (f32::from(calibration.par_p6) / 131072.0);
    var2 += var1 * f32::from(calibration.par_p5) * 2.0;
    var2 = var2 / 4.0 + f32::from(calibration.par_p4) * 65536.0;
    var1 = (f32::from(calibration.par_p3) * var1 * var1 / 16384.0
        + f32::from(calibration.par_p2) * var1)
        / 524288.0;
    var1 = (1.0 + var1 / 32768.0) * f32::from(calibration.par_p1);
    if var1 == 0.0 {
        return f32::NAN;
    }

    let mut pressure = (1048576.0 - adc as f32 - var2 / 4096.0) * 6250.0 / var1;
    var1 = f32::from(calibration.par_p9) * pressure * pressure / 2147483648.0;
    var2 = pressure * (f32::from(calibration.par_p8) / 32768.0);
    let var3 = pressure / 256.0
        * (pressure / 256.0)
        * (pressure / 256.0)
        * (f32::from(calibration.par_p10) / 131072.0);
    pressure += (var1 + var2 + var3 + f32::from(calibration.par_p7) * 128.0) / 16.0;
    pressure
}

fn compensate_humidity(adc: u16, t_fine: f32, calibration: &Calibration) -> f32 {
    let temperature = t_fine / 5120.0;
    let var1 = f32::from(adc)
        - (f32::from(calibration.par_h1) * 16.0
            + f32::from(calibration.par_h3) / 2.0 * temperature);
    let var2 = var1
        * (f32::from(calibration.par_h2) / 262144.0
            * (1.0
                + f32::from(calibration.par_h4) / 16384.0 * temperature
                + f32::from(calibration.par_h5) / 1048576.0 * temperature * temperature));
    let var3 = f32::from(calibration.par_h6) / 16384.0;
    let var4 = f32::from(calibration.par_h7) / 2097152.0;
    (var2 + (var3 + var4 * temperature) * var2 * var2).clamp(0.0, 100.0)
}

fn compensate_gas_resistance(adc: u16, range: u8) -> f32 {
    let var1 = 262144u32 >> range;
    let var2 = 4096 + (i32::from(adc) - 512) * 3;
    1_000_000.0 * var1 as f32 / var2 as f32
}

fn calculate_heater_resistance(target_c: u16, calibration: &Calibration) -> u8 {
    let target_c = target_c.min(400) as f32;
    let var1 = f32::from(calibration.par_gh1) / 16.0 + 49.0;
    let var2 = f32::from(calibration.par_gh2) / 32768.0 * 0.0005 + 0.00235;
    let var3 = f32::from(calibration.par_gh3) / 1024.0;
    let var4 = var1 * (1.0 + var2 * target_c);
    let var5 = var4 + var3 * 25.0;
    (3.4 * (var5
        * (4.0 / (4.0 + f32::from(calibration.res_heat_range)))
        * (1.0 / (1.0 + f32::from(calibration.res_heat_val) * 0.002))
        - 25.0)) as u8
}

fn encode_heater_duration(mut duration_ms: u16) -> u8 {
    if duration_ms >= 0x0fc0 {
        return 0xff;
    }

    let mut factor = 0;
    while duration_ms > 0x3f {
        duration_ms /= 4;
        factor += 1;
    }
    duration_ms as u8 + factor * 64
}

async fn read_register<I2C>(
    bus: &mut I2C,
    address: u8,
    register: u8,
) -> Result<u8, Error<I2C::Error>>
where
    I2C: I2c,
{
    let mut value = [0];
    bus.write_read(address, &[register], &mut value)
        .await
        .map_err(Error::I2c)?;
    Ok(value[0])
}

async fn write_register<I2C>(
    bus: &mut I2C,
    address: u8,
    register: u8,
    value: u8,
) -> Result<(), Error<I2C::Error>>
where
    I2C: I2c,
{
    bus.write(address, &[register, value])
        .await
        .map_err(Error::I2c)
}
