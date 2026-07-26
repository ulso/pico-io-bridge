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

const CHANNEL_COUNT: usize = 4;
const DEFAULT_AVERAGE_COUNT: u16 = 16;
const MAX_AVERAGE_COUNT: u16 = 256;
const ADC_COUNTS: f32 = 4096.0;
const ADC_VREF: f32 = 3.3;
const SCPI_BUFFER_SIZE: usize = 1024;
const SOCKET_BUFFER_SIZE: usize = 1024;

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
