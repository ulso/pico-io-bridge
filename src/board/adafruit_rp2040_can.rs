use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::i2c::{Config as I2cConfig, I2c, InterruptHandler};
use embassy_rp::peripherals::{FLASH, I2C1, PIN_8, PIN_14, PIN_15, PIN_19, SPI1, USB};
use embassy_rp::uart::{Blocking, Config as UartConfig, UartTx};
use embassy_rp::{Peri, Peripherals};

use super::StatusIndicator;
use crate::{can, i2c, scpi};

pub(crate) const FLASH_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const BOARD_NAME: &str = "RP2040 CAN Bus Feather";
pub(crate) const USB_PRODUCT: &str = "Pico I/O Bridge - RP2040 CAN Bus Feather";
pub(crate) const MDNS_HOST_LABEL: &str = "pico-io-can-feather";
pub(crate) const MDNS_SERVICE_INSTANCE: &str = "Pico I/O Bridge - RP2040 CAN Bus Feather";
pub(crate) const INTERFACE_STARTUP_LOG: &[u8] =
    b"CAN task starting, MCP25625 on SPI1 SCK GP14 MOSI GP15 MISO GP8 CS GP19\r\n\
I2C task starting, I2C1 SCL GP3 SDA GP2 at 400 kHz\r\n\
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
    spi: Peri<'static, SPI1>,
    sck: Peri<'static, PIN_14>,
    mosi: Peri<'static, PIN_15>,
    miso: Peri<'static, PIN_8>,
    cs: Peri<'static, PIN_19>,
    scpi: scpi::Hardware,
}

impl Interfaces {
    pub(crate) fn spawn(
        self,
        spawner: Spawner,
        stack: embassy_net::Stack<'static>,
        serial: &'static str,
    ) {
        spawner.spawn(can::can_task(self.spi, self.sck, self.mosi, self.miso, self.cs).unwrap());
        spawner.spawn(i2c::i2c1_task(self.i2c).unwrap());
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
            spi: p.SPI1,
            sck: p.PIN_14,
            mosi: p.PIN_15,
            miso: p.PIN_8,
            cs: p.PIN_19,
            scpi: scpi::Hardware::new(
                p.ADC,
                p.ADC_TEMP_SENSOR,
                p.PIN_26,
                p.PIN_27,
                p.PIN_28,
                p.PIN_29,
            ),
        },
    }
}
