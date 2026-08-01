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
    self as scpi, Adapter, Characters, ErrorCommands, ErrorQueue, Interface, Response as _,
    StandardCommands, StaticErrorQueue, StatusCommands, StatusRegisters,
};

use crate::devices;

const CHANNEL_COUNT: usize = 4;
const DEFAULT_AVERAGE_COUNT: u16 = 16;
const MAX_AVERAGE_COUNT: u16 = 256;
const ADC_COUNTS: f32 = 4096.0;
const ADC_VREF: f32 = 3.3;
const SCPI_BUFFER_SIZE: usize = 1024;
const SOCKET_BUFFER_SIZE: usize = 1024;
const USB_HOST_DIAGNOSTIC_PREFIX_CAPACITY: usize = 8;

struct DeviceListResponse(devices::DeviceList);
struct EnvironmentResponse(crate::bme688::Measurement);
struct ImuVectorResponse(crate::bno08x::Vector3);
struct ImuQuaternionResponse(crate::bno08x::Quaternion);
struct ImuResponse(crate::bno08x::Measurement);
struct ThermalFrameResponse([i16; crate::amg8833::PIXEL_COUNT]);
struct UsbHostStatusResponse {
    phase: &'static str,
    speed: &'static str,
    address: u8,
    vendor_id: u16,
    product_id: u16,
    rx_bytes: u32,
    tx_bytes: u32,
    error_count: u32,
    max_transfer: usize,
    unexpected_toggle_count: u32,
    accepted_zlp_count: u32,
    latest_expected_pid: Option<u8>,
    latest_actual_pid: Option<u8>,
    latest_payload_len: Option<u8>,
    latest_payload_prefix_len: u8,
    latest_payload_prefix: [u8; USB_HOST_DIAGNOSTIC_PREFIX_CAPACITY],
}
struct UsbHostEnumerationDiagnosticResponse {
    attempts: u32,
    failures: u32,
    origin: &'static str,
    error: &'static str,
    site: &'static str,
    handshake: &'static str,
    setup_attempts: u32,
    setup: [u8; 8],
}
struct UsbHostDataResponse {
    length: u8,
    data: [u8; crate::USB_HOST_CDC_MAX_TRANSFER],
}
struct BleuioCatalogResponse(crate::bleuio::Catalog);
struct BleuioFilterResponse(crate::bleuio::Filter);
struct BleuioSensorIdResponse(u32);
struct BleuioSensorDataResponse {
    sensor: crate::bleuio::Sensor,
    now_ms: u64,
}
struct P8055InputResponse {
    digital_inputs: u8,
    analog_input_1: u8,
    analog_input_2: u8,
    counter_1: u16,
    counter_2: u16,
}
struct P8055OutputResponse {
    digital_outputs: u8,
    analog_output_1: u8,
    analog_output_2: u8,
}

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

impl scpi::Response for UsbHostStatusResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        output.write_str(self.phase)?;
        output.write_char(',')?;
        output.write_str(self.speed)?;
        output.write_fmt(format_args!(
            ",{},{},{},{},{},{},{},{},{}",
            self.address,
            self.vendor_id,
            self.product_id,
            self.rx_bytes,
            self.tx_bytes,
            self.error_count,
            self.max_transfer,
            self.unexpected_toggle_count,
            self.accepted_zlp_count
        ))?;
        match (
            self.latest_expected_pid,
            self.latest_actual_pid,
            self.latest_payload_len,
        ) {
            (Some(expected_pid), Some(actual_pid), Some(payload_len)) => {
                output.write_fmt(format_args!(
                    ",{expected_pid:02X},{actual_pid:02X},{payload_len},"
                ))?;
                let prefix_len = usize::from(self.latest_payload_prefix_len)
                    .min(self.latest_payload_prefix.len());
                if prefix_len == 0 {
                    output.write_str("EMPTY")?;
                } else {
                    for byte in &self.latest_payload_prefix[..prefix_len] {
                        output.write_fmt(format_args!("{byte:02X}"))?;
                    }
                }
            }
            _ => output.write_str(",NONE,NONE,NONE,NONE")?,
        }
        Ok(())
    }
}

impl UsbHostStatusResponse {
    #[cfg(feature = "board-adafruit-rp2040-usb-host")]
    fn from_status(status: crate::usb_host::Status) -> Self {
        Self {
            phase: status.phase.as_str(),
            speed: match status.speed {
                Some(speed) => speed.as_str(),
                None => "NONE",
            },
            address: status.address,
            vendor_id: status.vendor_id,
            product_id: status.product_id,
            rx_bytes: status.rx_bytes,
            tx_bytes: status.tx_bytes,
            error_count: status.error_count,
            max_transfer: match status.speed {
                Some(crate::usb_host::HostSpeed::Low) => crate::p8055::REPORT_LEN,
                Some(crate::usb_host::HostSpeed::Full) | None => crate::USB_HOST_CDC_MAX_TRANSFER,
            },
            unexpected_toggle_count: status.unexpected_toggle_count,
            accepted_zlp_count: status.accepted_zlp_count,
            latest_expected_pid: status.latest_expected_pid,
            latest_actual_pid: status.latest_actual_pid,
            latest_payload_len: status.latest_payload_len,
            latest_payload_prefix_len: status.latest_payload_prefix_len,
            latest_payload_prefix: status.latest_payload_prefix,
        }
    }
}

impl scpi::Response for UsbHostEnumerationDiagnosticResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        output.write_fmt(format_args!(
            "{},{},{},{},{},{},{}",
            self.attempts,
            self.failures,
            self.origin,
            self.error,
            self.site,
            self.handshake,
            self.setup_attempts
        ))?;
        for byte in self.setup {
            output.write_fmt(format_args!(",{byte:02X}"))?;
        }
        Ok(())
    }
}

impl UsbHostEnumerationDiagnosticResponse {
    #[cfg(feature = "board-adafruit-rp2040-usb-host")]
    fn from_diagnostic(diagnostic: crate::usb_host::EnumerationDiagnostic) -> Self {
        Self {
            attempts: diagnostic.attempts(),
            failures: diagnostic.failures(),
            origin: diagnostic.origin(),
            error: diagnostic.error(),
            site: diagnostic.site(),
            handshake: diagnostic.handshake(),
            setup_attempts: diagnostic.setup_attempts(),
            setup: diagnostic.setup(),
        }
    }
}

impl scpi::Response for UsbHostDataResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        for byte in &self.data[..usize::from(self.length)] {
            output.write_fmt(format_args!("{byte:02X}"))?;
        }
        Ok(())
    }
}

impl UsbHostDataResponse {
    #[cfg(feature = "board-adafruit-rp2040-usb-host")]
    fn from_cdc(data: crate::usb_host::CdcData) -> Self {
        let mut response = Self {
            length: data.as_bytes().len() as u8,
            data: [0; crate::USB_HOST_CDC_MAX_TRANSFER],
        };
        response.data[..data.as_bytes().len()].copy_from_slice(data.as_bytes());
        response
    }
}

impl scpi::Response for BleuioCatalogResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        let mut first = true;
        for sensor in self.0.sensors.iter().flatten() {
            if !first {
                output.write_char(',')?;
            }
            first = false;
            output.write_fmt(format_args!("{:06X}", sensor.reading.board_id))?;
        }
        if first {
            output.write_str("NONE")?;
        }
        Ok(())
    }
}

impl scpi::Response for BleuioFilterResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        if self.0.is_all() {
            return output.write_str("ALL");
        }
        let mut first = true;
        for board_id in self.0.ids.iter().flatten() {
            if !first {
                output.write_char(',')?;
            }
            first = false;
            output.write_fmt(format_args!("{board_id:06X}"))?;
        }
        Ok(())
    }
}

impl scpi::Response for BleuioSensorIdResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        output.write_fmt(format_args!("{:06X}", self.0))
    }
}

fn write_bleuio_optional(
    output: &mut impl scpi::Write,
    value: Option<f32>,
) -> Result<(), scpi::Error> {
    match value {
        Some(value) => value.write_response(output),
        None => output.write_str("NAN"),
    }
}

impl scpi::Response for BleuioSensorDataResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        let reading = self.sensor.reading;
        let sensor_type = reading.sensor_type;
        output.write_fmt(format_args!(
            "{:06X},{},{},{}",
            reading.board_id,
            sensor_type.token(),
            self.now_ms.saturating_sub(self.sensor.last_seen_ms),
            self.sensor.reports,
        ))?;
        for value in [
            Some(f32::from(reading.temperature_tenths_c) / 10.0),
            Some(f32::from(reading.humidity_tenths_percent) / 10.0),
            Some(f32::from(reading.pressure_tenths_hpa) / 10.0),
            sensor_type.has_co2().then_some(f32::from(reading.co2_ppm)),
            Some(f32::from(reading.voc_raw)),
            Some(f32::from(reading.voc_type)),
            sensor_type
                .has_noise()
                .then_some(f32::from(reading.noise_db_spl)),
            sensor_type
                .has_particulate()
                .then_some(f32::from(reading.pm1_tenths) / 10.0),
            sensor_type
                .has_particulate()
                .then_some(f32::from(reading.pm25_tenths) / 10.0),
            sensor_type
                .has_particulate()
                .then_some(f32::from(reading.pm10_tenths) / 10.0),
            sensor_type
                .has_ambient_light()
                .then_some(f32::from(reading.ambient_light)),
        ] {
            output.write_char(',')?;
            write_bleuio_optional(output, value)?;
        }
        Ok(())
    }
}

impl scpi::Response for P8055InputResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        output.write_fmt(format_args!(
            "{},{},{},{},{}",
            self.digital_inputs,
            self.analog_input_1,
            self.analog_input_2,
            self.counter_1,
            self.counter_2
        ))
    }
}

impl P8055InputResponse {
    #[cfg(feature = "board-adafruit-rp2040-usb-host")]
    fn from_input(input: crate::p8055::InputReport) -> Self {
        Self {
            digital_inputs: input.digital_inputs(),
            analog_input_1: input.analog_input_1(),
            analog_input_2: input.analog_input_2(),
            counter_1: input.counter_1(),
            counter_2: input.counter_2(),
        }
    }
}

impl scpi::Response for P8055OutputResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        output.write_fmt(format_args!(
            "{},{},{}",
            self.digital_outputs, self.analog_output_1, self.analog_output_2
        ))
    }
}

impl P8055OutputResponse {
    #[cfg(feature = "board-adafruit-rp2040-usb-host")]
    fn from_output(output: crate::p8055::OutputState) -> Self {
        Self {
            digital_outputs: output.digital_outputs,
            analog_output_1: output.analog_output_1,
            analog_output_2: output.analog_output_2,
        }
    }
}

impl scpi::Response for EnvironmentResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        self.0.temperature_c.write_response(output)?;
        output.write_char(',')?;
        self.0.humidity_percent.write_response(output)?;
        output.write_char(',')?;
        self.0.pressure_pa.write_response(output)?;
        output.write_char(',')?;
        self.0.gas_resistance_ohm.write_response(output)
    }
}

fn write_imu_vector(
    vector: crate::bno08x::Vector3,
    output: &mut impl scpi::Write,
) -> Result<(), scpi::Error> {
    vector.x.write_response(output)?;
    output.write_char(',')?;
    vector.y.write_response(output)?;
    output.write_char(',')?;
    vector.z.write_response(output)?;
    output.write_char(',')?;
    vector.accuracy.write_response(output)
}

fn write_imu_quaternion(
    quaternion: crate::bno08x::Quaternion,
    output: &mut impl scpi::Write,
) -> Result<(), scpi::Error> {
    quaternion.i.write_response(output)?;
    output.write_char(',')?;
    quaternion.j.write_response(output)?;
    output.write_char(',')?;
    quaternion.k.write_response(output)?;
    output.write_char(',')?;
    quaternion.real.write_response(output)?;
    output.write_char(',')?;
    quaternion.accuracy_radians.write_response(output)?;
    output.write_char(',')?;
    quaternion.accuracy.write_response(output)
}

impl scpi::Response for ImuVectorResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        write_imu_vector(self.0, output)
    }
}

impl scpi::Response for ImuQuaternionResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        write_imu_quaternion(self.0, output)
    }
}

impl scpi::Response for ImuResponse {
    fn write_response(&self, output: &mut impl scpi::Write) -> Result<(), scpi::Error> {
        write_imu_vector(self.0.acceleration, output)?;
        output.write_char(',')?;
        write_imu_vector(self.0.gyroscope, output)?;
        output.write_char(',')?;
        write_imu_vector(self.0.magnetic_field, output)?;
        output.write_char(',')?;
        write_imu_quaternion(self.0.rotation, output)
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

fn imu_error(error: devices::DeviceError) -> scpi::Error {
    match error {
        devices::DeviceError::MeasurementInvalid => {
            scpi::Error::Custom(-301, "BNO08x protocol error")
        }
        devices::DeviceError::Timeout => scpi::Error::Custom(-302, "BNO08x timeout"),
        devices::DeviceError::Bus => scpi::Error::Custom(-303, "BNO08x I2C error"),
        error => device_error(error),
    }
}

#[cfg(feature = "board-adafruit-rp2040-usb-host")]
fn usb_host_error(error: crate::usb_host::Error) -> scpi::Error {
    match error {
        crate::usb_host::Error::InvalidLength => scpi::Error::DataOutOfRange,
        crate::usb_host::Error::InvalidHex => scpi::Error::InvalidStringData,
        crate::usb_host::Error::InvalidParameter => scpi::Error::DataOutOfRange,
        crate::usb_host::Error::DataStale => scpi::Error::DataCorruptOrStale,
        crate::usb_host::Error::NotReady | crate::usb_host::Error::ResourceBusy => {
            scpi::Error::SettingsConflict
        }
        crate::usb_host::Error::Timeout => scpi::Error::Custom(-310, "USB host timeout"),
        crate::usb_host::Error::Transfer => scpi::Error::Custom(-311, "USB host transfer error"),
        crate::usb_host::Error::Protocol => scpi::Error::Custom(-312, "P8055 protocol error"),
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
    selected_bleuio_sensor: Option<u32>,
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
            selected_bleuio_sensor: None,
        }
    }

    async fn bleuio_selected(&self) -> Result<crate::bleuio::Sensor, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            let board_id = self
                .selected_bleuio_sensor
                .ok_or(scpi::Error::SettingsConflict)?;
            crate::usb_host::bleuio_sensor(board_id)
                .await
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            Err(scpi::Error::SettingsConflict)
        }
    }

    async fn bleuio_catalog_response(&self) -> Result<BleuioCatalogResponse, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::bleuio_catalog()
                .await
                .map(BleuioCatalogResponse)
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            Err(scpi::Error::SettingsConflict)
        }
    }

    async fn bleuio_set_filter(&self, ids: &str) -> Result<(), scpi::Error> {
        let filter = crate::bleuio::Filter::parse(ids).ok_or(scpi::Error::DataOutOfRange)?;
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::bleuio_set_filter(filter).await;
            Ok(())
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = filter;
            Err(scpi::Error::SettingsConflict)
        }
    }

    async fn bleuio_filter_response(&self) -> Result<BleuioFilterResponse, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            Ok(BleuioFilterResponse(crate::usb_host::bleuio_filter().await))
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            Err(scpi::Error::SettingsConflict)
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

    async fn read_environment(
        &mut self,
        slot: u8,
    ) -> Result<crate::bme688::Measurement, scpi::Error> {
        match crate::i2c::measure_environment(slot).await {
            Ok(measurement) => Ok(measurement),
            Err(
                error @ (devices::DeviceError::MeasurementInvalid
                | devices::DeviceError::Timeout
                | devices::DeviceError::Bus),
            ) => {
                self.errors.push_error(device_error(error));
                Ok(crate::bme688::Measurement {
                    temperature_c: f32::NAN,
                    humidity_percent: f32::NAN,
                    pressure_pa: f32::NAN,
                    gas_resistance_ohm: f32::NAN,
                })
            }
            Err(error) => Err(device_error(error)),
        }
    }

    async fn read_imu(&mut self, slot: u8) -> Result<crate::bno08x::Measurement, scpi::Error> {
        match crate::i2c::measure_imu(slot).await {
            Ok(measurement) => Ok(measurement),
            Err(
                error @ (devices::DeviceError::MeasurementInvalid
                | devices::DeviceError::Timeout
                | devices::DeviceError::Bus),
            ) => {
                self.errors.push_error(imu_error(error));
                Ok(crate::bno08x::Measurement::invalid())
            }
            Err(error) => Err(imu_error(error)),
        }
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
        self.selected_bleuio_sensor = None;
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

    #[scpi(cmd = "SYSTem:RESet:CAUSe?")]
    async fn reset_cause(&mut self) -> Result<Characters<'static>, scpi::Error> {
        Ok(Characters(crate::network::reset_cause_label()))
    }

    #[scpi(cmd = "SYSTem:USB:HOST:STATus?")]
    async fn usb_host_status(&mut self) -> Result<UsbHostStatusResponse, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            Ok(UsbHostStatusResponse::from_status(
                crate::usb_host::status().await,
            ))
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:ENUMeration:DIAGnostic?")]
    async fn usb_host_enumeration_diagnostic(
        &mut self,
    ) -> Result<UsbHostEnumerationDiagnosticResponse, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            Ok(UsbHostEnumerationDiagnosticResponse::from_diagnostic(
                crate::usb_host::enumeration_diagnostic().await,
            ))
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:CATalog?")]
    async fn usb_host_bleuio_sensor_catalog(
        &mut self,
    ) -> Result<BleuioCatalogResponse, scpi::Error> {
        self.bleuio_catalog_response().await
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:FILTer")]
    async fn usb_host_bleuio_sensor_filter(&mut self, ids: &str) -> Result<(), scpi::Error> {
        self.bleuio_set_filter(ids).await
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:FILTer?")]
    async fn usb_host_bleuio_sensor_filter_query(
        &mut self,
    ) -> Result<BleuioFilterResponse, scpi::Error> {
        self.bleuio_filter_response().await
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:FILTer:CLEar")]
    async fn usb_host_bleuio_sensor_filter_clear(&mut self) -> Result<(), scpi::Error> {
        self.bleuio_set_filter("ALL").await
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:SELect")]
    async fn usb_host_bleuio_sensor_select(&mut self, id: &str) -> Result<(), scpi::Error> {
        self.selected_bleuio_sensor =
            Some(crate::bleuio::parse_board_id(id).ok_or(scpi::Error::DataOutOfRange)?);
        Ok(())
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:SELect?")]
    async fn usb_host_bleuio_sensor_select_query(
        &mut self,
    ) -> Result<BleuioSensorIdResponse, scpi::Error> {
        self.selected_bleuio_sensor
            .map(BleuioSensorIdResponse)
            .ok_or(scpi::Error::SettingsConflict)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:DATA?")]
    async fn usb_host_bleuio_sensor_data(
        &mut self,
    ) -> Result<BleuioSensorDataResponse, scpi::Error> {
        Ok(BleuioSensorDataResponse {
            sensor: self.bleuio_selected().await?,
            now_ms: embassy_time::Instant::now().as_millis(),
        })
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:TEMPerature?")]
    async fn usb_host_bleuio_sensor_temperature(&mut self) -> Result<f32, scpi::Error> {
        Ok(f32::from(self.bleuio_selected().await?.reading.temperature_tenths_c) / 10.0)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:HUMidity?")]
    async fn usb_host_bleuio_sensor_humidity(&mut self) -> Result<f32, scpi::Error> {
        Ok(f32::from(
            self.bleuio_selected()
                .await?
                .reading
                .humidity_tenths_percent,
        ) / 10.0)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:PRESsure?")]
    async fn usb_host_bleuio_sensor_pressure(&mut self) -> Result<f32, scpi::Error> {
        Ok(f32::from(self.bleuio_selected().await?.reading.pressure_tenths_hpa) / 10.0)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:CO2?")]
    async fn usb_host_bleuio_sensor_co2(&mut self) -> Result<u16, scpi::Error> {
        let reading = self.bleuio_selected().await?.reading;
        reading
            .sensor_type
            .has_co2()
            .then_some(reading.co2_ppm)
            .ok_or(scpi::Error::SettingsConflict)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:VOC?")]
    async fn usb_host_bleuio_sensor_voc(&mut self) -> Result<u16, scpi::Error> {
        Ok(self.bleuio_selected().await?.reading.voc_raw)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:NOISe?")]
    async fn usb_host_bleuio_sensor_noise(&mut self) -> Result<u16, scpi::Error> {
        let reading = self.bleuio_selected().await?.reading;
        reading
            .sensor_type
            .has_noise()
            .then_some(reading.noise_db_spl)
            .ok_or(scpi::Error::SettingsConflict)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:PM1?")]
    async fn usb_host_bleuio_sensor_pm1(&mut self) -> Result<f32, scpi::Error> {
        let reading = self.bleuio_selected().await?.reading;
        reading
            .sensor_type
            .has_particulate()
            .then_some(f32::from(reading.pm1_tenths) / 10.0)
            .ok_or(scpi::Error::SettingsConflict)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:PM25?")]
    async fn usb_host_bleuio_sensor_pm25(&mut self) -> Result<f32, scpi::Error> {
        let reading = self.bleuio_selected().await?.reading;
        reading
            .sensor_type
            .has_particulate()
            .then_some(f32::from(reading.pm25_tenths) / 10.0)
            .ok_or(scpi::Error::SettingsConflict)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:PM10?")]
    async fn usb_host_bleuio_sensor_pm10(&mut self) -> Result<f32, scpi::Error> {
        let reading = self.bleuio_selected().await?.reading;
        reading
            .sensor_type
            .has_particulate()
            .then_some(f32::from(reading.pm10_tenths) / 10.0)
            .ok_or(scpi::Error::SettingsConflict)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:LIGHt?")]
    async fn usb_host_bleuio_sensor_light(&mut self) -> Result<u16, scpi::Error> {
        let reading = self.bleuio_selected().await?.reading;
        reading
            .sensor_type
            .has_ambient_light()
            .then_some(reading.ambient_light)
            .ok_or(scpi::Error::SettingsConflict)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:AGE?")]
    async fn usb_host_bleuio_sensor_age(&mut self) -> Result<u32, scpi::Error> {
        let sensor = self.bleuio_selected().await?;
        Ok(embassy_time::Instant::now()
            .as_millis()
            .saturating_sub(sensor.last_seen_ms)
            .min(u64::from(u32::MAX)) as u32)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:BLEUio:SENSor:COUNt?")]
    async fn usb_host_bleuio_sensor_count(&mut self) -> Result<u32, scpi::Error> {
        Ok(self.bleuio_selected().await?.reports)
    }

    #[scpi(cmd = "SYSTem:USB:HOST:CDC:WRITe:HEX")]
    async fn usb_host_cdc_write_hex(&mut self, data: &str) -> Result<u8, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::cdc_write_hex(data)
                .await
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = data;
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:FTDI:BAUDrate")]
    async fn usb_host_ftdi_set_baud_rate(&mut self, baud_rate: u32) -> Result<(), scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::ftdi_set_baud_rate(baud_rate)
                .await
                .map(|_| ())
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = baud_rate;
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:FTDI:BAUDrate?")]
    async fn usb_host_ftdi_baud_rate(&mut self) -> Result<u32, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::ftdi_baud_rate()
                .await
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:CDC:READ:HEX?")]
    async fn usb_host_cdc_read_hex(
        &mut self,
        length: u8,
    ) -> Result<UsbHostDataResponse, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::cdc_read(length)
                .await
                .map(UsbHostDataResponse::from_cdc)
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = length;
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:CDC:EXCHange:HEX?")]
    async fn usb_host_cdc_exchange_hex(
        &mut self,
        data: &str,
        read_length: u8,
    ) -> Result<UsbHostDataResponse, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::cdc_exchange_hex(data, read_length)
                .await
                .map(UsbHostDataResponse::from_cdc)
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = (data, read_length);
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:P8055:INPut?")]
    async fn usb_host_p8055_input(&mut self) -> Result<P8055InputResponse, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::p8055_read_input()
                .await
                .map(P8055InputResponse::from_input)
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:P8055:OUTPut?")]
    async fn usb_host_p8055_output(&mut self) -> Result<P8055OutputResponse, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            crate::usb_host::p8055_get_output()
                .await
                .map(P8055OutputResponse::from_output)
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:P8055:OUTPut")]
    async fn usb_host_p8055_set_output(
        &mut self,
        digital_outputs: u16,
        analog_output_1: u16,
        analog_output_2: u16,
    ) -> Result<(), scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            let digital_outputs =
                u8::try_from(digital_outputs).map_err(|_| scpi::Error::DataOutOfRange)?;
            let analog_output_1 =
                u8::try_from(analog_output_1).map_err(|_| scpi::Error::DataOutOfRange)?;
            let analog_output_2 =
                u8::try_from(analog_output_2).map_err(|_| scpi::Error::DataOutOfRange)?;
            crate::usb_host::p8055_set_output(digital_outputs, analog_output_1, analog_output_2)
                .await
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = (digital_outputs, analog_output_1, analog_output_2);
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:P8055:COUNter:RESet")]
    async fn usb_host_p8055_reset_counter(&mut self, channel: u16) -> Result<(), scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            let channel = u8::try_from(channel).map_err(|_| scpi::Error::DataOutOfRange)?;
            crate::usb_host::p8055_reset_counter(channel)
                .await
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = channel;
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:P8055:COUNter:DEBounce")]
    async fn usb_host_p8055_set_debounce(
        &mut self,
        channel: u16,
        microseconds: u32,
    ) -> Result<(), scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            let channel = u8::try_from(channel).map_err(|_| scpi::Error::DataOutOfRange)?;
            crate::usb_host::p8055_set_debounce(channel, microseconds)
                .await
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = (channel, microseconds);
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:USB:HOST:P8055:COUNter:DEBounce?")]
    async fn usb_host_p8055_debounce(&mut self, channel: u16) -> Result<u32, scpi::Error> {
        #[cfg(feature = "board-adafruit-rp2040-usb-host")]
        {
            let channel = u8::try_from(channel).map_err(|_| scpi::Error::DataOutOfRange)?;
            crate::usb_host::p8055_get_debounce(channel)
                .await
                .map_err(usb_host_error)
        }
        #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
        {
            let _ = channel;
            Err(scpi::Error::SettingsConflict)
        }
    }

    #[scpi(cmd = "SYSTem:I2C:DEVice:CATalog?")]
    async fn i2c_device_catalog(&mut self) -> Result<Characters<'static>, scpi::Error> {
        Ok(Characters(
            "AMG8833,BME688,BNO08X,LC709203F,PCT2075,SEESAW_ENCODER,VL53L4CD",
        ))
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

    #[scpi(cmd = "SENSe:ENCoder:POSition")]
    async fn set_encoder_position(&mut self, slot: u8, position: i32) -> Result<(), scpi::Error> {
        crate::i2c::set_encoder_position(slot, position)
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
        let config = crate::i2c::device_get(slot).await.map_err(device_error)?;
        if config.kind == devices::DeviceKind::Bme688 {
            self.read_environment(slot)
                .await
                .map(|measurement| measurement.temperature_c)
        } else {
            crate::i2c::measure_external_temperature(slot)
                .await
                .map_err(device_error)
        }
    }

    #[scpi(cmd = "MEASure:HUMidity?")]
    async fn measure_humidity(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        self.read_environment(slot)
            .await
            .map(|measurement| measurement.humidity_percent)
    }

    #[scpi(cmd = "MEASure:PRESsure?")]
    async fn measure_pressure(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        self.read_environment(slot)
            .await
            .map(|measurement| measurement.pressure_pa)
    }

    #[scpi(cmd = "MEASure:GAS:RESistance?")]
    async fn measure_gas_resistance(&mut self, slot: u8) -> Result<f32, scpi::Error> {
        self.read_environment(slot)
            .await
            .map(|measurement| measurement.gas_resistance_ohm)
    }

    #[scpi(cmd = "READ:ENVironment?")]
    async fn read_environment_values(
        &mut self,
        slot: u8,
    ) -> Result<EnvironmentResponse, scpi::Error> {
        self.read_environment(slot).await.map(EnvironmentResponse)
    }

    #[scpi(cmd = "MEASure:IMU:ACCeleration?")]
    async fn measure_imu_acceleration(
        &mut self,
        slot: u8,
    ) -> Result<ImuVectorResponse, scpi::Error> {
        self.read_imu(slot)
            .await
            .map(|measurement| ImuVectorResponse(measurement.acceleration))
    }

    #[scpi(cmd = "MEASure:IMU:GYRoscope?")]
    async fn measure_imu_gyroscope(&mut self, slot: u8) -> Result<ImuVectorResponse, scpi::Error> {
        self.read_imu(slot)
            .await
            .map(|measurement| ImuVectorResponse(measurement.gyroscope))
    }

    #[scpi(cmd = "MEASure:IMU:MAGNetic?")]
    async fn measure_imu_magnetic(&mut self, slot: u8) -> Result<ImuVectorResponse, scpi::Error> {
        self.read_imu(slot)
            .await
            .map(|measurement| ImuVectorResponse(measurement.magnetic_field))
    }

    #[scpi(cmd = "MEASure:IMU:QUATernion?")]
    async fn measure_imu_quaternion(
        &mut self,
        slot: u8,
    ) -> Result<ImuQuaternionResponse, scpi::Error> {
        self.read_imu(slot)
            .await
            .map(|measurement| ImuQuaternionResponse(measurement.rotation))
    }

    #[scpi(cmd = "READ:IMU?")]
    async fn read_imu_values(&mut self, slot: u8) -> Result<ImuResponse, scpi::Error> {
        self.read_imu(slot).await.map(ImuResponse)
    }

    #[scpi(cmd = "MEASure:ENCoder:POSition?")]
    async fn measure_encoder_position(&mut self, slot: u8) -> Result<i32, scpi::Error> {
        crate::i2c::encoder_position(slot)
            .await
            .map_err(device_error)
    }

    #[scpi(cmd = "MEASure:ENCoder:DELTa?")]
    async fn measure_encoder_delta(&mut self, slot: u8) -> Result<i32, scpi::Error> {
        crate::i2c::encoder_delta(slot).await.map_err(device_error)
    }

    #[scpi(cmd = "MEASure:ENCoder:BUTTon?")]
    async fn measure_encoder_button(&mut self, slot: u8) -> Result<u8, scpi::Error> {
        crate::i2c::encoder_button(slot)
            .await
            .map(u8::from)
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

#[cfg(test)]
mod tests {
    #[cfg(feature = "board-adafruit-rp2040-usb-host")]
    use super::{BleuioFilterResponse, BleuioSensorIdResponse};
    use super::{SCPI_NODE_0, UsbHostEnumerationDiagnosticResponse};
    use microscpi::{self as scpi, Response as _};

    fn query_id(input: &[u8]) -> scpi::CommandId {
        let (remaining, call) =
            scpi::parser::parse(&SCPI_NODE_0, &SCPI_NODE_0, input).expect("valid SCPI query");
        assert!(remaining.is_empty());
        let call = call.expect("query call");
        assert!(call.query);
        assert!(call.terminated);
        assert!(call.args.is_empty());
        call.node.query.expect("registered query")
    }

    fn command_id(input: &[u8]) -> scpi::CommandId {
        let (remaining, call) =
            scpi::parser::parse(&SCPI_NODE_0, &SCPI_NODE_0, input).expect("valid SCPI command");
        assert!(remaining.is_empty());
        let call = call.expect("command call");
        assert!(!call.query);
        assert!(call.terminated);
        call.node.command.expect("registered command")
    }

    #[test]
    fn usb_host_enumeration_diagnostic_response_is_stable_csv() {
        let response = UsbHostEnumerationDiagnosticResponse {
            attempts: 3,
            failures: 2,
            origin: "ENUMERATE",
            error: "BAD_RESPONSE",
            site: "CONTROL_IN_SETUP",
            handshake: "INVALID_PID_COMPLEMENT",
            setup_attempts: 2,
            setup: [0x80, 0x06, 0, 1, 0, 0, 8, 0],
        };
        let mut output = heapless::Vec::<u8, 128>::new();

        scpi::Response::write_response(&response, &mut output).unwrap();

        assert_eq!(
            output.as_slice(),
            b"3,2,ENUMERATE,BAD_RESPONSE,CONTROL_IN_SETUP,INVALID_PID_COMPLEMENT,2,80,06,00,01,00,00,08,00"
        );

        let empty = UsbHostEnumerationDiagnosticResponse {
            attempts: 0,
            failures: 0,
            origin: "NONE",
            error: "NONE",
            site: "NONE",
            handshake: "NONE",
            setup_attempts: 0,
            setup: [0; 8],
        };
        output.clear();
        empty.write_response(&mut output).unwrap();
        assert_eq!(
            output.as_slice(),
            b"0,0,NONE,NONE,NONE,NONE,0,00,00,00,00,00,00,00,00"
        );
    }

    #[test]
    fn usb_host_enumeration_diagnostic_query_accepts_short_and_long_headers() {
        let short = query_id(b"SYST:USB:HOST:ENUM:DIAG?\n");
        let long = query_id(b"SYSTEM:USB:HOST:ENUMERATION:DIAGNOSTIC?\n");

        assert_eq!(short, long);
    }

    #[test]
    fn usb_host_ftdi_baud_accepts_short_and_long_headers() {
        let short_query = query_id(b"SYST:USB:HOST:FTDI:BAUD?\n");
        let long_query = query_id(b"SYSTEM:USB:HOST:FTDI:BAUDRATE?\n");
        let short_command = command_id(b"SYST:USB:HOST:FTDI:BAUD 9600\n");
        let long_command = command_id(b"SYSTEM:USB:HOST:FTDI:BAUDRATE 9600\n");

        assert_eq!(short_query, long_query);
        assert_eq!(short_command, long_command);
    }

    #[cfg(feature = "board-adafruit-rp2040-usb-host")]
    #[test]
    fn usb_host_bleuio_sensor_headers_and_responses_are_stable() {
        let short_query = query_id(b"SYST:USB:HOST:BLEU:SENS:CAT?\n");
        let long_query = query_id(b"SYSTEM:USB:HOST:BLEUIO:SENSOR:CATALOG?\n");
        let short_command = command_id(b"SYST:USB:HOST:BLEU:SENS:FILT \"22005A,22008C\"\n");
        let long_command = command_id(b"SYSTEM:USB:HOST:BLEUIO:SENSOR:FILTER \"22005A,22008C\"\n");
        assert_eq!(short_query, long_query);
        assert_eq!(short_command, long_command);

        let mut output = heapless::Vec::<u8, 64>::new();
        BleuioFilterResponse(crate::bleuio::Filter::parse("22005A,22008C").unwrap())
            .write_response(&mut output)
            .unwrap();
        assert_eq!(output.as_slice(), b"22005A,22008C");
        output.clear();
        BleuioSensorIdResponse(0x22005a)
            .write_response(&mut output)
            .unwrap();
        assert_eq!(output.as_slice(), b"22005A");
    }
}
