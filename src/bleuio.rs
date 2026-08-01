//! BleuIO discovery stream and HibouAir manufacturer-data decoding.

pub(crate) const VENDOR_ID: u16 = 0x2dcf;
pub(crate) const PRODUCT_ID: u16 = 0x6002;
pub(crate) const MAX_SENSORS: usize = 8;
const MAX_LINE: usize = 384;
const HIBOUAIR_MANUFACTURER_ID: u16 = 0x075b;
const HIBOUAIR_BEACON: u8 = 0x05;

pub(crate) const fn is_bleuio(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == VENDOR_ID && product_id == PRODUCT_ID
}

pub(crate) fn parse_board_id(value: &str) -> Option<u32> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        return None;
    }
    let mut board_id = 0_u32;
    for byte in value.bytes() {
        board_id = (board_id << 4) | u32::from(hex_digit(byte)?);
    }
    Some(board_id)
}

#[derive(Clone, Copy)]
pub(crate) struct Filter {
    pub(crate) ids: [Option<u32>; MAX_SENSORS],
}

impl Filter {
    pub(crate) const fn all() -> Self {
        Self {
            ids: [None; MAX_SENSORS],
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("ALL") || value.is_empty() {
            return Some(Self::all());
        }

        let mut filter = Self::all();
        let mut count = 0;
        for token in value.split(',') {
            let board_id = parse_board_id(token.trim())?;
            if filter.ids[..count].contains(&Some(board_id)) {
                continue;
            }
            if count == filter.ids.len() {
                return None;
            }
            filter.ids[count] = Some(board_id);
            count += 1;
        }
        (count != 0).then_some(filter)
    }

    pub(crate) fn is_all(self) -> bool {
        self.ids[0].is_none()
    }

    pub(crate) fn contains(self, board_id: u32) -> bool {
        self.is_all() || self.ids.contains(&Some(board_id))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SensorType {
    TemperatureHumidity,
    ParticulateMatter,
    Co2,
    No2OutdoorWifi,
    Co2Battery,
    No2OutdoorLte,
    Pir,
    Co2Noise,
    DuoMaster,
    DuoSlave,
    Matrix,
    Unknown(u8),
}

impl SensorType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TemperatureHumidity => "Temperature / humidity",
            Self::ParticulateMatter => "Particulate matter",
            Self::Co2 => "CO2",
            Self::No2OutdoorWifi => "NO2 outdoor Wi-Fi",
            Self::Co2Battery => "CO2 battery",
            Self::No2OutdoorLte => "NO2 outdoor LTE",
            Self::Pir => "PIR",
            Self::Co2Noise => "CO2 / noise",
            Self::DuoMaster => "Duo master",
            Self::DuoSlave => "Duo slave",
            Self::Matrix => "Matrix",
            Self::Unknown(_) => "HibouAir sensor",
        }
    }

    pub(crate) const fn token(self) -> &'static str {
        match self {
            Self::TemperatureHumidity => "TEMP_HUM",
            Self::ParticulateMatter => "PARTICULATE",
            Self::Co2 => "CO2",
            Self::No2OutdoorWifi => "NO2_WIFI",
            Self::Co2Battery => "CO2_BATTERY",
            Self::No2OutdoorLte => "NO2_LTE",
            Self::Pir => "PIR",
            Self::Co2Noise => "CO2_NOISE",
            Self::DuoMaster => "DUO_MASTER",
            Self::DuoSlave => "DUO_SLAVE",
            Self::Matrix => "MATRIX",
            Self::Unknown(_) => "UNKNOWN",
        }
    }

    pub(crate) const fn has_particulate(self) -> bool {
        matches!(self, Self::ParticulateMatter)
    }

    pub(crate) const fn has_co2(self) -> bool {
        matches!(
            self,
            Self::Co2 | Self::Co2Battery | Self::Co2Noise | Self::DuoMaster | Self::DuoSlave
        )
    }

    pub(crate) const fn has_noise(self) -> bool {
        matches!(self, Self::Co2Noise)
    }

    pub(crate) const fn has_ambient_light(self) -> bool {
        !self.has_noise()
    }
}

impl From<u8> for SensorType {
    fn from(value: u8) -> Self {
        match value {
            0x02 => Self::TemperatureHumidity,
            0x03 => Self::ParticulateMatter,
            0x04 => Self::Co2,
            0x05 => Self::No2OutdoorWifi,
            0x06 => Self::Co2Battery,
            0x07 => Self::No2OutdoorLte,
            0x08 => Self::Pir,
            0x09 => Self::Co2Noise,
            0x0a => Self::DuoMaster,
            0x0b => Self::DuoSlave,
            0x14 => Self::Matrix,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Reading {
    pub(crate) board_id: u32,
    pub(crate) sensor_type: SensorType,
    pub(crate) ambient_light: u16,
    pub(crate) noise_db_spl: u16,
    pub(crate) pressure_tenths_hpa: u16,
    pub(crate) temperature_tenths_c: i16,
    pub(crate) humidity_tenths_percent: u16,
    pub(crate) voc_raw: u16,
    pub(crate) voc_type: u8,
    pub(crate) pm1_tenths: u16,
    pub(crate) pm25_tenths: u16,
    pub(crate) pm10_tenths: u16,
    pub(crate) co2_ppm: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct Sensor {
    pub(crate) reading: Reading,
    pub(crate) last_seen_ms: u64,
    pub(crate) reports: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct Catalog {
    pub(crate) sensors: [Option<Sensor>; MAX_SENSORS],
    pub(crate) update_count: u32,
}

impl Catalog {
    pub(crate) const fn new() -> Self {
        Self {
            sensors: [None; MAX_SENSORS],
            update_count: 0,
        }
    }

    pub(crate) fn update(&mut self, reading: Reading, now_ms: u64, filter: Filter) {
        if !filter.contains(reading.board_id) {
            return;
        }
        let mut empty = None;
        let mut oldest = 0;
        for (index, sensor) in self.sensors.iter().enumerate() {
            match sensor {
                Some(sensor) if sensor.reading.board_id == reading.board_id => {
                    let reports = sensor.reports.wrapping_add(1);
                    self.sensors[index] = Some(Sensor {
                        reading,
                        last_seen_ms: now_ms,
                        reports,
                    });
                    self.update_count = self.update_count.wrapping_add(1);
                    return;
                }
                None if empty.is_none() => empty = Some(index),
                Some(sensor) => {
                    let oldest_seen =
                        self.sensors[oldest].map_or(u64::MAX, |oldest| oldest.last_seen_ms);
                    if sensor.last_seen_ms < oldest_seen {
                        oldest = index;
                    }
                }
                None => {}
            }
        }

        let index = empty.unwrap_or(oldest);
        self.sensors[index] = Some(Sensor {
            reading,
            last_seen_ms: now_ms,
            reports: 1,
        });
        self.update_count = self.update_count.wrapping_add(1);
    }

    pub(crate) fn retain_filter(&mut self, filter: Filter) {
        if filter.is_all() {
            return;
        }
        for sensor in &mut self.sensors {
            if sensor.is_some_and(|sensor| !filter.contains(sensor.reading.board_id)) {
                *sensor = None;
            }
        }
    }

    pub(crate) fn find(self, board_id: u32) -> Option<Sensor> {
        self.sensors
            .into_iter()
            .flatten()
            .find(|sensor| sensor.reading.board_id == board_id)
    }
}

pub(crate) struct Parser {
    line: [u8; MAX_LINE],
    length: usize,
    overflow: bool,
}

impl Parser {
    pub(crate) const fn new() -> Self {
        Self {
            line: [0; MAX_LINE],
            length: 0,
            overflow: false,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.length = 0;
        self.overflow = false;
    }

    pub(crate) fn push<F>(&mut self, bytes: &[u8], mut publish: F)
    where
        F: FnMut(Reading),
    {
        for &byte in bytes {
            if byte == b'\n' {
                if !self.overflow
                    && let Some(reading) = parse_line(&self.line[..self.length])
                {
                    publish(reading);
                }
                self.reset();
            } else if byte != b'\r' {
                if self.length < self.line.len() {
                    self.line[self.length] = byte;
                    self.length += 1;
                } else {
                    self.overflow = true;
                }
            }
        }
    }
}

fn parse_line(line: &[u8]) -> Option<Reading> {
    let marker = b"\"data\":\"";
    let start = find(line, marker)? + marker.len();
    let end = line[start..].iter().position(|&byte| byte == b'\"')? + start;
    parse_advertisement_hex(&line[start..end])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_advertisement_hex(hex: &[u8]) -> Option<Reading> {
    let mut bytes = [0_u8; 64];
    if !hex.len().is_multiple_of(2) || hex.len() / 2 > bytes.len() {
        return None;
    }
    for (index, pair) in hex.chunks_exact(2).enumerate() {
        bytes[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    let length = hex.len() / 2;
    if length < 31 {
        return None;
    }
    parse_hibouair(&bytes[5..31])
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn parse_hibouair(bytes: &[u8]) -> Option<Reading> {
    if bytes.len() < 26 || le16(bytes, 0) != HIBOUAIR_MANUFACTURER_ID || bytes[2] != HIBOUAIR_BEACON
    {
        return None;
    }

    let als_noise = le16(bytes, 7);
    Some(Reading {
        board_id: u32::from(bytes[4]) << 16 | u32::from(bytes[5]) << 8 | u32::from(bytes[6]),
        sensor_type: bytes[3].into(),
        ambient_light: als_noise,
        noise_db_spl: 120_u16.saturating_sub(als_noise.swap_bytes()),
        pressure_tenths_hpa: le16(bytes, 9),
        temperature_tenths_c: le16(bytes, 11) as i16,
        humidity_tenths_percent: le16(bytes, 13),
        voc_raw: le16(bytes, 15),
        pm1_tenths: le16(bytes, 17),
        pm25_tenths: le16(bytes, 19),
        pm10_tenths: le16(bytes, 21),
        co2_ppm: le16(bytes, 23).swap_bytes(),
        voc_type: bytes[25],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fragmented_hibouair_verbose_response() {
        let mut parser = Parser::new();
        let mut result = None;
        parser.push(
            b"{\"C\":\"SF\",\"data\":\"0201061BFF5B07050422005A0000BA27C60017",
            |reading| result = Some(reading),
        );
        assert!(result.is_none());
        parser.push(b"013E0000000000000001C002\"}\r\n", |reading| {
            result = Some(reading)
        });
        let reading = result.unwrap();
        assert_eq!(reading.board_id, 0x22005a);
        assert!(matches!(reading.sensor_type, SensorType::Co2));
        assert_eq!(reading.pressure_tenths_hpa, 10_170);
        assert_eq!(reading.temperature_tenths_c, 198);
        assert_eq!(reading.humidity_tenths_percent, 279);
        assert_eq!(reading.voc_raw, 62);
        assert_eq!(reading.co2_ppm, 448);
        assert_eq!(reading.voc_type, 2);
    }

    #[test]
    fn catalog_updates_existing_sensor() {
        let reading = parse_advertisement_hex(
            b"0201061BFF5B07050422005A0000BA27C60017013E0000000000000001C002",
        )
        .unwrap();
        let mut catalog = Catalog::new();
        catalog.update(reading, 10, Filter::all());
        catalog.update(reading, 20, Filter::all());
        let sensor = catalog.sensors[0].unwrap();
        assert_eq!(sensor.reports, 2);
        assert_eq!(sensor.last_seen_ms, 20);
    }

    #[test]
    fn converts_inverted_noise_advertisement_value() {
        let mut payload = [0_u8; 26];
        payload[0..2].copy_from_slice(&HIBOUAIR_MANUFACTURER_ID.to_le_bytes());
        payload[2] = HIBOUAIR_BEACON;
        payload[3] = 0x09;
        payload[7..9].copy_from_slice(&83_u16.swap_bytes().to_le_bytes());
        let reading = parse_hibouair(&payload).unwrap();
        assert_eq!(reading.noise_db_spl, 37);
    }

    #[test]
    fn parses_and_applies_sensor_filter() {
        let filter = Filter::parse("22005a,#22008C,22005A").unwrap();
        assert_eq!(filter.ids[0], Some(0x22005a));
        assert_eq!(filter.ids[1], Some(0x22008c));
        assert!(filter.ids[2].is_none());
        assert!(filter.contains(0x22005a));
        assert!(!filter.contains(0x22026a));
        assert!(Filter::parse("22005").is_none());
        assert!(Filter::parse("ALL").unwrap().is_all());
    }

    #[test]
    fn filtered_catalog_retains_only_whitelisted_sensors() {
        let mut first = parse_advertisement_hex(
            b"0201061BFF5B07050422005A0000BA27C60017013E0000000000000001C002",
        )
        .unwrap();
        let mut second = first;
        second.board_id = 0x22008c;
        let mut catalog = Catalog::new();
        catalog.update(first, 10, Filter::all());
        catalog.update(second, 20, Filter::all());

        let filter = Filter::parse("22008C").unwrap();
        catalog.retain_filter(filter);
        catalog.update(first, 30, filter);

        assert!(catalog.find(0x22005a).is_none());
        assert_eq!(catalog.find(0x22008c).unwrap().last_seen_ms, 20);
        first.board_id = 0x22026a;
        catalog.update(first, 40, filter);
        assert!(catalog.find(0x22026a).is_none());
    }
}
