use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::i2c::{Config as I2cConfig, I2c, InterruptHandler};
use embassy_rp::peripherals::{FLASH, I2C0, USB};
use embassy_rp::uart::{Blocking, Config as UartConfig, UartTx};
use embassy_rp::{Peri, Peripherals};

use super::StatusIndicator;
use crate::{i2c, scpi};

pub(crate) const BOARD_NAME: &str = "Adafruit Fruit Jam";
pub(crate) const USB_PRODUCT: &str = "Pico I/O Bridge - Adafruit Fruit Jam";
pub(crate) const MDNS_HOST_LABEL: &str = "pico-io-fruit-jam";
pub(crate) const MDNS_SERVICE_INSTANCE: &str = "Pico I/O Bridge - Adafruit Fruit Jam";
pub(crate) const INTERFACE_STARTUP_LOG: &[u8] =
    b"I2C task starting, I2C0 SCL GP21 SDA GP20 at 400 kHz\r\n\
SCPI server starting, ADC A0-A3 on GP40-GP43, TCP port 5025\r\n\
Fruit Jam USB host and CH334F hub disabled in this initial profile\r\n";

bind_interrupts!(struct I2cIrqs {
    I2C0_IRQ => InterruptHandler<I2C0>;
});

pub(crate) struct Board {
    pub(crate) flash: Peri<'static, FLASH>,
    pub(crate) usb: Peri<'static, USB>,
    pub(crate) uart: UartTx<'static, Blocking>,
    pub(crate) status: StatusIndicator,
    pub(crate) interfaces: Interfaces,
}

pub(crate) struct Interfaces {
    i2c: I2c<'static, I2C0, embassy_rp::i2c::Async>,
    scpi: scpi::Hardware,
}

impl Interfaces {
    pub(crate) fn spawn(
        self,
        spawner: Spawner,
        stack: embassy_net::Stack<'static>,
        serial: &'static str,
    ) {
        spawner.spawn(i2c::i2c0_task(self.i2c).unwrap());
        self.scpi.spawn(spawner, stack, serial);
    }
}

pub(crate) fn init(p: Peripherals) -> Board {
    let mut i2c_config = I2cConfig::default();
    i2c_config.frequency = i2c::I2C_FREQUENCY;

    Board {
        flash: p.FLASH,
        usb: p.USB,
        // Analog pin A4 (GP44) is otherwise unused by this initial profile.
        // Keeping diagnostics here avoids sending startup text to the
        // ESP32-C6 on GP8/GP9.
        uart: UartTx::new_blocking(p.UART0, p.PIN_44, UartConfig::default()),
        // The red LED is active-low and shares GP29 with the IR receiver.
        status: StatusIndicator::active_high(p.PIN_29),
        interfaces: Interfaces {
            i2c: I2c::new_async(p.I2C0, p.PIN_21, p.PIN_20, I2cIrqs, i2c_config),
            scpi: scpi::Hardware::new(
                p.ADC,
                p.ADC_TEMP_SENSOR,
                p.PIN_40,
                p.PIN_41,
                p.PIN_42,
                p.PIN_43,
            ),
        },
    }
}
