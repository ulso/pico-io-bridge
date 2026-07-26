use embassy_time::Delay;
use embedded_hal_async::i2c::I2c;
use vl53l4cd::{Vl53l4cd, wait::Poll};

pub(crate) const MAX_DEVICES: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceKind {
    Vl53l4cd,
}

impl DeviceKind {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Vl53l4cd => "VL53L4CD",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("VL53L4CD") {
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
}

impl DeviceRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [None; MAX_DEVICES],
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
        self.entries[index].take().ok_or(DeviceError::SlotEmpty)
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
        DeviceKind::Vl53l4cd => {
            let mut sensor = Vl53l4cd::with_addr(&mut *bus, config.address, Delay, Poll);
            let measurement = sensor.measure().await.map_err(map_vl53l4cd_error)?;
            if measurement.is_valid() && measurement.distance != 0 {
                Ok(measurement.distance)
            } else {
                Err(DeviceError::MeasurementInvalid)
            }
        }
    }
}
