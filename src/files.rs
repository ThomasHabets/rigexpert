//! Versioned JSON sessions, impedance CSV, and Touchstone 1.0 one-port S data.
use crate::{
    DeviceInfo, Error, Result, Sample, Sweep, SweepSettings, SweepStatus,
    analysis::{self, CableSettings},
};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub sweeps: Vec<Sweep>,
    pub cable: CableSettings,
}
impl Default for Session {
    fn default() -> Self {
        Self {
            version: 1,
            sweeps: Vec::new(),
            cable: Default::default(),
        }
    }
}
impl Session {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::Invalid(format!(
                "unsupported session version {}",
                self.version
            )));
        }
        self.cable.validate()?;
        for s in &self.sweeps {
            s.validate()?;
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
struct Metadata {
    name: String,
    settings: SweepSettings,
    status: SweepStatus,
    acquired_unix_seconds: u64,
    device: Option<DeviceInfo>,
}
impl Metadata {
    fn from_sweep(s: &Sweep) -> Self {
        Self {
            name: s.name.clone(),
            settings: s.settings,
            status: s.status.clone(),
            acquired_unix_seconds: s.acquired_unix_seconds,
            device: s.device.clone(),
        }
    }
    fn sweep(self, data: Vec<Sample>) -> Sweep {
        Sweep {
            name: self.name,
            settings: self.settings,
            status: self.status,
            acquired_unix_seconds: self.acquired_unix_seconds,
            device: self.device,
            data,
        }
    }
}
/// Writes via a temporary file in the same directory. Existing paths are rejected
/// unless overwrite is explicitly requested; incomplete writes never replace data.
fn write_file(path: &Path, data: &[u8], overwrite: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Error::Invalid("missing output filename".into()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let outcome = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        f.write_all(data)?;
        f.sync_all()?;
        if overwrite {
            std::fs::rename(&temporary, path)?;
        } else {
            std::fs::hard_link(&temporary, path)?;
            std::fs::remove_file(&temporary)?;
        }
        Ok(())
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    outcome
}
pub fn save_session(path: &Path, session: &Session, overwrite: bool) -> Result<()> {
    session.validate()?;
    write_file(path, &serde_json::to_vec_pretty(session)?, overwrite)
}
pub fn load_session(path: &Path) -> Result<Session> {
    let s: Session = serde_json::from_str(&read_text(path)?)?;
    s.validate()?;
    Ok(s)
}
fn read_text(path: &Path) -> Result<String> {
    if std::fs::metadata(path)?.len() > 64 * 1024 * 1024 {
        return Err(Error::Invalid("input file exceeds 64 MiB".into()));
    }
    Ok(std::fs::read_to_string(path)?)
}
pub fn export_csv(path: &Path, sweep: &Sweep, overwrite: bool) -> Result<()> {
    sweep.validate()?;
    let mut text = format!(
        "# rigexpert {}\nfrequency_hz,r_ohm,x_ohm\n",
        serde_json::to_string(&Metadata::from_sweep(sweep))?
    );
    for s in &sweep.data {
        text.push_str(&format!("{:.9},{:.12},{:.12}\n", s.frequency_hz, s.r, s.x));
    }
    write_file(path, text.as_bytes(), overwrite)
}
pub fn export_touchstone(path: &Path, sweep: &Sweep, overwrite: bool) -> Result<()> {
    sweep.validate()?;
    let mut text = format!(
        "! rigexpert {}\n# Hz S RI R {}\n",
        serde_json::to_string(&Metadata::from_sweep(sweep))?,
        sweep.settings.z0
    );
    for s in &sweep.data {
        let g = analysis::reflection(s, sweep.settings.z0)?;
        text.push_str(&format!(
            "{:.9} {:.16} {:.16}\n",
            s.frequency_hz, g.re, g.im
        ));
    }
    write_file(path, text.as_bytes(), overwrite)
}
fn imported(path: &Path, data: Vec<Sample>, z0: f64, metadata: Option<Metadata>) -> Result<Sweep> {
    if data.len() < 2 && metadata.is_none() {
        return Err(Error::Invalid(
            "file needs at least two samples or RigExpert metadata".into(),
        ));
    }
    let s = if let Some(m) = metadata {
        m.sweep(data)
    } else {
        let settings = SweepSettings {
            start_hz: data[0].frequency_hz.round() as u64,
            stop_hz: data.last().unwrap().frequency_hz.round() as u64,
            samples: data.len(),
            z0,
        };
        let mut s = Sweep::new(
            path.file_stem().unwrap_or_default().to_string_lossy(),
            settings,
        );
        s.data = data;
        s.status = SweepStatus::Complete;
        s
    };
    s.validate()?;
    Ok(s)
}
fn numbers(line: &str, separator: Option<char>) -> Result<Vec<f64>> {
    let words: Vec<_> = match separator {
        Some(c) => line.split(c).collect(),
        None => line.split_whitespace().collect(),
    };
    words
        .iter()
        .map(|v| {
            v.trim()
                .parse::<f64>()
                .map_err(|_| Error::Invalid(format!("invalid number {v:?}")))
        })
        .collect()
}
pub fn import_csv(path: &Path, z0: f64) -> Result<Sweep> {
    let mut metadata = None;
    let mut data = Vec::new();
    for (line_number, line) in read_text(path)?.lines().enumerate() {
        let line = line.trim();
        if let Some(m) = line.strip_prefix("# rigexpert ") {
            metadata = Some(serde_json::from_str(m)?);
        } else if line.is_empty() || line.starts_with('#') || line == "frequency_hz,r_ohm,x_ohm" {
            continue;
        } else {
            let n = numbers(line, Some(','))?;
            if n.len() != 3 {
                return Err(Error::Invalid(format!(
                    "CSV line {} needs frequency_hz,r_ohm,x_ohm",
                    line_number + 1
                )));
            }
            data.push(Sample {
                frequency_hz: n[0],
                r: n[1],
                x: n[2],
            });
        }
    }
    imported(path, data, z0, metadata)
}
pub fn import_touchstone(path: &Path) -> Result<Sweep> {
    let mut metadata = None;
    let mut data = Vec::new();
    let mut scale = 1e9;
    let mut format = "MA".to_string();
    let mut z0: f64 = 50.0;
    for line in read_text(path)?.lines() {
        let line = line.trim();
        if let Some(m) = line.strip_prefix("! rigexpert ") {
            metadata = Some(serde_json::from_str::<Metadata>(m)?);
            continue;
        }
        let line = line.split('!').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            return Err(Error::Invalid(
                "only Touchstone 1.0 one-port S data is supported".into(),
            ));
        }
        if let Some(options) = line.strip_prefix('#') {
            if !data.is_empty() {
                return Err(Error::Invalid("Touchstone option line after data".into()));
            }
            let words: Vec<_> = options
                .split_whitespace()
                .map(str::to_ascii_uppercase)
                .collect();
            if words.len() != 5 || words[1] != "S" || words[3] != "R" {
                return Err(Error::Invalid(
                    "expected # Hz|kHz|MHz|GHz S RI|MA|DB R impedance".into(),
                ));
            }
            scale = match words[0].as_str() {
                "HZ" => 1.0,
                "KHZ" => 1e3,
                "MHZ" => 1e6,
                "GHZ" => 1e9,
                _ => return Err(Error::Invalid("unknown Touchstone frequency unit".into())),
            };
            format = words[2].clone();
            if !["RI", "MA", "DB"].contains(&format.as_str()) {
                return Err(Error::Invalid("unknown Touchstone format".into()));
            }
            z0 = words[4]
                .parse()
                .map_err(|_| Error::Invalid("invalid Touchstone reference impedance".into()))?;
            if !z0.is_finite() || z0 <= 0.0 {
                return Err(Error::Invalid(
                    "invalid Touchstone reference impedance".into(),
                ));
            }
        } else {
            let n = numbers(line, None)?;
            if n.len() != 3 || n.iter().any(|n| !n.is_finite()) {
                return Err(Error::Invalid(
                    "expected one-port frequency/S11 data".into(),
                ));
            }
            let g = match format.as_str() {
                "RI" => num_complex::Complex64::new(n[1], n[2]),
                "MA" => num_complex::Complex64::from_polar(n[1], n[2].to_radians()),
                _ => num_complex::Complex64::from_polar(10f64.powf(n[1] / 20.0), n[2].to_radians()),
            };
            let z = z0 * (1.0 + g) / (1.0 - g);
            let r = if z.re < 0.0 && z.re > -1e-8 {
                0.0
            } else {
                z.re
            };
            data.push(Sample {
                frequency_hz: n[0] * scale,
                r,
                x: z.im,
            });
        }
    }
    if let Some(m) = &metadata
        && (m.settings.z0 - z0).abs() > 1e-9
    {
        return Err(Error::Invalid(
            "Touchstone metadata reference impedance mismatch".into(),
        ));
    }
    imported(path, data, z0, metadata)
}
pub fn load(path: &Path, z0: f64) -> Result<Session> {
    let extension = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    match extension.as_str() {
        "json" => load_session(path),
        "csv" => Ok(Session {
            sweeps: vec![import_csv(path, z0)?],
            ..Default::default()
        }),
        "s1p" => Ok(Session {
            sweeps: vec![import_touchstone(path)?],
            ..Default::default()
        }),
        _ => Err(Error::Invalid("use .json, .csv, or .s1p files".into())),
    }
}
pub fn export(path: &Path, sweep: &Sweep, overwrite: bool) -> Result<()> {
    match path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "csv" => export_csv(path, sweep, overwrite),
        "s1p" => export_touchstone(path, sweep, overwrite),
        "json" => save_session(
            path,
            &Session {
                sweeps: vec![sweep.clone()],
                ..Default::default()
            },
            overwrite,
        ),
        _ => Err(Error::Invalid("use .json, .csv, or .s1p output".into())),
    }
}
