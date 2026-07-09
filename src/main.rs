//! USB CDC-NCM network interface.

#![no_std]
#![no_main]

mod can;
#[cfg(feature = "dhcp-server")]
mod dhcp;
mod http;
#[cfg(feature = "mdns")]
mod mdns;
mod network;

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
use embassy_rp::uart::{Config as UartConfig, UartTx};
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
const FLASH_SIZE: usize = 2 * 1024 * 1024;
pub(crate) const HTTP_PORT: u16 = 80;
pub(crate) const HTTP_SOCKETS: usize = 3;
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
const HOST_SILENCE_RESET_LIMIT: u32 = 3;
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

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    #[cfg(feature = "mdns")]
    {
        static HEAP_MEM: StaticCell<[u8; HEAP_SIZE]> = StaticCell::new();
        let heap_mem = HEAP_MEM.init([0; HEAP_SIZE]);
        unsafe {
            HEAP.init(heap_mem.as_ptr() as usize, HEAP_SIZE);
        }
    }

    let mut uart = UartTx::new_blocking(p.UART0, p.PIN_0, UartConfig::default());
    uart.blocking_write(b"pico-can-bridge-rs boot\r\n").unwrap();

    let mut flash = Flash::<_, _, FLASH_SIZE>::new_blocking(p.FLASH);
    let mut flash_uid = [0; 16];
    if flash.blocking_unique_id(&mut flash_uid).is_err() {
        flash_uid = [
            b'p', b'i', b'c', b'o', b'-', b'c', b'a', b'n', b'-', b'b', b'r', b'i', b'd', b'g',
            b'e', 0,
        ];
        warn!("flash unique ID read failed, using fallback link-local seed");
    }

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

    let usb_driver = Driver::new(p.USB, Irqs);
    let mut usb_config = UsbConfig::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("pico-can-bridge-rs");
    usb_config.product = Some("Pico CAN Bridge CDC-NCM");
    usb_config.serial_number = Some("0001");
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

    static NET_RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
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
        spawner.spawn(http::http_task(stack).unwrap());
    }
    spawner.spawn(can::can_task(p.SPI1, p.PIN_14, p.PIN_15, p.PIN_8, p.PIN_19).unwrap());

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
    uart.blocking_write(b"CAN task starting, SPI1 SCK GP14 MOSI GP15 MISO GP8 CS GP19\r\n")
        .unwrap();

    #[cfg(feature = "mdns")]
    let mut mdns_state: Option<&'static mdns::MdnsState<mdns::MdnsRng>> = None;
    #[cfg(feature = "mdns")]
    let mut mdns_service: Option<mdns::ServiceHandle> = None;
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
                        cortex_m::peripheral::SCB::sys_reset();
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
            network::set_host_silence_reset_count(0);
            uart.blocking_write(b"host traffic seen\r\n").unwrap();
        } else if stack.is_link_up() {
            let resets = network::host_silence_reset_count();
            if resets < HOST_SILENCE_RESET_LIMIT {
                network::set_host_silence_reset_count(resets + 1);
                warn!("host silent after link-up, forcing USB re-enumeration");
                uart.blocking_write(b"host silent, re-enumerating (reset)\r\n")
                    .unwrap();
                uart.blocking_flush().unwrap();
                cortex_m::peripheral::SCB::sys_reset();
            } else {
                warn!("host still silent, reset limit reached, staying up");
                uart.blocking_write(b"host silent, reset limit reached\r\n")
                    .unwrap();
            }
        } else {
            // Link dropped while we were probing for host traffic.
            continue;
        }

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

            if let Some(handle) = mdns_service.take() {
                mdns.unregister_service(handle);
            }
            match mdns.register_service(mdns::ServiceSpec::new(mdns::build_mdns_records(
                device_ipv4_octets,
                device_ipv6,
            ))) {
                Ok(handle) => {
                    mdns_service = Some(handle);
                    uart.blocking_write(b"mDNS announced pico-can-bridge.local\r\n")
                        .unwrap();
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
                    if let (Some(mdns), Some(handle)) = (mdns_state, mdns_service) {
                        while let Some(update) = mdns.poll_service_update(handle) {
                            let line: &[u8] = match update {
                                mdns::ServiceUpdate::Established => b"mDNS service established\r\n",
                                mdns::ServiceUpdate::Renamed(_) => {
                                    b"mDNS CONFLICT: service renamed\r\n"
                                }
                                mdns::ServiceUpdate::Conflict => {
                                    b"mDNS CONFLICT: unresolved, service dead\r\n"
                                }
                                mdns::ServiceUpdate::HostConflict => {
                                    b"mDNS CONFLICT: host name claimed by peer\r\n"
                                }
                                _ => b"mDNS service update (unknown)\r\n",
                            };
                            uart.blocking_write(line).unwrap();
                        }
                    }
                    Timer::after(Duration::from_millis(500)).await;
                }
            };
            select(stack.wait_link_down(), monitor).await;
        }
        #[cfg(not(feature = "mdns"))]
        stack.wait_link_down().await;

        info!("CDC-NCM link down");
        uart.blocking_write(b"CDC-NCM link down\r\n").unwrap();
        Timer::after(Duration::from_millis(100)).await;
    }
}
