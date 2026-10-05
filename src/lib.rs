//! Async `RigExpert` BLE client, file formats, and one-port RF analysis.
//!
//! Connect using [`Analyzer::connect`], then acquire with [`Analyzer::sweep`].
//! All frequencies in the library are Hz, impedances are ohms, distances metres.
//! Hardware-free callers can use [`Analyzer::demo`] or inject a [`Transport`].
pub mod analysis;
mod att;
mod client;
pub mod files;
pub mod protocol;
pub mod transport;
pub use client::Analyzer;
use serde::{Deserialize, Serialize};
pub use transport::{ConnectionOptions, Transport};

pub const DEFAULT_ADDRESS: &str = "04:91:62:AE:BA:ED";
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Bluetooth: {0}")]
    Bluetooth(String),
    #[error("Protocol: {0}")]
    Protocol(String),
    #[error("Invalid input: {0}")]
    Invalid(String),
    #[error("Timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("Device disconnected")]
    Disconnected,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
impl From<bluer::Error> for Error {
    fn from(e: bluer::Error) -> Self {
        Self::Bluetooth(e.to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub address: String,
    pub serial: String,
    pub firmware: String,
    pub min_hz: u64,
    pub max_hz: u64,
    /// Maximum number of intervals supported by the protocol.
    pub max_intervals: usize,
    pub packed: bool,
}
impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            name: "AA-650 ZOOM".into(),
            address: DEFAULT_ADDRESS.into(),
            serial: String::new(),
            firmware: String::new(),
            min_hz: 100_000,
            max_hz: 650_000_000,
            max_intervals: 500,
            packed: false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepSettings {
    pub start_hz: u64,
    pub stop_hz: u64,
    /// Number of returned samples, including both endpoints.
    pub samples: usize,
    pub z0: f64,
}
impl Default for SweepSettings {
    fn default() -> Self {
        Self {
            start_hz: 144_000_000,
            stop_hz: 146_000_000,
            samples: 201,
            z0: 50.0,
        }
    }
}
impl SweepSettings {
    ///
    /// # Errors
    /// Returns an error for frequencies outside device limits, an unsupported BLE grid, sample
    /// count, or invalid impedance.
    pub fn validate(&self, info: &DeviceInfo) -> Result<()> {
        if self.start_hz < info.min_hz || self.stop_hz > info.max_hz || self.start_hz > self.stop_hz
        {
            return Err(Error::Invalid(format!(
                "frequency must lie within {}..={} Hz",
                info.min_hz, info.max_hz
            )));
        }
        if !self.start_hz.is_multiple_of(1000)
            || !self.stop_hz.is_multiple_of(1000)
            || !(self.stop_hz - self.start_hz).is_multiple_of(2000)
        {
            return Err(Error::Invalid(
                "BLE endpoints must be whole kHz with an even-kHz span".into(),
            ));
        }
        if self.samples < 2 || self.samples > info.max_intervals.saturating_add(1) {
            return Err(Error::Invalid(format!(
                "samples must be 2..={}",
                info.max_intervals + 1
            )));
        }
        if !self.z0.is_finite() || self.z0 <= 0.0 {
            return Err(Error::Invalid(
                "reference impedance must be positive and finite".into(),
            ));
        }
        Ok(())
    }
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "RF frequencies and sample indices are represented approximately as f64; validated grid indices round back to integers"
    )]
    pub fn frequency(&self, index: usize) -> f64 {
        self.start_hz as f64
            + (self.stop_hz - self.start_hz) as f64 * index as f64 / (self.samples - 1) as f64
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub frequency_hz: f64,
    pub r: f64,
    pub x: f64,
}
impl Sample {
    #[must_use]
    pub fn impedance(&self) -> num_complex::Complex64 {
        num_complex::Complex64::new(self.r, self.x)
    }
    ///
    /// # Errors
    /// Returns an error for nonpositive or nonfinite frequency, negative or nonfinite
    /// resistance, or nonfinite reactance.
    pub fn validate(&self) -> Result<()> {
        if !self.frequency_hz.is_finite()
            || self.frequency_hz <= 0.0
            || !self.r.is_finite()
            || self.r < 0.0
            || !self.x.is_finite()
        {
            return Err(Error::Invalid(
                "sample needs positive frequency, nonnegative R, and finite R/X".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason")]
pub enum SweepStatus {
    Complete,
    Partial(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sweep {
    pub name: String,
    pub settings: SweepSettings,
    pub data: Vec<Sample>,
    pub status: SweepStatus,
    pub acquired_unix_seconds: u64,
    pub device: Option<DeviceInfo>,
}
impl Sweep {
    pub fn new(name: impl Into<String>, settings: SweepSettings) -> Self {
        Self {
            name: name.into(),
            settings,
            data: Vec::new(),
            status: SweepStatus::Partial("not acquired".into()),
            acquired_unix_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            device: None,
        }
    }
    ///
    /// # Errors
    /// Returns an error for invalid settings, missing or excess samples, invalid impedances,
    /// or unordered or out-of-range frequencies.
    #[expect(
        clippy::cast_precision_loss,
        reason = "RF frequencies and sample indices are represented approximately as f64; validated grid indices round back to integers"
    )]
    pub fn validate(&self) -> Result<()> {
        if self.settings.start_hz == 0
            || self.settings.samples < 2
            || self.settings.samples > 1_000_001
            || !self.settings.z0.is_finite()
            || self.settings.z0 <= 0.0
            || self.settings.start_hz > self.settings.stop_hz
        {
            return Err(Error::Invalid("invalid stored sweep settings".into()));
        }
        if self.data.len() > self.settings.samples {
            return Err(Error::Invalid("too many samples".into()));
        }
        if matches!(self.status, SweepStatus::Complete) && self.data.len() != self.settings.samples
        {
            return Err(Error::Invalid("complete sweep is missing samples".into()));
        }
        for sample in &self.data {
            sample.validate()?;
            if sample.frequency_hz < self.settings.start_hz as f64 - 1.0
                || sample.frequency_hz > self.settings.stop_hz as f64 + 1.0
            {
                return Err(Error::Invalid("stored sample outside sweep bounds".into()));
            }
        }
        for pair in self.data.windows(2) {
            if pair[0].frequency_hz > pair[1].frequency_hz {
                return Err(Error::Invalid("frequencies must be ordered".into()));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub slot: u8,
    pub name: String,
    pub settings: SweepSettings,
}
#[derive(Clone, Debug)]
pub enum Progress {
    Sample { index: usize, sample: Sample },
    Warning(String),
}
