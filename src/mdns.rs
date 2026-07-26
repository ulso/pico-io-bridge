use alloc::vec::Vec;
use core::{convert::Infallible, net::Ipv4Addr};

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Ipv6Address, Stack};
use heapless::String;
pub(crate) use hick_embassy::MdnsState;
use mdns_proto::error::RegisterServiceError;
pub(crate) use mdns_proto::{EndpointConfig, ServiceHandle, ServiceSpec, ServiceUpdate};
use mdns_proto::{Name, ServiceRecords};
use rand_core::TryRng;
use static_cell::StaticCell;

pub(crate) const UID_SUFFIX_BYTES: usize = 6;
pub(crate) const SERVICE_COUNT: usize = 2;
const DNS_LABEL_BYTES: usize = 63;
const DNS_NAME_BYTES: usize = 128;
const HTTP_SERVICE_TYPE: &str = "_http._tcp.local.";
const SCPI_SERVICE_TYPE: &str = "_scpi-raw._tcp.local.";

pub(crate) struct MdnsRng(u64);

pub(crate) struct Registration {
    http_handle: ServiceHandle,
    scpi_handle: ServiceHandle,
    pub(crate) hostname: String<DNS_LABEL_BYTES>,
}

impl Registration {
    pub(crate) fn services(&self) -> [(&'static [u8], ServiceHandle); SERVICE_COUNT] {
        [
            (b"HTTP service", self.http_handle),
            (b"SCPI service", self.scpi_handle),
        ]
    }

    pub(crate) fn unregister(self, state: &MdnsState<MdnsRng>) {
        state.unregister_service(self.http_handle);
        state.unregister_service(self.scpi_handle);
    }
}

impl MdnsRng {
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

impl TryRng for MdnsRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(self.next() as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(self.next())
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        for chunk in dst.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }

        Ok(())
    }
}

pub(crate) fn encode_uid_suffix(uid: &[u8], suffix: &mut [u8; UID_SUFFIX_BYTES]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let uid = &uid[uid.len() - 3..];
    for (index, byte) in uid.iter().copied().enumerate() {
        suffix[index * 2] = HEX[(byte >> 4) as usize];
        suffix[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
}

fn label_with_optional_suffix(base: &str, uid_suffix: Option<&str>) -> String<DNS_LABEL_BYTES> {
    let mut label = String::new();
    label.push_str(base).unwrap();
    if let Some(uid_suffix) = uid_suffix {
        label.push('-').unwrap();
        label.push_str(uid_suffix).unwrap();
    }
    label
}

fn build_mdns_records(
    ipv4: [u8; 4],
    ipv6: Ipv6Address,
    hostname: &str,
    uid_suffix: Option<&str>,
    service_type: &str,
    port: u16,
) -> ServiceRecords {
    let service_instance =
        label_with_optional_suffix(crate::board::MDNS_SERVICE_INSTANCE, uid_suffix);

    let mut instance_name: String<DNS_NAME_BYTES> = String::new();
    instance_name.push_str(&service_instance).unwrap();
    instance_name.push('.').unwrap();
    instance_name.push_str(service_type).unwrap();

    let mut host_name: String<DNS_NAME_BYTES> = String::new();
    host_name.push_str(hostname).unwrap();
    host_name.push_str(".local.").unwrap();

    let mut records = ServiceRecords::new(
        Name::try_from_str(service_type).unwrap(),
        Name::try_from_str(&instance_name).unwrap(),
        Name::try_from_str(&host_name).unwrap(),
        port,
        crate::MDNS_TTL_SECS,
    );
    records.add_a(Ipv4Addr::new(ipv4[0], ipv4[1], ipv4[2], ipv4[3]));
    records.add_aaaa(ipv6);
    records
}

fn txt(key: &str, value: &str) -> Vec<u8> {
    let mut segment = Vec::with_capacity(key.len() + value.len() + 1);
    segment.extend_from_slice(key.as_bytes());
    segment.push(b'=');
    segment.extend_from_slice(value.as_bytes());
    segment
}

pub(crate) fn register_services(
    state: &MdnsState<MdnsRng>,
    ipv4: [u8; 4],
    ipv6: Ipv6Address,
    serial: &str,
    uid_suffix: Option<&str>,
) -> Result<Registration, RegisterServiceError> {
    let hostname = label_with_optional_suffix(crate::board::MDNS_HOST_LABEL, uid_suffix);
    let mut http_records = build_mdns_records(
        ipv4,
        ipv6,
        &hostname,
        uid_suffix,
        HTTP_SERVICE_TYPE,
        crate::HTTP_PORT,
    );
    http_records.add_txt_segment(txt("txtvers", "1"));
    http_records.add_txt_segment(txt("path", "/"));
    let http_handle = state.register_service(ServiceSpec::new(http_records))?;

    let mut scpi_records = build_mdns_records(
        ipv4,
        ipv6,
        &hostname,
        uid_suffix,
        SCPI_SERVICE_TYPE,
        crate::SCPI_PORT,
    );
    scpi_records.add_txt_segment(txt("txtvers", "1"));
    scpi_records.add_txt_segment(txt("Manufacturer", crate::MANUFACTURER));
    scpi_records.add_txt_segment(txt("Model", crate::board::BOARD_NAME));
    scpi_records.add_txt_segment(txt("SerialNumber", serial));
    scpi_records.add_txt_segment(txt("FirmwareVersion", crate::FIRMWARE_VERSION));
    let scpi_handle = match state.register_service(ServiceSpec::new(scpi_records)) {
        Ok(handle) => handle,
        Err(error) => {
            state.unregister_service(http_handle);
            return Err(error);
        }
    };

    Ok(Registration {
        http_handle,
        scpi_handle,
        hostname,
    })
}

#[embassy_executor::task]
pub(crate) async fn mdns_task(stack: Stack<'static>, state: &'static MdnsState<MdnsRng>) {
    static RX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static RX_META_V6: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static RX_BUF_V6: StaticCell<[u8; 2048]> = StaticCell::new();
    static TX_META_V6: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static TX_BUF_V6: StaticCell<[u8; 2048]> = StaticCell::new();
    static SCRATCH: StaticCell<[u8; 2048]> = StaticCell::new();

    let rx_meta = RX_META.init([PacketMetadata::EMPTY; 4]);
    let rx_buf = RX_BUF.init([0; 2048]);
    let tx_meta = TX_META.init([PacketMetadata::EMPTY; 4]);
    let tx_buf = TX_BUF.init([0; 2048]);
    let rx_meta_v6 = RX_META_V6.init([PacketMetadata::EMPTY; 4]);
    let rx_buf_v6 = RX_BUF_V6.init([0; 2048]);
    let tx_meta_v6 = TX_META_V6.init([PacketMetadata::EMPTY; 4]);
    let tx_buf_v6 = TX_BUF_V6.init([0; 2048]);
    let scratch = SCRATCH.init([0; 2048]);

    stack
        .join_multicast_group(IpAddress::v4(224, 0, 0, 251))
        .unwrap();
    stack
        .join_multicast_group(IpAddress::v6(0xff02, 0, 0, 0, 0, 0, 0, 0x00fb))
        .unwrap();

    let mut socket_v4 = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    socket_v4.bind(5353).unwrap();
    let mut socket_v6 = UdpSocket::new(stack, rx_meta_v6, rx_buf_v6, tx_meta_v6, tx_buf_v6);
    socket_v6.bind(5353).unwrap();

    defmt::info!("mDNS responder ready");
    state
        .run(Some(&mut socket_v4), Some(&mut socket_v6), scratch)
        .await;
}
