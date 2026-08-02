use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::dma;
use embassy_rp::i2c::{Config as I2cConfig, I2c, InterruptHandler};
use embassy_rp::peripherals::{DMA_CH11, FLASH, I2C0, PIN_16, PIO2, USB};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program, Rgb};
use embassy_rp::uart::{Blocking, Config as UartConfig, UartTx};
use embassy_rp::{Peri, Peripherals};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use smart_leds::RGB8;

use super::StatusIndicator;
use crate::{i2c, scpi};

pub(crate) const BOARD_NAME: &str = "Waveshare RP2350-USB-A";
pub(crate) const USB_PRODUCT: &str = "Pico I/O Bridge - Waveshare RP2350-USB-A";
pub(crate) const MDNS_HOST_LABEL: &str = "pico-io-waveshare-rp2350";
pub(crate) const MDNS_SERVICE_INSTANCE: &str = "Pico I/O Bridge - Waveshare RP2350-USB-A";
pub(crate) const INTERFACE_STARTUP_LOG: &[u8] =
    b"I2C task starting, I2C0 SCL GP5 SDA GP4 at 400 kHz\r\n\
SCPI server starting, ADC A0-A3 on GP26-GP29, TCP port 5025\r\n";

bind_interrupts!(struct I2cIrqs {
    I2C0_IRQ => InterruptHandler<I2C0>;
});

bind_interrupts!(struct StatusLedIrqs {
    PIO2_IRQ_0 => PioInterruptHandler<PIO2>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH11>;
});

static STATUS_READY: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub(super) fn set_status_ready(ready: bool) {
    STATUS_READY.signal(ready);
}

struct StatusLedHardware {
    pio: Peri<'static, PIO2>,
    dma: Peri<'static, DMA_CH11>,
    pin: Peri<'static, PIN_16>,
}

#[embassy_executor::task]
async fn status_led_task(hardware: StatusLedHardware) {
    const RED: RGB8 = RGB8::new(16, 0, 0);
    const GREEN: RGB8 = RGB8::new(0, 16, 0);

    let mut pio = Pio::new(hardware.pio, StatusLedIrqs);
    let program = PioWs2812Program::new(&mut pio.common);
    let mut led = PioWs2812::<_, 0, 1, Rgb>::with_color_order(
        &mut pio.common,
        pio.sm0,
        hardware.dma,
        StatusLedIrqs,
        hardware.pin,
        &program,
    );

    led.write(&[RED]).await;
    loop {
        let color = if STATUS_READY.wait().await {
            GREEN
        } else {
            RED
        };
        led.write(&[color]).await;
    }
}

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
    status_led: StatusLedHardware,
}

impl Interfaces {
    pub(crate) fn spawn(
        self,
        spawner: Spawner,
        stack: embassy_net::Stack<'static>,
        serial: &'static str,
    ) {
        spawner.spawn(i2c::i2c0_task(self.i2c).unwrap());
        spawner.spawn(status_led_task(self.status_led).unwrap());
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
            status_led: StatusLedHardware {
                pio: p.PIO2,
                dma: p.DMA_CH11,
                pin: p.PIN_16,
            },
        },
    }
}
