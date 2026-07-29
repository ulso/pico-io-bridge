//! Original Velleman K8055/P8055 eight-byte HID report protocol.

pub(crate) const VELLEMAN_VENDOR_ID: u16 = 0x10cf;
pub(crate) const PRODUCT_ID_BASE: u16 = 0x5500;
pub(crate) const REPORT_LEN: usize = 8;
pub(crate) const MAX_DEBOUNCE_MICROS: u32 = 115 * u8::MAX as u32 * u8::MAX as u32;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct InputReport {
    bytes: [u8; REPORT_LEN],
}

impl InputReport {
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            bytes: <[u8; REPORT_LEN]>::try_from(bytes).ok()?,
        })
    }

    /// Digital inputs I1..I5 in bits 0..4.
    ///
    /// Open inputs are one and grounded inputs are zero, matching the board's
    /// electrical convention.
    pub(crate) const fn digital_inputs(self) -> u8 {
        let raw = self.bytes[0];
        ((raw >> 4) & 0x03) | ((raw << 2) & 0x04) | ((raw >> 3) & 0x18)
    }

    pub(crate) const fn analog_input_1(self) -> u8 {
        self.bytes[2]
    }

    pub(crate) const fn analog_input_2(self) -> u8 {
        self.bytes[3]
    }

    pub(crate) const fn counter_1(self) -> u16 {
        u16::from_le_bytes([self.bytes[4], self.bytes[5]])
    }

    pub(crate) const fn counter_2(self) -> u16 {
        u16::from_le_bytes([self.bytes[6], self.bytes[7]])
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct OutputState {
    pub(crate) digital_outputs: u8,
    pub(crate) analog_output_1: u8,
    pub(crate) analog_output_2: u8,
}

impl OutputState {
    pub(crate) const fn all_off() -> Self {
        Self {
            digital_outputs: 0,
            analog_output_1: 0,
            analog_output_2: 0,
        }
    }

    pub(crate) const fn reset_report() -> [u8; REPORT_LEN] {
        [0; REPORT_LEN]
    }

    pub(crate) const fn apply_report(self) -> [u8; REPORT_LEN] {
        [
            5,
            self.digital_outputs,
            self.analog_output_1,
            self.analog_output_2,
            0,
            0,
            0,
            0,
        ]
    }

    pub(crate) const fn reset_counter_report(self, channel: u8) -> [u8; REPORT_LEN] {
        [
            channel + 2,
            self.digital_outputs,
            self.analog_output_1,
            self.analog_output_2,
            0,
            0,
            0,
            0,
        ]
    }

    pub(crate) fn set_debounce_report(self, channel: u8, microseconds: u32) -> [u8; REPORT_LEN] {
        let mut report = [
            channel,
            self.digital_outputs,
            self.analog_output_1,
            self.analog_output_2,
            0,
            0,
            0,
            0,
        ];
        report[usize::from(channel) + 5] = debounce_raw(microseconds);
        report
    }
}

pub(crate) fn quantized_debounce_micros(microseconds: u32) -> u32 {
    let raw = u32::from(debounce_raw(microseconds));
    115 * raw * raw
}

fn debounce_raw(microseconds: u32) -> u8 {
    let mut raw = 0_u16;
    loop {
        let current = 115_u32 * u32::from(raw) * u32::from(raw);
        if raw == u16::from(u8::MAX) {
            return u8::MAX;
        }
        let next_raw = raw + 1;
        let next = 115_u32 * u32::from(next_raw) * u32::from(next_raw);
        if current.abs_diff(microseconds) <= next.abs_diff(microseconds) {
            return raw as u8;
        }
        raw = next_raw;
    }
}

pub(crate) const fn is_original(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == VELLEMAN_VENDOR_ID
        && product_id >= PRODUCT_ID_BASE
        && product_id <= PRODUCT_ID_BASE + 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_original_product_ids() {
        for product_id in 0x5500..=0x5503 {
            assert!(is_original(0x10cf, product_id));
        }
        assert!(!is_original(0x10cf, 0x54ff));
        assert!(!is_original(0x10cf, 0x5504));
        assert!(!is_original(0xffff, 0x5500));
    }

    #[test]
    fn decodes_input_bits_analogs_and_little_endian_counters() {
        let input = InputReport::parse(&[0xb5, 7, 23, 42, 0x34, 0x12, 0xcd, 0xab]).unwrap();
        assert_eq!(input.digital_inputs(), 0b10111);
        assert_eq!(input.analog_input_1(), 23);
        assert_eq!(input.analog_input_2(), 42);
        assert_eq!(input.counter_1(), 0x1234);
        assert_eq!(input.counter_2(), 0xabcd);
        assert!(InputReport::parse(&[0; REPORT_LEN - 1]).is_none());
    }

    #[test]
    fn preserves_outputs_in_control_reports() {
        assert_eq!(
            OutputState::all_off().apply_report(),
            [5, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(OutputState::reset_report(), [0; REPORT_LEN]);
        let output = OutputState {
            digital_outputs: 85,
            analog_output_1: 17,
            analog_output_2: 29,
        };
        assert_eq!(output.apply_report(), [5, 85, 17, 29, 0, 0, 0, 0]);
        assert_eq!(output.reset_counter_report(1), [3, 85, 17, 29, 0, 0, 0, 0]);
        assert_eq!(output.reset_counter_report(2), [4, 85, 17, 29, 0, 0, 0, 0]);
        assert_eq!(
            output.set_debounce_report(1, 2875),
            [1, 85, 17, 29, 0, 0, 5, 0]
        );
        assert_eq!(
            output.set_debounce_report(2, 2875),
            [2, 85, 17, 29, 0, 0, 0, 5]
        );
    }

    #[test]
    fn reports_quantized_debounce_value() {
        assert_eq!(quantized_debounce_micros(0), 0);
        assert_eq!(quantized_debounce_micros(2875), 2875);
        assert_eq!(
            quantized_debounce_micros(MAX_DEBOUNCE_MICROS),
            MAX_DEBOUNCE_MICROS
        );
    }
}
