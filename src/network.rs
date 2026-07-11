use core::task::Context;

use embassy_net::Ipv6Address;
use embassy_net::driver::{Capabilities, Driver as NetDriver, HardwareAddress, LinkState};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_time::{Duration, Timer};
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_ncm;
use embassy_usb::class::cdc_ncm::embassy_net::Device as NcmDevice;
use portable_atomic::{AtomicU32, Ordering};

pub(crate) static NET_RX_PACKETS: AtomicU32 = AtomicU32::new(0);

/// embassy-net device wrapper that counts received frames, so the main loop
/// can tell a live host from one that missed the NCM link-up notification.
pub(crate) struct CountingDevice<D> {
    pub(crate) inner: D,
}

impl<D: NetDriver> NetDriver for CountingDevice<D> {
    type RxToken<'a>
        = D::RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = D::TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let tokens = self.inner.receive(cx);
        if tokens.is_some() {
            NET_RX_PACKETS.fetch_add(1, Ordering::Relaxed);
        }
        tokens
    }

    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        self.inner.transmit(cx)
    }

    fn link_state(&mut self, cx: &mut Context) -> LinkState {
        self.inner.link_state(cx)
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn hardware_address(&self) -> HardwareAddress {
        self.inner.hardware_address()
    }
}

fn stored_host_silence_reset_count() -> u32 {
    let value = embassy_rp::pac::WATCHDOG.scratch0().read();
    if value & 0xFFFF_0000 == crate::HOST_SILENCE_RESET_MAGIC {
        value & 0xFFFF
    } else {
        0
    }
}

fn store_host_silence_reset_count(count: u32) {
    embassy_rp::pac::WATCHDOG
        .scratch0()
        .write_value(crate::HOST_SILENCE_RESET_MAGIC | (count & 0xFFFF));
}

/// Recover the retry count only when the previous boot deliberately requested
/// a USB recovery reset. A reset-button press therefore starts a fresh recovery
/// sequence even though RP2040 watchdog scratch registers survive system reset.
pub(crate) fn take_host_silence_reset_count() -> u32 {
    let marker = embassy_rp::pac::WATCHDOG.scratch1().read();
    embassy_rp::pac::WATCHDOG.scratch1().write_value(0);

    if marker == crate::HOST_SILENCE_RESET_MARKER {
        stored_host_silence_reset_count()
    } else {
        store_host_silence_reset_count(0);
        0
    }
}

pub(crate) fn arm_host_silence_reset(count: u32) {
    store_host_silence_reset_count(count);
    embassy_rp::pac::WATCHDOG
        .scratch1()
        .write_value(crate::HOST_SILENCE_RESET_MARKER);
}

pub(crate) fn clear_host_silence_reset_count() {
    store_host_silence_reset_count(0);
    embassy_rp::pac::WATCHDOG.scratch1().write_value(0);
}

/// Force an unmistakable USB disconnect before rebooting. A bare system reset
/// can be too short for a host to discard a half-configured CDC-NCM interface.
pub(crate) async fn usb_reenumeration_reset(disconnect_time: Duration) -> ! {
    embassy_rp::pac::USB
        .sie_ctrl()
        .modify(|w| w.set_pullup_en(false));
    Timer::after(disconnect_time).await;
    cortex_m::peripheral::SCB::sys_reset();
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;

    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    hash
}

fn fnv1a64_with_role(seed: &[u8], role: u8) -> u64 {
    let mut hash = fnv1a64(seed);
    hash ^= u64::from(role);
    hash.wrapping_mul(0x0000_0100_0000_01b3)
}

#[cfg(not(feature = "dhcp-server"))]
pub(crate) fn link_local_from_seed(seed: &[u8], role: u8) -> [u8; 4] {
    let hash = fnv1a64_with_role(seed, role);
    let host = (hash % (254 * 256)) as u16;

    [169, 254, 1 + (host / 256) as u8, (host & 0xff) as u8]
}

#[cfg(feature = "dhcp-server")]
pub(crate) fn private_subnet_from_seed(seed: &[u8], role: u8) -> ([u8; 4], [u8; 4]) {
    let bytes = fnv1a64_with_role(seed, role).to_le_bytes();
    let subnet_x = 1 + (bytes[0] % 254);
    let subnet_y = bytes[1];

    ([10, subnet_x, subnet_y, 1], [10, subnet_x, subnet_y, 2])
}

pub(crate) fn mac_from_seed(seed: &[u8], role: u8) -> [u8; 6] {
    let bytes = fnv1a64_with_role(seed, role).to_le_bytes();
    [0x02, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]]
}

/// EUI-64 derived IPv6 link-local address (fe80::/64) for a MAC address.
///
/// macOS resolves these with an interface scope, so connections to the AAAA
/// record always leave through the right interface - unlike IPv4 link-local,
/// where 169.254/16 routes are ambiguous across interfaces.
pub(crate) fn ipv6_link_local_from_mac(mac: &[u8; 6]) -> Ipv6Address {
    Ipv6Address::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([mac[0] ^ 0x02, mac[1]]),
        u16::from_be_bytes([mac[2], 0xff]),
        u16::from_be_bytes([0xfe, mac[3]]),
        u16::from_be_bytes([mac[4], mac[5]]),
    )
}

#[embassy_executor::task]
pub(crate) async fn usb_task(mut usb: UsbDevice<'static, Driver<'static, USB>>) {
    usb.run().await;
}

#[embassy_executor::task]
pub(crate) async fn ncm_task(
    runner: cdc_ncm::embassy_net::Runner<'static, Driver<'static, USB>, { crate::MTU }>,
) {
    runner.run().await;
}

#[embassy_executor::task]
pub(crate) async fn net_task(
    mut runner: embassy_net::Runner<'static, CountingDevice<NcmDevice<'static, { crate::MTU }>>>,
) {
    runner.run().await;
}
