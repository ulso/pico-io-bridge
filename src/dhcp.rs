use core::net::Ipv4Addr;

use embassy_net::Stack;
use leasehund::{DhcpConfigBuilder, DhcpServer, TransactionEvent};

const DHCP_LEASE_SECS: u32 = 3600;
const DHCP_MAX_CLIENTS: usize = 2;
const DHCP_MAX_DNS: usize = 1;

#[embassy_executor::task]
pub(crate) async fn dhcp_task(stack: Stack<'static>, device_ipv4: [u8; 4], host_ipv4: [u8; 4]) {
    let device_ip = Ipv4Addr::new(
        device_ipv4[0],
        device_ipv4[1],
        device_ipv4[2],
        device_ipv4[3],
    );
    let host_ip = Ipv4Addr::new(host_ipv4[0], host_ipv4[1], host_ipv4[2], host_ipv4[3]);

    let config = DhcpConfigBuilder::<DHCP_MAX_DNS>::new()
        .server_ip(device_ip)
        .subnet_mask(Ipv4Addr::new(255, 255, 255, 0))
        .no_router()
        .ip_pool(host_ip, host_ip)
        .lease_time(DHCP_LEASE_SECS)
        .build();
    let mut server = DhcpServer::<DHCP_MAX_CLIENTS, DHCP_MAX_DNS>::with_config(config);

    defmt::info!(
        "DHCP server ready, device {}.{}.{}.{}, host lease {}.{}.{}.{}/24",
        device_ipv4[0],
        device_ipv4[1],
        device_ipv4[2],
        device_ipv4[3],
        host_ipv4[0],
        host_ipv4[1],
        host_ipv4[2],
        host_ipv4[3]
    );

    server
        .run_with_callback(stack, |event| match event {
            TransactionEvent::Leased(ip, mac) => {
                let octets = ip.octets();
                defmt::info!(
                    "DHCP lease {}.{}.{}.{} to {=[u8]:02x}",
                    octets[0],
                    octets[1],
                    octets[2],
                    octets[3],
                    mac
                );
            }
            TransactionEvent::Released(ip, mac) => {
                let octets = ip.octets();
                defmt::info!(
                    "DHCP release {}.{}.{}.{} from {=[u8]:02x}",
                    octets[0],
                    octets[1],
                    octets[2],
                    octets[3],
                    mac
                );
            }
        })
        .await;
}
