//! MetaGeek Wi-Spy Original (Gen1) sweep assembly.

pub(crate) const VENDOR_ID: u16 = 0x1781;
pub(crate) const PRODUCT_ID: u16 = 0x083e;
pub(crate) const LEGACY_VENDOR_ID: u16 = 0x04b4;
pub(crate) const LEGACY_PRODUCT_ID: u16 = 0x0bad;

pub(crate) const REPORT_LEN: usize = 8;
pub(crate) const REPORT_DESCRIPTOR_LEN: usize = 48;
pub(crate) const SAMPLE_COUNT: usize = 83;
pub(crate) const START_MHZ: u16 = 2400;
pub(crate) const STEP_MHZ: u8 = 1;
pub(crate) const OFFSET_MDBM: i32 = -97_000;
pub(crate) const RESOLUTION_MDBM: i32 = 1_500;

pub(crate) const fn is_original(vendor_id: u16, product_id: u16) -> bool {
    (vendor_id == VENDOR_ID && product_id == PRODUCT_ID)
        || (vendor_id == LEGACY_VENDOR_ID && product_id == LEGACY_PRODUCT_ID)
}

#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SweepProgress {
    WaitingForStart,
    InProgress,
    Complete,
}

pub(crate) struct SweepAssembler {
    samples: [u8; SAMPLE_COUNT],
    synchronized: bool,
}

impl SweepAssembler {
    pub(crate) const fn new() -> Self {
        Self {
            samples: [0; SAMPLE_COUNT],
            synchronized: false,
        }
    }

    pub(crate) fn push(&mut self, report: &[u8; REPORT_LEN]) -> Result<SweepProgress, ()> {
        let start = usize::from(report[0]);
        if start >= SAMPLE_COUNT {
            return Err(());
        }
        if start == 0 {
            self.synchronized = true;
        } else if !self.synchronized {
            return Ok(SweepProgress::WaitingForStart);
        }

        for (offset, sample) in report[1..].iter().copied().enumerate() {
            let bin = start + offset;
            if bin < SAMPLE_COUNT {
                self.samples[bin] = sample;
            }
        }

        if start + (REPORT_LEN - 1) >= SAMPLE_COUNT {
            self.synchronized = false;
            Ok(SweepProgress::Complete)
        } else {
            Ok(SweepProgress::InProgress)
        }
    }

    pub(crate) const fn samples(&self) -> &[u8; SAMPLE_COUNT] {
        &self.samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_current_and_legacy_ids() {
        assert!(is_original(0x1781, 0x083e));
        assert!(is_original(0x04b4, 0x0bad));
        assert!(!is_original(0x1781, 0x083f));
    }

    #[test]
    fn assembles_only_complete_synchronized_sweeps() {
        let mut sweep = SweepAssembler::new();
        assert!(matches!(
            sweep.push(&[7, 1, 2, 3, 4, 5, 6, 7]),
            Ok(SweepProgress::WaitingForStart)
        ));

        for start in (0_u8..=77).step_by(7) {
            let report = [
                start,
                start,
                start + 1,
                start + 2,
                start + 3,
                start + 4,
                start + 5,
                start + 6,
            ];
            let progress = sweep.push(&report).unwrap();
            if start == 77 {
                assert_eq!(progress, SweepProgress::Complete);
            } else {
                assert_eq!(progress, SweepProgress::InProgress);
            }
        }

        assert_eq!(sweep.samples()[0], 0);
        assert_eq!(sweep.samples()[82], 82);
    }

    #[test]
    fn rejects_out_of_range_start_bin() {
        let mut sweep = SweepAssembler::new();
        assert!(sweep.push(&[83, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }
}
