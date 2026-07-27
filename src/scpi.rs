use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::Peri;
use embassy_rp::adc::{Adc, Async, Channel, Config, InterruptHandler};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::Pull;
use embassy_rp::peripherals::{ADC, ADC_TEMP_SENSOR, PIN_26, PIN_27, PIN_28, PIN_29};
use embassy_time::{Duration, Timer};
use microscpi::{
    self as scpi, Adapter, Characters, ErrorCommands, ErrorQueue, Interface, StandardCommands,
    StaticErrorQueue, StatusCommands, StatusRegisters,
};

use crate::devices;

const CHANNEL_COUNT: usize = 4;
const DEFAULT_AVERAGE_COUNT: u16 = 16;
const MAX_AVERAGE_COUNT: u16 = 256;
const ADC_COUNTS: f32 = 4096.0;
const ADC_VREF: f32 = 3.3;
const SCPI_BUFFER_SIZE: usize = 1024;
const SOCKET_BUFFER_SIZE: usize = 1024;

struct DeviceListResponse(devices::DeviceList);
struct ThermalFrameResponse([i16; crate::amg8833::PIXEL_COUNT]);

impl scpi::Response for DeviceListResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        let mut first = true;
        for config in self.0.entries.iter().flatten() {
            if !first {
                output.write_char(';')?;
            }
            first = false;
            output.write_fmt(format_args!(
                "{},{},{},{}",
                config.slot,
                config.kind.name(),
                config.bus,
                config.address
            ))?;
        }
        if first {
            output.write_str("NONE")?;
        }
        Ok(())
    }
}

impl scpi::Response for ThermalFrameResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        for (index, quarters) in self.0.iter().enumerate() {
            if index > 0 {
                output.write_char(',')?;
            }
            (f32::from(*quarters) * 0.25).write_response(output)?;
        }
        Ok(())
    }
}

fn device_error(error: devices::DeviceError) -> scpi::Error {
    match error {
        devices::DeviceError::InvalidSlot
        | devices::DeviceError::InvalidAddress
        | devices::DeviceError::InvalidCapacity => scpi::Error::DataOutOfRange,
        devices::DeviceError::SlotEmpty
        | devices::DeviceError::SlotOccupied
        | devices::DeviceError::AddressInUse
        | devices::DeviceError::UnsupportedModel
        | devices::DeviceError::WrongDevice => scpi::Error::IllegalParameterValue,
        devices::DeviceError::NotConfigured => scpi::Error::SettingsConflict,
        devices::DeviceError::InvalidIdentity
        | devices::DeviceError::MeasurementInvalid
        | devices::DeviceError::Timeout
        | devices::DeviceError::Bus => scpi::Error::HardwareError,
    }
}

bind_interrupts!(struct AdcIrqs {
    ADC_IRQ_FIFO => InterruptHandler;
});

pub(crate) struct Hardware {
    adc: Peri<'static, ADC>,
    temp_sensor: Peri<'static, ADC_TEMP_SENSOR>,
    a0: Peri<'static, PIN_26>,
    a1: Peri<'static, PIN_27>,
    a2: Peri<'static, PIN_28>,
    a3: Peri<'static, PIN_29>,
}

impl Hardware {
    pub(crate) fn new(
        adc: Peri<'static, ADC>,
        temp_sensor: Peri<'static, ADC_TEMP_SENSOR>,
        a0: Peri<'static, PIN_26>,
        a1: Peri<'static, PIN_27>,
        a2: Peri<'static, PIN_28>,
        a3: Peri<'static, PIN_29>,
    ) -> Self {
        Self {
            adc,
            temp_sensor,
            a0,
            a1,
            a2,
            a3,
        }
    }

    pub(crate) fn spawn(self, spawner: Spawner, stack: Stack<'static>, serial: &'static str) {
        spawner.spawn(scpi_task(stack, serial, self).unwrap());
    }
}

struct ScpiInstrument {
    adc: Adc<'static, Async>,
    channels: [Channel<'static>; CHANNEL_COUNT],
    temp_sensor: Channel<'static>,
    average_count: u16,
    serial: &'static str,
    errors: StaticErrorQueue<8>,
    registers: StatusRegisters,
}

impl ScpiInstrument {
    fn new(hardware: Hardware, serial: &'static str) -> Self {
        Self {
            adc: Adc::new(hardware.adc, AdcIrqs, Config::default()),
            channels: [
                Channel::new_pin(hardware.a0, Pull::None),
                Channel::new_pin(hardware.a1, Pull::None),
                Channel::new_pin(hardware.a2, Pull::None),
                Channel::new_pin(hardware.a3, Pull::None),
            ],
            temp_sensor: Channel::new_temp_sensor(hardware.temp_sensor),
            average_count: DEFAULT_AVERAGE_COUNT,
            serial,
            errors: StaticErrorQueue::new(),
            registers: StatusRegisters::default(),
        }
    }

    async fn read_average(&mut self, channel: u8) -> Result<u16, scpi::Error> {
        let channel = self
            .channels
            .get_mut(channel as usize)
            .ok_or(scpi::Error::DataOutOfRange)?;
        Self::read_channel_average(&mut self.adc, channel, self.average_count).await
    }

    async fn read_temperature_average(&mut self) -> Result<u16, scpi::Error> {
        Self::read_channel_average(&mut self.adc, &mut self.temp_sensor, self.average_count).await
    }

    async fn read_channel_average(
        adc: &mut Adc<'static, Async>,
        channel: &mut Channel<'static>,
        average_count: u16,
    ) -> Result<u16, scpi::Error> {
        let mut sum = 0u32;
        let average_count = u32::from(average_count);

        for _ in 0..average_count {
            sum += adc
                .read(channel)
                .await
                .map_err(|_| scpi::Error::HardwareError)? as u32;
        }

        Ok(((sum + average_count / 2) / average_count) as u16)
    }
}

impl ErrorCommands for ScpiInstrument {
    fn error_queue(&mut self) -> &mut impl ErrorQueue {
        &mut self.errors
    }
}

impl StandardCommands for ScpiInstrument {}

impl StatusCommands for ScpiInstrument {
    fn status_registers(&mut self) -> &mut StatusRegisters {
        &mut self.registers
    }
}

#[scpi::interface(StandardCommands, ErrorCommands, StatusCommands)]
impl ScpiInstrument {
    #[scpi(cmd = "*IDN?")]
    async fn identify(
        &mut self,
    ) -> Result<
        (
            Characters<'_>,
            Characters<'_>,
            Characters<'_>,
            Characters<'_>,
        ),
        scpi::Error,
    > {
        Ok((
            Characters(crate::MANUFACTURER),
            Characters(crate::board::BOARD_NAME),
            Characters(self.serial),
            Characters(crate::FIRMWARE_VERSION),
        ))
    }

    #[scpi(cmd = "*RST")]
    async fn reset(&mut self) -> Result<(), scpi::Error> {
        self.errors.clear();
        self.registers = StatusRegisters::default();
        self.average_count = DEFAULT_AVERAGE_COUNT;
        Ok(())
    }

    #[scpi(cmd = "*TST?")]
    async fn self_test(&mut self) -> Result<i16, scpi::Error> {
        Ok(if self.adc.read(&mut self.temp_sensor).await.is_ok() {
            0
        } else {
            1
        })
    }

    #[scpi(cmd = "SYSTem:CHANnel:COUNt?")]
    async fn channel_count(&mut self) -> Result<usize, scpi::Error> {
        Ok(CHANNEL_COUNT)
    }

    #[scpi(cmd = "SYSTem:I2C:DEVice:CATalog?")]
    async fn i2c_device_catalog(&mut self) -> Result<Characters<'static>, scpi::Error> {
        Ok(Characters("AMG8833,LC709203F,PCT2075,VL53L4CD"))
    }

    #[scpi(cmd = "SYSTem:I2C:DEVice:ADD")]
    async fn i2c_device_add(
        &mut self,
        slot: u8,
        model: &str,
        address: u8,
    ) -> Result<(), scpi::Error> {
        let kind = devices::DeviceKind::from_name(model)
            .ok_or(devices::DeviceError::UnsupportedModel)
            .map_err(device_error)?;
        crate::i2c::device_add(slot, kind, address)
            .await
            .map_err(device_error)?;
        Ok(())
    }

    #[scpi(cmd = "SYSTem:I2C:DEVice?")]
    async fn i2c_device(
        &mut self,
        slot: u8,
    ) -> Result<(u8, Characters<'static>, u8, u8), scpi::Error> {
        let config = crate::i2c::device_get(slot).await.map_err(device_error)?;
        Ok((
            config.slot,
            Characters(config.kind.name()),
            config.bus,
            config.address,
        ))
    }

    #[scpi(cmd = "SYSTem:I2C:DEVice:LIST?")]
    async fn i2c_device_list(&mut self) -> Result<DeviceListResponse, scpi::Error> {
        crate::i2c::device_list()
            .await
            .map(DeviceListResponse)
            .map_err(device_error)
    }

    #[scpi(cmd = "SYSTem:I2C:DEVice:COUNt?")]
    async fn i2c_device_count(&mut self) -> Result<u8, scpi::Error> {
        crate::i2c::device_count().await.map_err(device_error)
    }

    #[scpi(cmd = "SYSTem:I2C:DEVice:DELete")]
    async fn i2c_device_delete(&mut self, slot: u8) -> Result<(), scpi::Error> {
        crate::i2c::device_remove(slot)
            .await
            .map_err(device_error)?;
        Ok(())
    }

    #[scpi(cmd = "SYSTem:I2C:DEVice:CLEar")]
    async fn i2c_device_clear(&mut self) -> Result<(), scpi::Error> {
        crate::i2c::device_clear().await.map_err(device_error)
    }

    #[scpi(cmd = "SENSe:AVERage:COUNt")]
    async fn set_average_count(&mut self, count: u16) -> Result<(), scpi::Error> {
        if !(1..=MAX_AVERAGE_COUNT).contains(&count) {
            return Err(scpi::Error::DataOutOfRange);
        }
        self.average_count = count;
        Ok(())
    }

    #[scpi(cmd = "SENSe:AVERage:COUNt?")]
    async fn average_count(&mut self) -> Result<u16, scpi::Error> {
        Ok(self.average_count)
    }

    #[scpi(cmd = "SENSe:BATTery:CAPacity")]
    async fn set_battery_capacity(
        &mut self,
        slot: u8,
        capacity_mah: u16,
    ) -> Result<(), scpi::Error> {
        crate::i2c::set_battery_capacity(slot, capacity_mah)
            .await
            .map_err(device_error)
    }

    #[scpi(cmd = "SENSe:BATTery:CAPacity?")]
    async fn battery_capacity(&mut self, slot: u8) -> Result<u16, scpi::Error> {
        crate::i2c::battery_capacity(slot)
            .await
            .map_err(device_error)
    }

    #[scpi(cmd = "MEASure:ADC:RAW?")]
    async fn measure_adc_raw(&mut self, channel: u8) -> Result<u16, scpi::Error> {
        self.read_average(channel).await
    }

    #[scpi(cmd = "MEASure:VOLTage:DC?")]
    async fn measure_voltage_dc(&mut self, channel: u8) -> Result<f32, scpi::Error> {
        let raw = self.read_average(channel).await?;
        Ok(raw as f32 * ADC_VREF / ADC_COUNTS)
    }

    #[scpi(cmd = "MEASure:TEMPerature?")]
    async fn measure_temperature(&mut self) -> Result<f32, scpi::Error> {
        let raw = self.read_temperature_average().await?;
        let volts = raw as f32 * ADC_VREF / ADC_COUNTS;
        Ok(27.0 - (volts - 0.706) / 0.001721)
    }

    #[scpi(cmd = "MEASure:TEMPerature:EXTernal?")]
    async fn measure_external_temperature(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        crate::i2c::measure_external_temperature(slot)
            .await
            .map(|eighths| f32::from(eighths) * 0.125)
            .map_err(device_error)
    }

    #[scpi(cmd = "MEASure:BATTery:VOLTage?")]
    async fn measure_battery_voltage(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        crate::i2c::measure_battery_voltage(slot)
            .await
            .map(|millivolts| f32::from(millivolts) / 1000.0)
            .map_err(device_error)
    }

    #[scpi(cmd = "MEASure:BATTery:SOC?")]
    async fn measure_battery_soc(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        crate::i2c::measure_battery_soc(slot)
            .await
            .map(|tenths| f32::from(tenths) / 10.0)
            .map_err(device_error)
    }

    #[scpi(cmd = "MEASure:DISTance?")]
    async fn measure_distance(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        match crate::i2c::measure_distance(slot).await {
            Ok(millimeters) => Ok(f32::from(millimeters) / 1000.0),
            Err(
                error @ (devices::DeviceError::MeasurementInvalid
                | devices::DeviceError::Timeout
                | devices::DeviceError::Bus),
            ) => {
                self.errors.push_error(device_error(error));
                Ok(f32::NAN)
            }
            Err(error) => Err(device_error(error)),
        }
    }

    #[scpi(cmd = "MEASure:THERMal:PIXel?")]
    async fn measure_thermal_pixel(&mut self, slot: u8, pixel: u8) -> Result<f32, scpi::Error> {
        let frame = crate::i2c::measure_thermal_frame(slot)
            .await
            .map_err(device_error)?;
        let quarters = frame
            .get(usize::from(pixel))
            .ok_or(scpi::Error::DataOutOfRange)?;
        Ok(f32::from(*quarters) * 0.25)
    }

    #[scpi(cmd = "MEASure:THERMal:MINimum?")]
    async fn measure_thermal_minimum(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        let frame = crate::i2c::measure_thermal_frame(slot)
            .await
            .map_err(device_error)?;
        let quarters = frame.iter().min().ok_or(scpi::Error::HardwareError)?;
        Ok(f32::from(*quarters) * 0.25)
    }

    #[scpi(cmd = "MEASure:THERMal:MAXimum?")]
    async fn measure_thermal_maximum(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        let frame = crate::i2c::measure_thermal_frame(slot)
            .await
            .map_err(device_error)?;
        let quarters = frame.iter().max().ok_or(scpi::Error::HardwareError)?;
        Ok(f32::from(*quarters) * 0.25)
    }

    #[scpi(cmd = "MEASure:THERMal:AVERage?")]
    async fn measure_thermal_average(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        let frame = crate::i2c::measure_thermal_frame(slot)
            .await
            .map_err(device_error)?;
        let sum: i32 = frame.iter().map(|value| i32::from(*value)).sum();
        Ok(sum as f32 * 0.25 / crate::amg8833::PIXEL_COUNT as f32)
    }

    #[scpi(cmd = "READ:THERMal:ARRay?")]
    async fn read_thermal_array(&mut self, slot: u8) -> Result<ThermalFrameResponse, scpi::Error> {
        crate::i2c::measure_thermal_frame(slot)
            .await
            .map(ThermalFrameResponse)
            .map_err(device_error)
    }
}

#[derive(Clone, Copy)]
enum TransportError {
    Disconnected,
    Network,
}

struct TcpAdapter<'socket, 'buffer> {
    socket: &'socket mut TcpSocket<'buffer>,
}

impl Adapter for TcpAdapter<'_, '_> {
    type Error = TransportError;

    async fn read(&mut self, dst: &mut [u8]) -> Result<usize, Self::Error> {
        let count = self
            .socket
            .read(dst)
            .await
            .map_err(|_| TransportError::Network)?;
        if count == 0 {
            Err(TransportError::Disconnected)
        } else {
            Ok(count)
        }
    }

    async fn write(&mut self, mut src: &[u8]) -> Result<(), Self::Error> {
        while !src.is_empty() {
            let count = self
                .socket
                .write(src)
                .await
                .map_err(|_| TransportError::Network)?;
            if count == 0 {
                return Err(TransportError::Disconnected);
            }
            src = &src[count..];
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.socket
            .flush()
            .await
            .map_err(|_| TransportError::Network)
    }
}

#[embassy_executor::task]
async fn scpi_task(stack: Stack<'static>, serial: &'static str, hardware: Hardware) {
    let mut instrument = ScpiInstrument::new(hardware, serial);
    let mut rx_buf = [0; SOCKET_BUFFER_SIZE];
    let mut tx_buf = [0; SOCKET_BUFFER_SIZE];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);

    loop {
        socket.set_timeout(None);
        socket.set_keep_alive(Some(Duration::from_secs(10)));
        socket.set_nagle_enabled(false);

        if socket.accept(crate::SCPI_PORT).await.is_ok() {
            info!("SCPI client connected");
            let result = {
                let mut adapter = TcpAdapter {
                    socket: &mut socket,
                };
                instrument
                    .process::<SCPI_BUFFER_SIZE, _>(&mut adapter)
                    .await
            };

            match result {
                Err(TransportError::Disconnected) => info!("SCPI client disconnected"),
                Err(TransportError::Network) => warn!("SCPI transport error"),
                Ok(()) => {}
            }
        }

        socket.abort();
        let _ = socket.flush().await;
        Timer::after(Duration::from_millis(20)).await;
    }
}
