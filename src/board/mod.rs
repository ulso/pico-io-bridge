#[cfg(not(any(
    feature = "board-adafruit-kb2040",
    feature = "board-waveshare-rp2350-usb-a"
)))]
use embassy_rp::Peri;
use embassy_rp::gpio::Output;
#[cfg(not(any(
    feature = "board-adafruit-kb2040",
    feature = "board-waveshare-rp2350-usb-a"
)))]
use embassy_rp::gpio::{Level, Pin};

#[cfg(not(any(
    feature = "board-adafruit-rp2040-can",
    feature = "board-adafruit-feather-rp2040",
    feature = "board-adafruit-rp2040-usb-host",
    feature = "board-adafruit-kb2040",
    feature = "board-waveshare-rp2350-usb-a",
    feature = "board-adafruit-fruit-jam"
)))]
compile_error!(
    "select one board feature: board-adafruit-rp2040-can, board-adafruit-feather-rp2040, board-adafruit-rp2040-usb-host, board-adafruit-kb2040, board-waveshare-rp2350-usb-a, or board-adafruit-fruit-jam"
);

#[cfg(any(
    all(
        feature = "board-adafruit-rp2040-can",
        any(
            feature = "board-adafruit-feather-rp2040",
            feature = "board-adafruit-rp2040-usb-host",
            feature = "board-adafruit-kb2040",
            feature = "board-waveshare-rp2350-usb-a",
            feature = "board-adafruit-fruit-jam"
        )
    ),
    all(
        feature = "board-adafruit-feather-rp2040",
        any(
            feature = "board-adafruit-rp2040-usb-host",
            feature = "board-adafruit-kb2040",
            feature = "board-waveshare-rp2350-usb-a",
            feature = "board-adafruit-fruit-jam"
        )
    ),
    all(
        feature = "board-adafruit-rp2040-usb-host",
        any(
            feature = "board-adafruit-kb2040",
            feature = "board-waveshare-rp2350-usb-a",
            feature = "board-adafruit-fruit-jam"
        )
    ),
    all(
        feature = "board-adafruit-kb2040",
        any(
            feature = "board-waveshare-rp2350-usb-a",
            feature = "board-adafruit-fruit-jam"
        )
    ),
    all(
        feature = "board-waveshare-rp2350-usb-a",
        feature = "board-adafruit-fruit-jam"
    )
))]
compile_error!("board features are mutually exclusive; select exactly one board");

#[cfg(all(
    not(feature = "board-adafruit-rp2040-can"),
    any(
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-rp2040-usb-host",
        feature = "board-adafruit-kb2040",
        feature = "board-waveshare-rp2350-usb-a",
        feature = "board-adafruit-fruit-jam"
    ),
    feature = "can"
))]
compile_error!("the selected board profile does not define CAN hardware");

#[cfg(feature = "board-adafruit-rp2040-can")]
mod adafruit_rp2040_can;
#[cfg(feature = "board-adafruit-rp2040-can")]
pub(crate) use adafruit_rp2040_can::*;

#[cfg(feature = "board-adafruit-feather-rp2040")]
mod adafruit_feather_rp2040;
#[cfg(feature = "board-adafruit-feather-rp2040")]
pub(crate) use adafruit_feather_rp2040::*;

#[cfg(feature = "board-adafruit-rp2040-usb-host")]
mod adafruit_rp2040_usb_host;
#[cfg(feature = "board-adafruit-rp2040-usb-host")]
pub(crate) use adafruit_rp2040_usb_host::*;

#[cfg(feature = "board-adafruit-kb2040")]
mod adafruit_kb2040;
#[cfg(feature = "board-adafruit-kb2040")]
pub(crate) use adafruit_kb2040::*;

#[cfg(feature = "board-waveshare-rp2350-usb-a")]
mod waveshare_rp2350_usb_a;
#[cfg(feature = "board-waveshare-rp2350-usb-a")]
pub(crate) use waveshare_rp2350_usb_a::*;

#[cfg(feature = "board-adafruit-fruit-jam")]
mod adafruit_fruit_jam;
#[cfg(feature = "board-adafruit-fruit-jam")]
pub(crate) use adafruit_fruit_jam::*;

pub(crate) fn rp_config() -> embassy_rp::config::Config {
    #[cfg(feature = "pio-usb-host")]
    {
        const SYS_CLOCK_HZ: u32 = 120_000_000;
        let clocks = embassy_rp::clocks::ClockConfig::system_freq(SYS_CLOCK_HZ)
            .expect("valid 120 MHz PLL setup");
        embassy_rp::config::Config::new(clocks)
    }

    #[cfg(not(feature = "pio-usb-host"))]
    {
        Default::default()
    }
}

pub(crate) struct StatusIndicator {
    output: Option<Output<'static>>,
    #[cfg(feature = "board-waveshare-rp2350-usb-a")]
    ws2812: bool,
}

impl StatusIndicator {
    #[cfg(not(any(
        feature = "board-adafruit-kb2040",
        feature = "board-waveshare-rp2350-usb-a"
    )))]
    pub(crate) fn active_high<P: Pin>(pin: Peri<'static, P>) -> Self {
        Self {
            output: Some(Output::new(pin, Level::High)),
        }
    }

    #[cfg(feature = "board-adafruit-kb2040")]
    pub(crate) const fn none() -> Self {
        Self { output: None }
    }

    #[cfg(feature = "board-waveshare-rp2350-usb-a")]
    pub(crate) const fn ws2812() -> Self {
        Self {
            output: None,
            ws2812: true,
        }
    }

    pub(crate) fn set_busy(&mut self) {
        if let Some(output) = self.output.as_mut() {
            output.set_high();
        }
        #[cfg(feature = "board-waveshare-rp2350-usb-a")]
        if self.ws2812 {
            waveshare_rp2350_usb_a::set_status_ready(false);
        }
    }

    pub(crate) fn set_ready(&mut self) {
        if let Some(output) = self.output.as_mut() {
            output.set_low();
        }
        #[cfg(feature = "board-waveshare-rp2350-usb-a")]
        if self.ws2812 {
            waveshare_rp2350_usb_a::set_status_ready(true);
        }
    }
}
