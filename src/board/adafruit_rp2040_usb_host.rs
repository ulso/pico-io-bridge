use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::i2c::{Config as I2cConfig, I2c, InterruptHandler};
use embassy_rp::peripherals::{FLASH, I2C1, USB};
use embassy_rp::uart::{Blocking, Config as UartConfig, UartTx};
use embassy_rp::{Peri, Peripherals};

use super::StatusIndicator;
use crate::{i2c, scpi, usb_host};

pub(crate) const FLASH_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const BOARD_NAME: &str = "Feather RP2040 USB Host";
pub(crate) const USB_PRODUCT: &str = "Pico I/O Bridge - Feather RP2040 USB Host";
pub(crate) const MDNS_HOST_LABEL: &str = "pico-io-usb-host";
pub(crate) const MDNS_SERVICE_INSTANCE: &str = "Pico I/O Bridge - Feather RP2040 USB Host";
pub(crate) const INTERFACE_STARTUP_LOG: &[u8] =
    b"I2C task starting, I2C1 SCL GP3 SDA GP2 at 400 kHz\r\n\
PIO USB host starting, D+ GP16 D- GP17 VBUS enable GP18\r\n\
SCPI server starting, ADC A0-A3 on GP26-GP29, TCP port 5025\r\n";

bind_interrupts!(struct I2cIrqs {
    I2C1_IRQ => InterruptHandler<I2C1>;
});

pub(crate) struct Board {
    pub(crate) flash: Peri<'static, FLASH>,
    pub(crate) usb: Peri<'static, USB>,
    pub(crate) uart: UartTx<'static, Blocking>,
    pub(crate) status: StatusIndicator,
    pub(crate) interfaces: Interfaces,
}

pub(crate) struct Interfaces {
    i2c: I2c<'static, I2C1, embassy_rp::i2c::Async>,
    scpi: scpi::Hardware,
    usb_host: usb_host::Hardware,
}

impl Interfaces {
    pub(crate) fn spawn(
        self,
        spawner: Spawner,
        stack: embassy_net::Stack<'static>,
        serial: &'static str,
    ) {
        spawner.spawn(i2c::i2c1_task(self.i2c).unwrap());
        spawner.spawn(usb_host::usb_host_task(self.usb_host).unwrap());
        self.scpi.spawn(spawner, stack, serial);
    }
}

pub(crate) fn init(p: Peripherals) -> Board {
    let mut i2c_config = I2cConfig::default();
    i2c_config.frequency = i2c::I2C_FREQUENCY;

    Board {
        flash: p.FLASH,
        usb: p.USB,
        uart: UartTx::new_blocking(p.UART0, p.PIN_0, UartConfig::default()),
        status: StatusIndicator::active_high(p.PIN_13),
        interfaces: Interfaces {
            i2c: I2c::new_async(p.I2C1, p.PIN_3, p.PIN_2, I2cIrqs, i2c_config),
            scpi: scpi::Hardware::new(
                p.ADC,
                p.ADC_TEMP_SENSOR,
                p.PIN_26,
                p.PIN_27,
                p.PIN_28,
                p.PIN_29,
            ),
            usb_host: usb_host::Hardware::new(
                p.PIO0, p.PIO1, p.DMA_CH0, p.PIN_16, p.PIN_17, p.PIN_18,
            ),
        },
    }
}
