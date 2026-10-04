//! Regional amateur-band measurement presets within the AA-650 frequency range.
//! Sources: IARU Region 1 HF/VHF/UHF and Region 2 band plans linked in README.
use rigexpert::SweepSettings;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Regions {
    One,
    Two,
    Both,
}
impl Regions {
    const fn label(self) -> &'static str {
        match self {
            Self::One => "Region 1",
            Self::Two => "Region 2",
            Self::Both => "Regions 1 & 2",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Band {
    pub name: &'static str,
    pub region: Regions,
    pub start_hz: u64,
    pub stop_hz: u64,
}
impl Band {
    const fn new(name: &'static str, region: Regions, start_hz: u64, stop_hz: u64) -> Self {
        Self {
            name,
            region,
            start_hz,
            stop_hz,
        }
    }
    pub fn label(self) -> String {
        format!(
            "{:<7} {:<13}  {}",
            self.name,
            self.region.label(),
            range(self.start_hz, self.stop_hz)
        )
    }
    pub fn settings(self, current: SweepSettings) -> SweepSettings {
        // Cover the band edges with whole-kHz endpoints and an even-kHz span,
        // as required by the analyzer's integer-kHz center/span wire format.
        let start_hz = self.start_hz / 1000 * 1000;
        let mut stop_hz = self.stop_hz.div_ceil(1000) * 1000;
        if !(stop_hz - start_hz).is_multiple_of(2000) {
            stop_hz += 1000;
        }
        SweepSettings {
            start_hz,
            stop_hz,
            ..current
        }
    }
}
fn frequency(hz: u64) -> String {
    let value = format!("{}.{:06}", hz / 1_000_000, hz % 1_000_000);
    value.trim_end_matches('0').trim_end_matches('.').to_owned()
}
pub(super) fn range(start_hz: u64, stop_hz: u64) -> String {
    format!("{}–{} MHz", frequency(start_hz), frequency(stop_hz))
}

// Share matching ranges; keep differing regional entries adjacent.
pub(super) const BANDS: &[Band] = &[
    Band::new("2200 m", Regions::Both, 135_700, 137_800),
    Band::new("630 m", Regions::Both, 472_000, 479_000),
    Band::new("160 m", Regions::One, 1_810_000, 2_000_000),
    Band::new("160 m", Regions::Two, 1_800_000, 2_000_000),
    Band::new("80 m", Regions::One, 3_500_000, 3_800_000),
    Band::new("80 m", Regions::Two, 3_500_000, 4_000_000),
    Band::new("60 m", Regions::Both, 5_351_500, 5_366_500),
    Band::new("40 m", Regions::One, 7_000_000, 7_200_000),
    Band::new("40 m", Regions::Two, 7_000_000, 7_300_000),
    Band::new("30 m", Regions::Both, 10_100_000, 10_150_000),
    Band::new("20 m", Regions::Both, 14_000_000, 14_350_000),
    Band::new("17 m", Regions::Both, 18_068_000, 18_168_000),
    Band::new("15 m", Regions::Both, 21_000_000, 21_450_000),
    Band::new("12 m", Regions::Both, 24_890_000, 24_990_000),
    Band::new("10 m", Regions::Both, 28_000_000, 29_700_000),
    Band::new("6 m", Regions::Both, 50_000_000, 54_000_000),
    Band::new("4 m", Regions::One, 70_000_000, 70_500_000),
    Band::new("2 m", Regions::One, 144_000_000, 146_000_000),
    Band::new("2 m", Regions::Two, 144_000_000, 148_000_000),
    Band::new("1.25 m", Regions::Two, 220_000_000, 225_000_000),
    Band::new("70 cm", Regions::One, 430_000_000, 440_000_000),
    Band::new("70 cm", Regions::Two, 420_000_000, 450_000_000),
];

#[cfg(test)]
mod tests {
    use super::*;
    use rigexpert::DeviceInfo;

    #[test]
    fn matching_ranges_have_one_entry_for_both_regions() {
        for (index, band) in BANDS.iter().enumerate() {
            assert!(
                !BANDS[index + 1..].iter().any(|other| {
                    band.name == other.name
                        && band.start_hz == other.start_hz
                        && band.stop_hz == other.stop_hz
                }),
                "duplicate range: {}",
                band.label()
            );
        }
        let twenty = BANDS
            .iter()
            .filter(|band| band.name == "20 m")
            .collect::<Vec<_>>();
        assert_eq!(twenty.len(), 1);
        assert!(twenty[0].region == Regions::Both);
        assert!(twenty[0].label().contains("Regions 1 & 2"));
        let two = BANDS
            .iter()
            .filter(|band| band.name == "2 m")
            .collect::<Vec<_>>();
        assert_eq!(two.len(), 2);
        assert!(two[0].region == Regions::One);
        assert!(two[1].region == Regions::Two);
        assert_ne!(two[0].stop_hz, two[1].stop_hz);
    }

    #[test]
    fn every_band_covers_its_edges_on_a_valid_scan_grid() {
        for band in BANDS {
            let settings = band.settings(SweepSettings::default());
            settings.validate(&DeviceInfo::default()).unwrap();
            assert!(settings.start_hz <= band.start_hz, "{}", band.label());
            assert!(settings.stop_hz >= band.stop_hz, "{}", band.label());
            assert!(band.start_hz - settings.start_hz < 1000);
            assert!(settings.stop_hz - band.stop_hz < 2000);
        }
        let sixty = BANDS.iter().find(|band| band.name == "60 m").unwrap();
        let settings = sixty.settings(SweepSettings::default());
        assert_eq!(
            (settings.start_hz, settings.stop_hz),
            (5_351_000, 5_367_000)
        );
        assert!(sixty.label().contains("5.3515–5.3665 MHz"));
    }
}
