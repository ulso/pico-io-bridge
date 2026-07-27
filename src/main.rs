//! USB CDC-NCM network interface.

#![no_std]
#![no_main]

#[cfg(feature = "mdns")]
extern crate alloc;

#[cfg(all(feature = "can", not(any(feature = "mcp2515", feature = "mcp25625"))))]
compile_error!("the can feature requires one controller feature: mcp2515 or mcp25625");
#[cfg(all(feature = "mcp2515", feature = "mcp25625"))]
compile_error!("mcp2515 and mcp25625 are mutually exclusive");

#[cfg(feature = "i2c")]
mod amg8833;
#[cfg(feature = "i2c")]
mod bme688;
#[cfg(feature = "i2c")]
mod bno08x;
mod board;
#[cfg(feature = "can")]
mod can;
#[cfg(feature = "i2c")]
mod devices;
#[cfg(feature = "dhcp-server")]
mod dhcp;
mod http;
#[cfg(feature = "i2c")]
mod i2c;
#[cfg(any(feature = "can", feature = "i2c"))]
mod json;
#[cfg(feature = "i2c")]
mod lc709203f;
#[cfg(feature = "mdns")]
mod mdns;
mod network;
#[cfg(feature = "i2c")]
mod pct2075;
mod scpi;
#[cfg(feature = "i2c")]
mod seesaw_encoder;
#[cfg(any(feature = "can", feature = "i2c"))]
mod websocket;

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::{
    Config as NetConfig, ConfigV6, Ipv4Address, Ipv4Cidr, Ipv6Cidr, StackResources, StaticConfigV4,
    StaticConfigV6,
};
use embassy_rp::bind_interrupts;
use embassy_rp::flash::Flash;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_time::{Duration, Timer};
use embassy_usb::class::cdc_ncm;
use embassy_usb::class::cdc_ncm::embassy_net::State as NcmNetState;
use embassy_usb::{Builder, Config as UsbConfig};
#[cfg(feature = "mdns")]
use embedded_alloc::LlffHeap as Heap;
use portable_atomic::Ordering;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

pub(crate) const MTU: usize = 1514;
const USB_MAX_PACKET_SIZE: u16 = 64;
const FLASH_UID_BYTES: usize = 8;
const USB_SERIAL_BYTES: usize = FLASH_UID_BYTES * 2;
pub(crate) const HTTP_PORT: u16 = 80;
pub(crate) const HTTP_SOCKETS: usize = 4;
pub(crate) const SCPI_PORT: u16 = 5025;
pub(crate) const MANUFACTURER: &str = "Pico I/O Bridge Project";
pub(crate) const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const WS_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const WS_KEEPALIVE: Duration = Duration::from_secs(5);
const CDC_NCM_LINK_UP_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(feature = "mdns")]
const MDNS_HOST_SETTLE_DELAY: Duration = Duration::from_secs(3);
// The responder re-broadcasts unsolicited announcements at 80% of the record
// TTL. Announcements sent while the host is still configuring the fresh
// interface are lost, so keep the TTL short enough that the next broadcast
// arrives within seconds, not minutes.
#[cfg(feature = "mdns")]
pub(crate) const MDNS_TTL_SECS: u32 = 25;
const CDC_NCM_LINK_UP_RESET_MISSES: u8 = 2;
// A healthy host floods a fresh interface with NDP/mDNS within a second or
// two of link-up. Total silence means its NCM driver missed the one-shot
// NetworkConnection notification and considers the link down; only a USB
// re-enumeration makes embassy-usb send that notification again.
const HOST_SILENCE_TIMEOUT: Duration = Duration::from_secs(8);
pub(crate) const HOST_SILENCE_RESET_MAGIC: u32 = 0xCAFE_0000;
pub(crate) const HOST_SILENCE_RESET_MARKER: u32 = 0xC0DE_CAFE;
const HOST_SILENCE_RESET_LIMIT: u32 = 5;
#[cfg(feature = "mdns")]
const HEAP_SIZE: usize = 32768;

#[cfg(feature = "mdns")]
#[global_allocator]
static HEAP: Heap = Heap::empty();

#[cfg(feature = "dhcp-server")]
const DEVICE_IPV4_PREFIX_LEN: u8 = 24;
#[cfg(not(feature = "dhcp-server"))]
const DEVICE_IPV4_PREFIX_LEN: u8 = 16;
#[cfg(feature = "dhcp-server")]
const DEVICE_DHCP_ROLE: u8 = 4;
#[cfg(not(feature = "dhcp-server"))]
const DEVICE_IPV4_ROLE: u8 = 3;
const DEVICE_MAC_ROLE: u8 = 1;
const HOST_MAC_ROLE: u8 = 2;

bind_interrupts!(struct UsbIrqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

fn encode_usb_serial(uid: &[u8; FLASH_UID_BYTES], serial: &mut [u8; USB_SERIAL_BYTES]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    for (index, byte) in uid.iter().copied().enumerate() {
        serial[index * 2] = HEX[(byte >> 4) as usize];
        serial[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
}

fn host_silence_recovery(attempt: u32) -> (Duration, &'static [u8]) {
    match attempt {
        1 => (
            Duration::from_millis(350),
            b"host silent, USB detach 350 ms then reset\r\n",
        ),
        2 => (
            Duration::from_secs(1),
            b"host silent, USB detach 1 s then reset\r\n",
        ),
        3 => (
            Duration::from_secs(3),
            b"host silent, USB detach 3 s then reset\r\n",
        ),
        _ => (
            Duration::from_secs(5),
            b"host silent, USB detach 5 s then reset\r\n",
        ),
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let board::Board {
        flash,
        usb,
        mut uart,
        mut status,
        interfaces,
    } = board::init(p);
    let mut host_silence_resets = network::take_host_silence_reset_count();

    #[cfg(feature = "mdns")]
    {
        static HEAP_MEM: StaticCell<[u8; HEAP_SIZE]> = StaticCell::new();
        let heap_mem = HEAP_MEM.init([0; HEAP_SIZE]);
        unsafe {
            HEAP.init(heap_mem.as_ptr() as usize, HEAP_SIZE);
        }
    }

    uart.blocking_write(b"pico-io-bridge boot\r\n").unwrap();

    let mut flash = Flash::<_, _, { board::FLASH_SIZE }>::new_blocking(flash);
    let mut flash_uid = [0; FLASH_UID_BYTES];
    if flash.blocking_unique_id(&mut flash_uid).is_err() {
        flash_uid = *b"pico-io!";
        warn!("flash unique ID read failed, using fallback identity seed");
    }

    static USB_SERIAL: StaticCell<[u8; USB_SERIAL_BYTES]> = StaticCell::new();
    let usb_serial_bytes = USB_SERIAL.init([0; USB_SERIAL_BYTES]);
    encode_usb_serial(&flash_uid, usb_serial_bytes);
    let usb_serial: &'static str = core::str::from_utf8(usb_serial_bytes).unwrap();
    uart.blocking_write(b"USB serial ").unwrap();
    uart.blocking_write(usb_serial.as_bytes()).unwrap();
    uart.blocking_write(b"\r\n").unwrap();

    #[cfg(feature = "mdns")]
    let mut mdns_uid_suffix_bytes = [0; mdns::UID_SUFFIX_BYTES];
    #[cfg(feature = "mdns")]
    mdns::encode_uid_suffix(&flash_uid, &mut mdns_uid_suffix_bytes);
    #[cfg(feature = "mdns")]
    let mdns_uid_suffix = core::str::from_utf8(&mdns_uid_suffix_bytes).unwrap();

    #[cfg(feature = "dhcp-server")]
    let (device_ipv4_octets, host_ipv4_octets) =
        network::private_subnet_from_seed(&flash_uid, DEVICE_DHCP_ROLE);
    #[cfg(not(feature = "dhcp-server"))]
    let device_ipv4_octets = network::link_local_from_seed(&flash_uid, DEVICE_IPV4_ROLE);
    let device_mac = network::mac_from_seed(&flash_uid, DEVICE_MAC_ROLE);
    let host_mac = network::mac_from_seed(&flash_uid, HOST_MAC_ROLE);
    let device_ipv4 = Ipv4Address::new(
        device_ipv4_octets[0],
        device_ipv4_octets[1],
        device_ipv4_octets[2],
        device_ipv4_octets[3],
    );
    let device_ipv6 = network::ipv6_link_local_from_mac(&device_mac);

    let usb_driver = Driver::new(usb, UsbIrqs);
    let mut usb_config = UsbConfig::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some(MANUFACTURER);
    usb_config.product = Some(board::USB_PRODUCT);
    usb_config.serial_number = Some(usb_serial);
    usb_config.device_class = cdc_ncm::USB_CLASS_CDC;
    usb_config.device_sub_class = 0x00;
    usb_config.device_protocol = 0x00;
    usb_config.composite_with_iads = false;

    static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESCRIPTOR: StaticCell<[u8; 128]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();

    let mut builder = Builder::new(
        usb_driver,
        usb_config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        MSOS_DESCRIPTOR.init([0; 128]),
        CONTROL_BUF.init([0; 128]),
    );

    static NCM_STATE: StaticCell<cdc_ncm::State<'static>> = StaticCell::new();
    let ncm = cdc_ncm::CdcNcmClass::new(
        &mut builder,
        NCM_STATE.init(cdc_ncm::State::new()),
        host_mac,
        USB_MAX_PACKET_SIZE,
    );

    static NCM_NET_STATE: StaticCell<NcmNetState<MTU, 4, 4>> = StaticCell::new();
    let (ncm_runner, ncm_device) =
        ncm.into_embassy_net_device(NCM_NET_STATE.init(NcmNetState::new()), device_mac);

    let mut net_config = NetConfig::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(device_ipv4, DEVICE_IPV4_PREFIX_LEN),
        gateway: None,
        dns_servers: Default::default(),
    });
    net_config.ipv6 = ConfigV6::Static(StaticConfigV6 {
        address: Ipv6Cidr::new(device_ipv6, 64),
        gateway: None,
        dns_servers: Default::default(),
    });

    static NET_RESOURCES: StaticCell<StackResources<10>> = StaticCell::new();
    let (stack, net_runner) = embassy_net::new(
        network::CountingDevice { inner: ncm_device },
        net_config,
        NET_RESOURCES.init(StackResources::new()),
        0x1234_5678,
    );

    let usb = builder.build();
    spawner.spawn(network::usb_task(usb).unwrap());
    spawner.spawn(network::ncm_task(ncm_runner).unwrap());
    spawner.spawn(network::net_task(net_runner).unwrap());
    #[cfg(feature = "dhcp-server")]
    spawner.spawn(dhcp::dhcp_task(stack, device_ipv4_octets, host_ipv4_octets).unwrap());
    for _ in 0..HTTP_SOCKETS {
        spawner.spawn(http::http_task(stack, usb_serial).unwrap());
    }
    interfaces.spawn(spawner, stack, usb_serial);

    #[cfg(feature = "dhcp-server")]
    info!(
        "USB CDC-NCM ready, DHCP IPv4 {}.{}.{}.{}/24, host lease {}.{}.{}.{}/24, device MAC={=[u8]:02x}, host MAC={=[u8]:02x}",
        device_ipv4_octets[0],
        device_ipv4_octets[1],
        device_ipv4_octets[2],
        device_ipv4_octets[3],
        host_ipv4_octets[0],
        host_ipv4_octets[1],
        host_ipv4_octets[2],
        host_ipv4_octets[3],
        device_mac,
        host_mac
    );
    #[cfg(not(feature = "dhcp-server"))]
    info!(
        "USB CDC-NCM ready, IPv4 {}.{}.{}.{}/16, device MAC={=[u8]:02x}, host MAC={=[u8]:02x}",
        device_ipv4_octets[0],
        device_ipv4_octets[1],
        device_ipv4_octets[2],
        device_ipv4_octets[3],
        device_mac,
        host_mac
    );
    #[cfg(feature = "dhcp-server")]
    uart.blocking_write(b"USB CDC-NCM ready, DHCP IPv4 from flash UID\r\n")
        .unwrap();
    #[cfg(not(feature = "dhcp-server"))]
    uart.blocking_write(b"USB CDC-NCM ready, IPv4 link-local from flash UID\r\n")
        .unwrap();
    uart.blocking_write(board::INTERFACE_STARTUP_LOG).unwrap();

    #[cfg(feature = "mdns")]
    let mut mdns_state: Option<&'static mdns::MdnsState<mdns::MdnsRng>> = None;
    #[cfg(feature = "mdns")]
    let mut mdns_registration: Option<mdns::Registration> = None;
    #[cfg(feature = "mdns")]
    let mut mdns_uses_uid_suffix = false;
    let mut link_ever_up = false;
    let mut link_watchdog_misses = 0;

    loop {
        if link_ever_up {
            // The link has worked before: a down period is host-driven (sleep,
            // interface reconfigure) and a reset would only re-enumerate USB and
            // make the host start over. Wait without a watchdog.
            stack.wait_link_up().await;
        } else {
            match select(stack.wait_link_up(), Timer::after(CDC_NCM_LINK_UP_TIMEOUT)).await {
                Either::First(()) => {
                    link_watchdog_misses = 0;
                }
                Either::Second(()) => {
                    link_watchdog_misses += 1;
                    warn!("CDC-NCM did not reach link-up before watchdog timeout");
                    uart.blocking_write(b"CDC-NCM link watchdog timeout\r\n")
                        .unwrap();
                    if link_watchdog_misses >= CDC_NCM_LINK_UP_RESET_MISSES {
                        warn!("CDC-NCM watchdog requesting system reset");
                        uart.blocking_write(b"CDC-NCM watchdog reset\r\n").unwrap();
                        uart.blocking_flush().unwrap();
                        network::usb_reenumeration_reset(Duration::from_millis(350)).await;
                    }
                    continue;
                }
            }
        }
        link_ever_up = true;

        info!(
            "CDC-NCM link up, IPv4 address {}.{}.{}.{}/{}",
            device_ipv4_octets[0],
            device_ipv4_octets[1],
            device_ipv4_octets[2],
            device_ipv4_octets[3],
            DEVICE_IPV4_PREFIX_LEN
        );
        uart.blocking_write(b"CDC-NCM link up\r\n").unwrap();

        // Wait for proof the host can hear us; a deaf host needs a fresh USB
        // enumeration to see the NCM connection notification again.
        let rx_before = network::NET_RX_PACKETS.load(Ordering::Relaxed);
        let mut host_alive = false;
        let mut waited = Duration::from_millis(0);
        while stack.is_link_up() && waited < HOST_SILENCE_TIMEOUT {
            if network::NET_RX_PACKETS.load(Ordering::Relaxed) != rx_before {
                host_alive = true;
                break;
            }
            Timer::after(Duration::from_millis(250)).await;
            waited += Duration::from_millis(250);
        }

        if host_alive {
            network::clear_host_silence_reset_count();
            uart.blocking_write(b"host traffic seen\r\n").unwrap();
        } else if stack.is_link_up() {
            if host_silence_resets < HOST_SILENCE_RESET_LIMIT {
                host_silence_resets += 1;
                network::arm_host_silence_reset(host_silence_resets);
                let (disconnect_time, message) = host_silence_recovery(host_silence_resets);
                warn!(
                    "host silent after link-up, USB detach {} ms before reset",
                    disconnect_time.as_millis()
                );
                uart.blocking_write(message).unwrap();
                uart.blocking_flush().unwrap();
                network::usb_reenumeration_reset(disconnect_time).await;
            } else {
                warn!("host still silent, reset limit reached, staying up");
                uart.blocking_write(b"host silent, reset limit reached; network not ready\r\n")
                    .unwrap();
            }
        } else {
            // Link dropped while we were probing for host traffic.
            continue;
        }

        let rx_after_host_probe = network::NET_RX_PACKETS.load(Ordering::Relaxed);
        let mut host_ready = host_alive;
        #[cfg(feature = "dhcp-server")]
        let mut dhcp_lease_logged = false;
        let mut network_ready_logged = false;
        #[cfg(feature = "mdns")]
        let mut mdns_service_ready = [false; mdns::SERVICE_COUNT];

        #[cfg(feature = "mdns")]
        {
            let mdns = match mdns_state {
                Some(mdns) => mdns,
                None => {
                    uart.blocking_write(b"mDNS starting\r\n").unwrap();

                    static MDNS_STATE: StaticCell<mdns::MdnsState<mdns::MdnsRng>> =
                        StaticCell::new();

                    let mdns: &'static mdns::MdnsState<mdns::MdnsRng> =
                        MDNS_STATE.init(mdns::MdnsState::new(
                            mdns::EndpointConfig::new(),
                            mdns::MdnsRng::new(0x7069_636f_6361_6e01),
                        ));
                    spawner.spawn(mdns::mdns_task(stack, mdns).unwrap());
                    mdns_state = Some(mdns);
                    mdns
                }
            };

            // The host is still configuring a fresh interface at link-up (IPv6
            // DAD, IPv4 link-local ARP claim) and misses announcements sent
            // before it is listening. Wait for it to settle, then (re)register
            // so a full probe + announce cycle goes out on every link-up, as
            // RFC 6762 section 8.3 expects.
            Timer::after(MDNS_HOST_SETTLE_DELAY).await;

            if let Some(registration) = mdns_registration.take() {
                registration.unregister(mdns);
            }
            let uid_suffix = mdns_uses_uid_suffix.then_some(mdns_uid_suffix);
            match mdns::register_services(
                mdns,
                device_ipv4_octets,
                device_ipv6,
                usb_serial,
                uid_suffix,
            ) {
                Ok(registration) => {
                    uart.blocking_write(b"mDNS announced ").unwrap();
                    uart.blocking_write(registration.hostname.as_bytes())
                        .unwrap();
                    uart.blocking_write(b".local\r\n").unwrap();
                    mdns_registration = Some(registration);
                }
                Err(_) => {
                    warn!("mDNS service registration failed");
                    uart.blocking_write(b"mDNS registration failed\r\n")
                        .unwrap();
                }
            }
        }

        // While the link is up, surface mDNS lifecycle events on the UART.
        // Probe conflicts rename or kill the registration silently otherwise,
        // which is indistinguishable from success in the log.
        #[cfg(feature = "mdns")]
        {
            let monitor = async {
                loop {
                    let packet_seen =
                        network::NET_RX_PACKETS.load(Ordering::Relaxed) != rx_after_host_probe;
                    if packet_seen && !host_ready {
                        host_ready = true;
                        network::clear_host_silence_reset_count();
                        uart.blocking_write(b"host traffic seen after startup\r\n")
                            .unwrap();
                    }

                    #[cfg(feature = "dhcp-server")]
                    let network_ready = {
                        let network_ready = dhcp::lease_active();
                        if network_ready && !dhcp_lease_logged {
                            dhcp_lease_logged = true;
                            uart.blocking_write(b"DHCP lease established\r\n").unwrap();
                        } else if !network_ready {
                            dhcp_lease_logged = false;
                        }
                        network_ready
                    };
                    #[cfg(not(feature = "dhcp-server"))]
                    let network_ready = host_ready;

                    if let (Some(mdns), Some(registration)) =
                        (mdns_state, mdns_registration.as_ref())
                    {
                        let mut retry_with_uid_suffix = false;
                        for (index, (service_name, handle)) in
                            registration.services().into_iter().enumerate()
                        {
                            while let Some(update) = mdns.poll_service_update(handle) {
                                match update {
                                    mdns::ServiceUpdate::Established => {
                                        mdns_service_ready[index] = true;
                                        uart.blocking_write(b"mDNS ").unwrap();
                                        uart.blocking_write(service_name).unwrap();
                                        uart.blocking_write(b" locally established\r\n").unwrap();
                                    }
                                    mdns::ServiceUpdate::Renamed(renamed) => {
                                        mdns_service_ready[index] = false;
                                        uart.blocking_write(b"mDNS service renamed to ").unwrap();
                                        uart.blocking_write(renamed.new_name().as_str().as_bytes())
                                            .unwrap();
                                        uart.blocking_write(b"\r\n").unwrap();
                                    }
                                    mdns::ServiceUpdate::Conflict => {
                                        mdns_service_ready[index] = false;
                                        uart.blocking_write(
                                            b"mDNS CONFLICT: unresolved, service dead\r\n",
                                        )
                                        .unwrap();
                                    }
                                    mdns::ServiceUpdate::HostConflict => {
                                        mdns_service_ready[index] = false;
                                        if mdns_uses_uid_suffix {
                                            uart.blocking_write(
                                                b"mDNS CONFLICT: UID host name claimed by peer\r\n",
                                            )
                                            .unwrap();
                                        } else {
                                            retry_with_uid_suffix = true;
                                            uart.blocking_write(
                                                b"mDNS host conflict; retrying with flash UID suffix\r\n",
                                            )
                                            .unwrap();
                                        }
                                    }
                                    _ => {
                                        uart.blocking_write(b"mDNS service update (unknown)\r\n")
                                            .unwrap();
                                    }
                                }
                            }
                        }

                        if retry_with_uid_suffix {
                            if let Some(registration) = mdns_registration.take() {
                                registration.unregister(mdns);
                            }
                            mdns_service_ready.fill(false);
                            mdns_uses_uid_suffix = true;

                            match mdns::register_services(
                                mdns,
                                device_ipv4_octets,
                                device_ipv6,
                                usb_serial,
                                Some(mdns_uid_suffix),
                            ) {
                                Ok(registration) => {
                                    uart.blocking_write(b"mDNS announced ").unwrap();
                                    uart.blocking_write(registration.hostname.as_bytes())
                                        .unwrap();
                                    uart.blocking_write(b".local\r\n").unwrap();
                                    mdns_registration = Some(registration);
                                }
                                Err(_) => {
                                    warn!("mDNS UID service registration failed");
                                    uart.blocking_write(b"mDNS UID registration failed\r\n")
                                        .unwrap();
                                }
                            }
                        }
                    }
                    if network_ready && mdns_service_ready.iter().all(|ready| *ready) {
                        status.set_ready();
                        if !network_ready_logged {
                            network_ready_logged = true;
                            uart.blocking_write(b"network ready\r\n").unwrap();
                        }
                    } else {
                        network_ready_logged = false;
                        status.set_busy();
                    }
                    Timer::after(Duration::from_millis(500)).await;
                }
            };
            select(stack.wait_link_down(), monitor).await;
        }
        #[cfg(not(feature = "mdns"))]
        {
            let monitor = async {
                loop {
                    let packet_seen =
                        network::NET_RX_PACKETS.load(Ordering::Relaxed) != rx_after_host_probe;
                    if packet_seen && !host_ready {
                        host_ready = true;
                        network::clear_host_silence_reset_count();
                        uart.blocking_write(b"host traffic seen after startup\r\n")
                            .unwrap();
                    }

                    #[cfg(feature = "dhcp-server")]
                    let network_ready = {
                        let network_ready = dhcp::lease_active();
                        if network_ready && !dhcp_lease_logged {
                            dhcp_lease_logged = true;
                            uart.blocking_write(b"DHCP lease established\r\n").unwrap();
                        } else if !network_ready {
                            dhcp_lease_logged = false;
                        }
                        network_ready
                    };
                    #[cfg(not(feature = "dhcp-server"))]
                    let network_ready = host_ready;

                    if network_ready {
                        status.set_ready();
                        if !network_ready_logged {
                            network_ready_logged = true;
                            uart.blocking_write(b"network ready\r\n").unwrap();
                        }
                    } else {
                        network_ready_logged = false;
                        status.set_busy();
                    }
                    Timer::after(Duration::from_millis(500)).await;
                }
            };
            select(stack.wait_link_down(), monitor).await;
        }

        info!("CDC-NCM link down");
        status.set_busy();
        host_silence_resets = 0;
        network::clear_host_silence_reset_count();
        #[cfg(feature = "dhcp-server")]
        dhcp::clear_lease();
        uart.blocking_write(b"CDC-NCM link down\r\n").unwrap();
        Timer::after(Duration::from_millis(100)).await;
    }
}
