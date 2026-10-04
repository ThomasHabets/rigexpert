//! One-port RF and transmission-line calculations. Measurements are never changed.
use crate::{Error, Result, Sample, Sweep, SweepStatus};
use num_complex::Complex64;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;
pub const SPEED_OF_LIGHT: f64 = 299_792_458.0;
fn positive(value: f64, name: &str) -> Result<()> {
    if !value.is_finite() || value <= 0.0 {
        Err(Error::Invalid(format!(
            "{name} must be positive and finite"
        )))
    } else {
        Ok(())
    }
}
pub fn reflection(sample: &Sample, z0: f64) -> Result<Complex64> {
    sample.validate()?;
    positive(z0, "Z0")?;
    let z = sample.impedance();
    Ok((z - z0) / (z + z0))
}
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub gamma: Complex64,
    pub swr: f64,
    pub return_loss_db: f64,
    pub magnitude: f64,
    pub phase_degrees: f64,
    pub inductance_h: Option<f64>,
    pub capacitance_f: Option<f64>,
}
pub fn metrics(sample: &Sample, z0: f64) -> Result<Metrics> {
    let gamma = reflection(sample, z0)?;
    let rho = gamma.norm();
    Ok(Metrics {
        gamma,
        swr: if rho >= 1.0 {
            f64::INFINITY
        } else {
            (1.0 + rho) / (1.0 - rho)
        },
        return_loss_db: if rho == 0.0 {
            f64::INFINITY
        } else {
            -20.0 * rho.log10()
        },
        magnitude: sample.impedance().norm(),
        phase_degrees: sample.impedance().arg().to_degrees(),
        inductance_h: if sample.x > 0.0 {
            Some(sample.x / (2.0 * PI * sample.frequency_hz))
        } else {
            None
        },
        capacitance_f: if sample.x < 0.0 {
            Some(-1.0 / (2.0 * PI * sample.frequency_hz * sample.x))
        } else {
            None
        },
    })
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CableSettings {
    pub impedance_ohm: f64,
    pub length_m: f64,
    pub velocity_factor: f64,
    /// dB/m at the reference frequency, separated into conductor and dielectric losses.
    pub conductor_loss_db_per_m: f64,
    pub dielectric_loss_db_per_m: f64,
    pub reference_hz: f64,
}
impl Default for CableSettings {
    fn default() -> Self {
        Self {
            impedance_ohm: 50.0,
            length_m: 10.0,
            velocity_factor: 0.66,
            conductor_loss_db_per_m: 0.0,
            dielectric_loss_db_per_m: 0.0,
            reference_hz: 100_000_000.0,
        }
    }
}
impl CableSettings {
    pub fn validate(&self) -> Result<()> {
        positive(self.impedance_ohm, "cable impedance")?;
        positive(self.reference_hz, "reference frequency")?;
        validate_vf(self.velocity_factor)?;
        for (v, name) in [
            (self.length_m, "length"),
            (self.conductor_loss_db_per_m, "conductor loss"),
            (self.dielectric_loss_db_per_m, "dielectric loss"),
        ] {
            if !v.is_finite() || v < 0.0 {
                return Err(Error::Invalid(format!(
                    "{name} must be nonnegative and finite"
                )));
            }
        }
        Ok(())
    }
    pub fn attenuation_db_per_m(&self, f: f64) -> f64 {
        let ratio = f / self.reference_hz;
        self.conductor_loss_db_per_m * ratio.sqrt() + self.dielectric_loss_db_per_m * ratio
    }
}
pub fn validate_vf(vf: f64) -> Result<()> {
    positive(vf, "velocity factor")?;
    if vf > 1.0 {
        return Err(Error::Invalid("velocity factor cannot exceed 1".into()));
    }
    Ok(())
}
/// Add a cable in front of a load, or remove it to estimate the far-end load.
pub fn transform(sample: &Sample, cable: &CableSettings, remove: bool) -> Result<Sample> {
    sample.validate()?;
    cable.validate()?;
    let direction = if remove { -1.0 } else { 1.0 };
    let alpha = cable.attenuation_db_per_m(sample.frequency_hz) / 8.685_889_638_065_037;
    let beta = 2.0 * PI * sample.frequency_hz / (SPEED_OF_LIGHT * cable.velocity_factor);
    let t = (Complex64::new(alpha, beta) * cable.length_m * direction).tanh();
    let z0 = cable.impedance_ohm;
    let z = z0 * (sample.impedance() + z0 * t) / (z0 + sample.impedance() * t);
    let result = Sample {
        frequency_hz: sample.frequency_hz,
        r: z.re,
        x: z.im,
    };
    result.validate().map_err(|_| {
        Error::Invalid("cable transform is singular or gives negative resistance".into())
    })?;
    Ok(result)
}
pub fn transform_sweep(sweep: &Sweep, cable: &CableSettings, remove: bool) -> Result<Sweep> {
    sweep.validate()?;
    let mut result = sweep.clone();
    result.name = format!(
        "{} [{} cable]",
        sweep.name,
        if remove { "remove" } else { "add" }
    );
    result.data = sweep
        .data
        .iter()
        .map(|s| transform(s, cable, remove))
        .collect::<Result<_>>()?;
    Ok(result)
}
/// One-way loss estimate for an ideal open/short termination and matched cable.
pub fn cable_loss(sample: &Sample, z0: f64) -> Result<f64> {
    let rho = reflection(sample, z0)?.norm();
    if rho <= 0.0 || rho > 1.0 + 1e-9 {
        return Err(Error::Invalid(
            "loss estimate requires 0 < |reflection| <= 1".into(),
        ));
    }
    Ok(-10.0 * rho.min(1.0).log10())
}
/// Characteristic impedance from corresponding ideal open and short measurements.
pub fn characteristic_impedance(open: &Sweep, short: &Sweep) -> Result<Vec<Complex64>> {
    open.validate()?;
    short.validate()?;
    if open.status != SweepStatus::Complete
        || short.status != SweepStatus::Complete
        || open.data.len() != short.data.len()
    {
        return Err(Error::Invalid(
            "open/short sweeps must be complete and have matching grids".into(),
        ));
    }
    open.data
        .iter()
        .zip(&short.data)
        .map(|(o, s)| {
            if (o.frequency_hz - s.frequency_hz).abs() > 1.0 {
                return Err(Error::Invalid("open/short frequency grids differ".into()));
            }
            let mut z = (o.impedance() * s.impedance()).sqrt();
            if z.re < 0.0 {
                z = -z;
            }
            if !z.re.is_finite() || !z.im.is_finite() {
                return Err(Error::Invalid("singular open/short impedance".into()));
            }
            Ok(z)
        })
        .collect()
}
/// Shortest nonnegative lossless stub length for a target series reactance.
pub fn stub_length(frequency_hz: f64, reactance: f64, z0: f64, vf: f64, open: bool) -> Result<f64> {
    positive(frequency_hz, "frequency")?;
    positive(z0, "Z0")?;
    validate_vf(vf)?;
    if !reactance.is_finite() {
        return Err(Error::Invalid("reactance must be finite".into()));
    }
    let mut phase = if open {
        (-z0).atan2(reactance)
    } else {
        (reactance / z0).atan()
    };
    if phase < 0.0 {
        phase += PI;
    }
    Ok(phase * SPEED_OF_LIGHT * vf / (2.0 * PI * frequency_hz))
}
/// Linear interpolation of measured X=0 crossings, including exact zeros.
pub fn resonances(sweep: &Sweep) -> Vec<f64> {
    let mut result: Vec<f64> = Vec::new();
    for pair in sweep.data.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let f = if a.x == 0.0 {
            Some(a.frequency_hz)
        } else if a.x.signum() != b.x.signum() {
            Some(a.frequency_hz + (b.frequency_hz - a.frequency_hz) * (-a.x) / (b.x - a.x))
        } else {
            None
        };
        if let Some(f) = f
            && result.last().is_none_or(|last| (*last - f).abs() > 1e-6)
        {
            result.push(f);
        }
    }
    if let Some(s) = sweep.data.last()
        && s.x == 0.0
        && result
            .last()
            .is_none_or(|f| (*f - s.frequency_hz).abs() > 1e-6)
    {
        result.push(s.frequency_hz);
    }
    result
}
#[derive(Clone, Debug)]
pub struct TdrPoint {
    pub distance_m: f64,
    pub impulse: f64,
    pub step: f64,
    pub impedance_ohm: Option<f64>,
}
#[derive(Clone, Debug)]
pub struct Tdr {
    pub points: Vec<TdrPoint>,
    pub resolution_m: f64,
    pub range_m: f64,
    pub velocity_factor: f64,
}
impl Tdr {
    /// Strongest reflection after a minimum distance; the user may choose another cursor.
    pub fn peak(&self, min_distance: f64) -> Option<&TdrPoint> {
        self.points
            .iter()
            .filter(|p| p.distance_m >= min_distance)
            .max_by(|a, b| a.impulse.abs().total_cmp(&b.impulse.abs()))
    }
}
pub fn length_from_delay(delay_seconds: f64, vf: f64) -> Result<f64> {
    positive(delay_seconds, "round-trip delay")?;
    validate_vf(vf)?;
    Ok(SPEED_OF_LIGHT * vf * delay_seconds / 2.0)
}
pub fn velocity_factor(length_m: f64, delay_seconds: f64) -> Result<f64> {
    positive(length_m, "known length")?;
    positive(delay_seconds, "round-trip delay")?;
    let vf = 2.0 * length_m / (SPEED_OF_LIGHT * delay_seconds);
    validate_vf(vf)?;
    Ok(vf)
}

/// Low-pass, windowed TDR. The missing DC value is estimated from the lowest
/// measured reflection. A low-start, uniform, complete sweep is required.
pub fn tdr(sweep: &Sweep, vf: f64) -> Result<Tdr> {
    validate_vf(vf)?;
    sweep.validate()?;
    if sweep.status != SweepStatus::Complete || sweep.data.len() < 16 {
        return Err(Error::Invalid(
            "TDR requires a complete sweep with at least 16 samples".into(),
        ));
    }
    let data = &sweep.data;
    let df = data[1].frequency_hz - data[0].frequency_hz;
    positive(df, "frequency spacing")?;
    if data[0].frequency_hz > df * 1.01 {
        return Err(Error::Invalid("TDR requires a broadband sweep starting at or below one frequency step; acquire 100 kHz to 650 MHz".into()));
    }
    if data
        .windows(2)
        .any(|p| ((p[1].frequency_hz - p[0].frequency_hz) - df).abs() > df * 1e-5 + 1.0)
    {
        return Err(Error::Invalid(
            "TDR requires uniformly spaced frequencies".into(),
        ));
    }
    let gamma: Vec<_> = data
        .iter()
        .map(|s| reflection(s, sweep.settings.z0))
        .collect::<Result<_>>()?;
    let bins = (data.last().unwrap().frequency_hz / df).floor() as usize;
    let n = (2 * (bins + 1)).next_power_of_two() * 8;
    if n > 1_048_576 {
        return Err(Error::Invalid("TDR transform too large".into()));
    }
    let mut spectrum = vec![Complex64::new(0.0, 0.0); n];
    // DC is real. Preserve the sign of the low-frequency reflection.
    spectrum[0] = Complex64::new(gamma[0].norm().copysign(gamma[0].re), 0.0);
    for k in 1..=bins {
        let f = k as f64 * df;
        let index = ((f - data[0].frequency_hz) / df).max(0.0);
        let lo = (index.floor() as usize).min(gamma.len() - 1);
        let hi = (lo + 1).min(gamma.len() - 1);
        let weight = (index - lo as f64).clamp(0.0, 1.0);
        let g = gamma[lo] * (1.0 - weight) + gamma[hi] * weight;
        let window = 0.54 + 0.46 * (PI * k as f64 / bins as f64).cos();
        spectrum[k] = g * window;
        spectrum[n - k] = spectrum[k].conj();
    }
    rustfft::FftPlanner::<f64>::new()
        .plan_fft_inverse(n)
        .process(&mut spectrum);
    for s in &mut spectrum {
        *s /= n as f64;
    }
    let distance_step = SPEED_OF_LIGHT * vf / (2.0 * n as f64 * df);
    let mut step: f64 = spectrum[n / 2..].iter().map(|v| v.re).sum();
    let points = spectrum[..n / 2]
        .iter()
        .enumerate()
        .map(|(i, s)| {
            step += s.re;
            let z = if step.abs() < 1.0 {
                Some(sweep.settings.z0 * (1.0 + step) / (1.0 - step))
            } else {
                None
            };
            TdrPoint {
                distance_m: i as f64 * distance_step,
                impulse: s.re,
                step,
                impedance_ohm: z,
            }
        })
        .collect();
    Ok(Tdr {
        points,
        resolution_m: SPEED_OF_LIGHT * vf / (2.0 * data.last().unwrap().frequency_hz),
        range_m: distance_step * (n / 2) as f64,
        velocity_factor: vf,
    })
}
