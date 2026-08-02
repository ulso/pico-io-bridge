use core::ptr::addr_of_mut;

use embassy_executor::{Executor, Spawner};
use embassy_rp::bind_interrupts;
use embassy_rp::i2c::{Config as I2cConfig, I2c, InterruptHandler};
use embassy_rp::multicore::{self, Stack};
use embassy_rp::peripherals::{CORE1, FLASH, I2C0, USB};
use embassy_rp::uart::{Blocking, Config as UartConfig, UartTx};
use embassy_rp::{Peri, Peripherals};
use static_cell::StaticCell;

use super::StatusIndicator;
use crate::{i2c, scpi, usb_host};

pub(crate) const BOARD_NAME: &str = "Waveshare RP2350-USB-A";
pub(crate) const USB_PRODUCT: &str = "Pico I/O Bridge - Waveshare RP2350-USB-A";
pub(crate) const MDNS_HOST_LABEL: &str = "pico-io-waveshare-rp2350";
pub(crate) const MDNS_SERVICE_INSTANCE: &str = "Pico I/O Bridge - Waveshare RP2350-USB-A";
pub(crate) const INTERFACE_STARTUP_LOG: &[u8] =
    b"I2C task starting, I2C0 SCL GP5 SDA GP4 at 400 kHz\r\n\
PIO USB host starting, D+ GP12 D- GP13, VBUS permanently powered\r\n\
Raw USB serial bridge starting, TCP port 7000\r\n\
USBTMC SCPI bridge starting, TCP port 5026\r\n\
SCPI server starting, ADC A0-A3 on GP26-GP29, TCP port 5025\r\n";

bind_interrupts!(struct I2cIrqs {
    I2C0_IRQ => InterruptHandler<I2C0>;
});

pub(super) fn set_status_ready(_ready: bool) {
    // PIO2 is intentionally left unused while PIO0/PIO1 own the USB host.
    // embassy-rp 0.10 shares its PIO ownership counter between instances.
}

const CORE1_STACK_SIZE: usize = 16 * 1024;
static mut CORE1_STACK: Stack<CORE1_STACK_SIZE> = Stack::new();
static CORE1_EXECUTOR: StaticCell<Executor> = StaticCell::new();

pub(crate) struct Board {
    pub(crate) flash: Peri<'static, FLASH>,
    pub(crate) usb: Peri<'static, USB>,
    pub(crate) core1: Peri<'static, CORE1>,
    pub(crate) uart: UartTx<'static, Blocking>,
    pub(crate) status: StatusIndicator,
    pub(crate) interfaces: Interfaces,
}

pub(crate) struct Interfaces {
    i2c: I2c<'static, I2C0, embassy_rp::i2c::Async>,
    scpi: scpi::Hardware,
    usb_host: usb_host::Hardware,
}

impl Interfaces {
    pub(crate) fn spawn(
        self,
        spawner: Spawner,
        stack: embassy_net::Stack<'static>,
        serial: &'static str,
        core1: Peri<'static, CORE1>,
    ) {
        let Self {
            i2c,
            scpi,
            usb_host,
        } = self;

        multicore::spawn_core1(
            core1,
            unsafe { &mut *addr_of_mut!(CORE1_STACK) },
            move || {
                let executor = CORE1_EXECUTOR.init(Executor::new());
                executor.run(|spawner| {
                    spawner.spawn(usb_host::usb_host_task(usb_host).unwrap());
                })
            },
        );

        spawner.spawn(i2c::i2c0_task(i2c).unwrap());
        spawner.spawn(usb_host::usb_serial_task(stack).unwrap());
        spawner.spawn(usb_host::usbtmc_task(stack).unwrap());
        scpi.spawn(spawner, stack, serial);
    }
}

pub(crate) fn init(p: Peripherals) -> Board {
    let mut i2c_config = I2cConfig::default();
    i2c_config.frequency = i2c::I2C_FREQUENCY;

    Board {
        flash: p.FLASH,
        usb: p.USB,
        core1: p.CORE1,
        uart: UartTx::new_blocking(p.UART0, p.PIN_0, UartConfig::default()),
        status: StatusIndicator::ws2812(),
        interfaces: Interfaces {
            i2c: I2c::new_async(p.I2C0, p.PIN_5, p.PIN_4, I2cIrqs, i2c_config),
            scpi: scpi::Hardware::new(
                p.ADC,
                p.ADC_TEMP_SENSOR,
                p.PIN_26,
                p.PIN_27,
                p.PIN_28,
                p.PIN_29,
            ),
            usb_host: usb_host::Hardware::new(p.PIO0, p.PIO1, p.DMA_CH0, p.PIN_12, p.PIN_13),
        },
    }
}
