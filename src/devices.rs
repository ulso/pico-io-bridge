use embassy_time::Delay;
use embedded_hal_async::i2c::I2c;
use vl53l4cd::{Vl53l4cd, wait::Poll};

pub(crate) const MAX_DEVICES: usize = 8;
const DISTANCE_MEASUREMENT_ATTEMPTS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceKind {
    Amg8833,
    Bme688,
    Lc709203f,
    Pct2075,
    SeesawEncoder,
    Vl53l4cd,
}

impl DeviceKind {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Amg8833 => "AMG8833",
            Self::Bme688 => "BME688",
            Self::Lc709203f => "LC709203F",
            Self::Pct2075 => "PCT2075",
            Self::SeesawEncoder => "SEESAW_ENCODER",
            Self::Vl53l4cd => "VL53L4CD",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("AMG8833") {
            Some(Self::Amg8833)
        } else if name.eq_ignore_ascii_case("BME688") {
            Some(Self::Bme688)
        } else if name.eq_ignore_ascii_case("LC709203F") {
            Some(Self::Lc709203f)
        } else if name.eq_ignore_ascii_case("PCT2075") {
            Some(Self::Pct2075)
        } else if name.eq_ignore_ascii_case("SEESAW_ENCODER") {
            Some(Self::SeesawEncoder)
        } else if name.eq_ignore_ascii_case("VL53L4CD") {
            Some(Self::Vl53l4cd)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DeviceConfig {
    pub(crate) slot: u8,
    pub(crate) kind: DeviceKind,
    pub(crate) bus: u8,
    pub(crate) address: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct DeviceList {
    pub(crate) entries: [Option<DeviceConfig>; MAX_DEVICES],
}

pub(crate) struct DeviceRegistry {
    entries: [Option<DeviceConfig>; MAX_DEVICES],
    battery_capacity_mah: [Option<u16>; MAX_DEVICES],
    bme688_calibration: [Option<crate::bme688::Calibration>; MAX_DEVICES],
}

impl DeviceRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [None; MAX_DEVICES],
            battery_capacity_mah: [None; MAX_DEVICES],
            bme688_calibration: [None; MAX_DEVICES],
        }
    }

    pub(crate) fn get(&self, slot: u8) -> Result<DeviceConfig, DeviceError> {
        let index = slot_index(slot)?;
        self.entries[index].ok_or(DeviceError::SlotEmpty)
    }

    pub(crate) fn list(&self) -> DeviceList {
        DeviceList {
            entries: self.entries,
        }
    }

    pub(crate) fn count(&self) -> u8 {
        self.entries.iter().flatten().count() as u8
    }

    fn validate_add(
        &self,
        slot: u8,
        kind: DeviceKind,
        address: u8,
    ) -> Result<DeviceConfig, DeviceError> {
        let index = slot_index(slot)?;
        if self.entries[index].is_some() {
            return Err(DeviceError::SlotOccupied);
        }
        if !(0x08..=0x77).contains(&address) {
            return Err(DeviceError::InvalidAddress);
        }
        if self
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.bus == 0 && entry.address == address)
        {
            return Err(DeviceError::AddressInUse);
        }

        Ok(DeviceConfig {
            slot,
            kind,
            bus: 0,
            address,
        })
    }

    fn insert(&mut self, config: DeviceConfig) {
        self.entries[(config.slot - 1) as usize] = Some(config);
    }

    fn remove(&mut self, slot: u8) -> Result<DeviceConfig, DeviceError> {
        let index = slot_index(slot)?;
        let config = self.entries[index].take().ok_or(DeviceError::SlotEmpty)?;
        self.battery_capacity_mah[index] = None;
        self.bme688_calibration[index] = None;
        Ok(config)
    }

    fn set_battery_capacity(&mut self, slot: u8, capacity_mah: u16) {
        self.battery_capacity_mah[(slot - 1) as usize] = Some(capacity_mah);
    }

    fn battery_capacity(&self, slot: u8) -> Result<u16, DeviceError> {
        let config = self.get(slot)?;
        if config.kind != DeviceKind::Lc709203f {
            return Err(DeviceError::WrongDevice);
        }
        self.battery_capacity_mah[(slot - 1) as usize].ok_or(DeviceError::NotConfigured)
    }

    fn set_bme688_calibration(&mut self, slot: u8, calibration: crate::bme688::Calibration) {
        self.bme688_calibration[(slot - 1) as usize] = Some(calibration);
    }

    fn bme688_calibration(&self, slot: u8) -> Result<crate::bme688::Calibration, DeviceError> {
        let config = self.get(slot)?;
        if config.kind != DeviceKind::Bme688 {
            return Err(DeviceError::WrongDevice);
        }
        self.bme688_calibration[(slot - 1) as usize].ok_or(DeviceError::NotConfigured)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum DeviceError {
    InvalidSlot,
    SlotEmpty,
    SlotOccupied,
    InvalidAddress,
    AddressInUse,
    UnsupportedModel,
    WrongDevice,
    NotConfigured,
    InvalidCapacity,
    InvalidIdentity,
    MeasurementInvalid,
    Timeout,
    Bus,
}

fn slot_index(slot: u8) -> Result<usize, DeviceError> {
    if (1..=MAX_DEVICES as u8).contains(&slot) {
        Ok((slot - 1) as usize)
    } else {
        Err(DeviceError::InvalidSlot)
    }
}

fn map_vl53l4cd_error<E>(error: vl53l4cd::Error<E>) -> DeviceError {
    match error {
        vl53l4cd::Error::I2c(_) => DeviceError::Bus,
        vl53l4cd::Error::Timeout => DeviceError::Timeout,
        vl53l4cd::Error::InvalidArgument => DeviceError::InvalidIdentity,
        vl53l4cd::Error::Gpio => DeviceError::Bus,
    }
}

fn map_lc709203f_error<E>(error: crate::lc709203f::Error<E>) -> DeviceError {
    match error {
        crate::lc709203f::Error::I2c(_) | crate::lc709203f::Error::Crc => DeviceError::Bus,
        crate::lc709203f::Error::InvalidIdentity => DeviceError::InvalidIdentity,
        crate::lc709203f::Error::InvalidCapacity => DeviceError::InvalidCapacity,
    }
}

fn map_bme688_error<E>(error: crate::bme688::Error<E>) -> DeviceError {
    match error {
        crate::bme688::Error::I2c(_) => DeviceError::Bus,
        crate::bme688::Error::InvalidIdentity => DeviceError::InvalidIdentity,
        crate::bme688::Error::MeasurementInvalid => DeviceError::MeasurementInvalid,
    }
}

fn map_seesaw_encoder_error<E>(error: crate::seesaw_encoder::Error<E>) -> DeviceError {
    match error {
        crate::seesaw_encoder::Error::I2c(_) => DeviceError::Bus,
        crate::seesaw_encoder::Error::InvalidIdentity => DeviceError::InvalidIdentity,
    }
}

pub(crate) async fn add<I2C>(
    bus: &mut I2C,
    registry: &mut DeviceRegistry,
    slot: u8,
    kind: DeviceKind,
    address: u8,
) -> Result<DeviceConfig, DeviceError>
where
    I2C: I2c,
{
    let config = registry.validate_add(slot, kind, address)?;

    match kind {
        DeviceKind::Amg8833 => {
            crate::amg8833::initialize(bus, address)
                .await
                .map_err(|_| DeviceError::Bus)?;
        }
        DeviceKind::Bme688 => {
            if address != crate::bme688::PRIMARY_ADDRESS
                && address != crate::bme688::SECONDARY_ADDRESS
            {
                return Err(DeviceError::InvalidAddress);
            }
            let calibration = crate::bme688::initialize(bus, address)
                .await
                .map_err(map_bme688_error)?;
            registry.set_bme688_calibration(slot, calibration);
        }
        DeviceKind::Lc709203f => {
            if address != crate::lc709203f::ADDRESS {
                return Err(DeviceError::InvalidAddress);
            }
            crate::lc709203f::initialize(bus, address)
                .await
                .map_err(map_lc709203f_error)?;
        }
        DeviceKind::Pct2075 => {
            crate::pct2075::initialize(bus, address)
                .await
                .map_err(|_| DeviceError::Bus)?;
        }
        DeviceKind::SeesawEncoder => {
            if !(crate::seesaw_encoder::FIRST_ADDRESS..=crate::seesaw_encoder::LAST_ADDRESS)
                .contains(&address)
            {
                return Err(DeviceError::InvalidAddress);
            }
            crate::seesaw_encoder::initialize(bus, address)
                .await
                .map_err(map_seesaw_encoder_error)?;
        }
        DeviceKind::Vl53l4cd => {
            let mut sensor = Vl53l4cd::with_addr(&mut *bus, address, Delay, Poll);
            sensor.init().await.map_err(map_vl53l4cd_error)?;
            sensor.start_ranging().await.map_err(map_vl53l4cd_error)?;
        }
    }

    registry.insert(config);
    Ok(config)
}

pub(crate) async fn remove<I2C>(
    bus: &mut I2C,
    registry: &mut DeviceRegistry,
    slot: u8,
) -> Result<DeviceConfig, DeviceError>
where
    I2C: I2c,
{
    let config = registry.get(slot)?;

    match config.kind {
        DeviceKind::Amg8833 => {
            crate::amg8833::sleep(bus, config.address)
                .await
                .map_err(|_| DeviceError::Bus)?;
        }
        DeviceKind::Bme688 => {
            crate::bme688::sleep(bus, config.address)
                .await
                .map_err(map_bme688_error)?;
        }
        DeviceKind::Lc709203f => {
            crate::lc709203f::sleep(bus, config.address)
                .await
                .map_err(map_lc709203f_error)?;
        }
        DeviceKind::Pct2075 => {
            crate::pct2075::sleep(bus, config.address)
                .await
                .map_err(|_| DeviceError::Bus)?;
        }
        DeviceKind::SeesawEncoder => {
            crate::seesaw_encoder::deinitialize(bus, config.address)
                .await
                .map_err(map_seesaw_encoder_error)?;
        }
        DeviceKind::Vl53l4cd => {
            let mut sensor = Vl53l4cd::with_addr(&mut *bus, config.address, Delay, Poll);
            sensor.stop_ranging().await.map_err(map_vl53l4cd_error)?;
        }
    }

    registry.remove(slot)
}

pub(crate) async fn clear<I2C>(
    bus: &mut I2C,
    registry: &mut DeviceRegistry,
) -> Result<(), DeviceError>
where
    I2C: I2c,
{
    for slot in 1..=MAX_DEVICES as u8 {
        if registry.get(slot).is_ok() {
            remove(bus, registry, slot).await?;
        }
    }
    Ok(())
}

pub(crate) async fn measure_distance<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<u16, DeviceError>
where
    I2C: I2c,
{
    let config = registry.get(slot)?;

    match config.kind {
        DeviceKind::Amg8833
        | DeviceKind::Bme688
        | DeviceKind::Lc709203f
        | DeviceKind::Pct2075
        | DeviceKind::SeesawEncoder => Err(DeviceError::WrongDevice),
        DeviceKind::Vl53l4cd => {
            let mut sensor = Vl53l4cd::with_addr(&mut *bus, config.address, Delay, Poll);
            if sensor.has_measurement().await.map_err(map_vl53l4cd_error)? {
                sensor.clear_interrupt().await.map_err(map_vl53l4cd_error)?;
            }

            for _ in 0..DISTANCE_MEASUREMENT_ATTEMPTS {
                let measurement = sensor.measure().await.map_err(map_vl53l4cd_error)?;
                if measurement.is_valid() && measurement.distance != 0 {
                    return Ok(measurement.distance);
                }
            }
            Err(DeviceError::MeasurementInvalid)
        }
    }
}

pub(crate) async fn measure_thermal_frame<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<[i16; crate::amg8833::PIXEL_COUNT], DeviceError>
where
    I2C: I2c,
{
    let config = registry.get(slot)?;
    match config.kind {
        DeviceKind::Amg8833 => crate::amg8833::read_frame(bus, config.address)
            .await
            .map_err(|_| DeviceError::Bus),
        DeviceKind::Bme688
        | DeviceKind::Lc709203f
        | DeviceKind::Pct2075
        | DeviceKind::SeesawEncoder
        | DeviceKind::Vl53l4cd => Err(DeviceError::WrongDevice),
    }
}

pub(crate) async fn measure_external_temperature<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<f32, DeviceError>
where
    I2C: I2c,
{
    let config = registry.get(slot)?;
    match config.kind {
        DeviceKind::Bme688 => measure_environment(bus, registry, slot)
            .await
            .map(|measurement| measurement.temperature_c),
        DeviceKind::Pct2075 => crate::pct2075::read_temperature_eighths(bus, config.address)
            .await
            .map(|eighths| f32::from(eighths) * 0.125)
            .map_err(|_| DeviceError::Bus),
        DeviceKind::Amg8833
        | DeviceKind::Lc709203f
        | DeviceKind::SeesawEncoder
        | DeviceKind::Vl53l4cd => Err(DeviceError::WrongDevice),
    }
}

pub(crate) async fn measure_environment<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<crate::bme688::Measurement, DeviceError>
where
    I2C: I2c,
{
    let config = registry.get(slot)?;
    let calibration = registry.bme688_calibration(slot)?;
    crate::bme688::measure(bus, config.address, calibration)
        .await
        .map_err(map_bme688_error)
}

fn encoder_config(registry: &DeviceRegistry, slot: u8) -> Result<DeviceConfig, DeviceError> {
    let config = registry.get(slot)?;
    if config.kind != DeviceKind::SeesawEncoder {
        return Err(DeviceError::WrongDevice);
    }
    Ok(config)
}

pub(crate) async fn encoder_position<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<i32, DeviceError>
where
    I2C: I2c,
{
    let config = encoder_config(registry, slot)?;
    crate::seesaw_encoder::position(bus, config.address)
        .await
        .map_err(map_seesaw_encoder_error)
}

pub(crate) async fn set_encoder_position<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
    position: i32,
) -> Result<(), DeviceError>
where
    I2C: I2c,
{
    let config = encoder_config(registry, slot)?;
    crate::seesaw_encoder::set_position(bus, config.address, position)
        .await
        .map_err(map_seesaw_encoder_error)
}

pub(crate) async fn encoder_delta<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<i32, DeviceError>
where
    I2C: I2c,
{
    let config = encoder_config(registry, slot)?;
    crate::seesaw_encoder::delta(bus, config.address)
        .await
        .map_err(map_seesaw_encoder_error)
}

pub(crate) async fn encoder_button<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<bool, DeviceError>
where
    I2C: I2c,
{
    let config = encoder_config(registry, slot)?;
    crate::seesaw_encoder::button_pressed(bus, config.address)
        .await
        .map_err(map_seesaw_encoder_error)
}

pub(crate) async fn set_battery_capacity<I2C>(
    bus: &mut I2C,
    registry: &mut DeviceRegistry,
    slot: u8,
    capacity_mah: u16,
) -> Result<(), DeviceError>
where
    I2C: I2c,
{
    let config = registry.get(slot)?;
    if config.kind != DeviceKind::Lc709203f {
        return Err(DeviceError::WrongDevice);
    }
    crate::lc709203f::set_capacity(bus, config.address, capacity_mah)
        .await
        .map_err(map_lc709203f_error)?;
    registry.set_battery_capacity(slot, capacity_mah);
    Ok(())
}

pub(crate) fn battery_capacity(registry: &DeviceRegistry, slot: u8) -> Result<u16, DeviceError> {
    registry.battery_capacity(slot)
}

pub(crate) async fn measure_battery_voltage<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<u16, DeviceError>
where
    I2C: I2c,
{
    let config = registry.get(slot)?;
    if config.kind != DeviceKind::Lc709203f {
        return Err(DeviceError::WrongDevice);
    }
    crate::lc709203f::read_voltage_mv(bus, config.address)
        .await
        .map_err(map_lc709203f_error)
}

pub(crate) async fn measure_battery_soc<I2C>(
    bus: &mut I2C,
    registry: &DeviceRegistry,
    slot: u8,
) -> Result<u16, DeviceError>
where
    I2C: I2c,
{
    let config = registry.get(slot)?;
    let _ = registry.battery_capacity(slot)?;
    crate::lc709203f::read_soc_tenths(bus, config.address)
        .await
        .map_err(map_lc709203f_error)
}
