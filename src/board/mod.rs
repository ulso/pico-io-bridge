#[cfg(not(feature = "board-adafruit-kb2040"))]
use embassy_rp::Peri;
use embassy_rp::gpio::Output;
#[cfg(not(feature = "board-adafruit-kb2040"))]
use embassy_rp::gpio::{Level, Pin};

#[cfg(not(any(
    feature = "board-adafruit-rp2040-can",
    feature = "board-adafruit-feather-rp2040",
    feature = "board-adafruit-rp2040-usb-host",
    feature = "board-adafruit-kb2040"
)))]
compile_error!(
    "select one board feature: board-adafruit-rp2040-can, board-adafruit-feather-rp2040, board-adafruit-rp2040-usb-host, or board-adafruit-kb2040"
);

#[cfg(any(
    all(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-feather-rp2040"
    ),
    all(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-rp2040-usb-host"
    ),
    all(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-kb2040"
    ),
    all(
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-rp2040-usb-host"
    ),
    all(
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-kb2040"
    ),
    all(
        feature = "board-adafruit-rp2040-usb-host",
        feature = "board-adafruit-kb2040"
    )
))]
compile_error!("board features are mutually exclusive; select exactly one board");

#[cfg(all(
    not(feature = "board-adafruit-rp2040-can"),
    any(
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-rp2040-usb-host",
        feature = "board-adafruit-kb2040"
    ),
    feature = "can"
))]
compile_error!("the selected board profile does not define CAN hardware");

#[cfg(all(
    feature = "board-adafruit-rp2040-can",
    not(any(
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-rp2040-usb-host",
        feature = "board-adafruit-kb2040"
    ))
))]
mod adafruit_rp2040_can;
#[cfg(all(
    feature = "board-adafruit-rp2040-can",
    not(any(
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-rp2040-usb-host",
        feature = "board-adafruit-kb2040"
    ))
))]
pub(crate) use adafruit_rp2040_can::*;

#[cfg(all(
    feature = "board-adafruit-feather-rp2040",
    not(any(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-rp2040-usb-host",
        feature = "board-adafruit-kb2040"
    ))
))]
mod adafruit_feather_rp2040;
#[cfg(all(
    feature = "board-adafruit-feather-rp2040",
    not(any(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-rp2040-usb-host",
        feature = "board-adafruit-kb2040"
    ))
))]
pub(crate) use adafruit_feather_rp2040::*;

#[cfg(all(
    feature = "board-adafruit-rp2040-usb-host",
    not(any(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-kb2040"
    ))
))]
mod adafruit_rp2040_usb_host;
#[cfg(all(
    feature = "board-adafruit-rp2040-usb-host",
    not(any(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-kb2040"
    ))
))]
pub(crate) use adafruit_rp2040_usb_host::*;

#[cfg(all(
    feature = "board-adafruit-kb2040",
    not(any(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-rp2040-usb-host"
    ))
))]
mod adafruit_kb2040;
#[cfg(all(
    feature = "board-adafruit-kb2040",
    not(any(
        feature = "board-adafruit-rp2040-can",
        feature = "board-adafruit-feather-rp2040",
        feature = "board-adafruit-rp2040-usb-host"
    ))
))]
pub(crate) use adafruit_kb2040::*;

pub(crate) fn rp_config() -> embassy_rp::config::Config {
    #[cfg(feature = "board-adafruit-rp2040-usb-host")]
    {
        const SYS_CLOCK_HZ: u32 = 120_000_000;
        let clocks = embassy_rp::clocks::ClockConfig::system_freq(SYS_CLOCK_HZ)
            .expect("valid 120 MHz PLL setup");
        embassy_rp::config::Config::new(clocks)
    }

    #[cfg(not(feature = "board-adafruit-rp2040-usb-host"))]
    {
        Default::default()
    }
}

pub(crate) struct StatusIndicator {
    output: Option<Output<'static>>,
}

impl StatusIndicator {
    #[cfg(not(feature = "board-adafruit-kb2040"))]
    pub(crate) fn active_high<P: Pin>(pin: Peri<'static, P>) -> Self {
        Self {
            output: Some(Output::new(pin, Level::High)),
        }
    }

    #[cfg(feature = "board-adafruit-kb2040")]
    pub(crate) const fn none() -> Self {
        Self { output: None }
    }

    pub(crate) fn set_busy(&mut self) {
        if let Some(output) = self.output.as_mut() {
            output.set_high();
        }
    }

    pub(crate) fn set_ready(&mut self) {
        if let Some(output) = self.output.as_mut() {
            output.set_low();
        }
    }
}
