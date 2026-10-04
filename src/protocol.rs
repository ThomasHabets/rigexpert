//! RigExpert binary GATT protocol. Independently implemented from the wire
//! layouts in https://github.com/rigexpert/AntScope2/tree/master/analyzer.
use crate::{DeviceInfo, Error, Record, Result, Sample, SweepSettings};
pub const SERVICE: &str = "d973f2e0-b19e-11e2-9e96-0800200c9a66";
pub const READ: &str = "706e4f15-3ee6-41c6-ba10-ca8abdcf3043";
pub const WRITE: &str = "6f8963a8-21e9-4055-86b8-2f911d736cff";
pub const RETURN: &str = "07395738-5d8a-11ec-bf63-0242ac130002";
pub const FRX: u8 = 0x7f;
pub const LIST: u8 = 0x8f;
pub const DATA: u8 = 0x9f;
pub const PING: u8 = 0x5a;
pub const BREAK: u8 = 0x69;
pub const INFO: u8 = 0x9b;
pub type Packet = [u8; 20];

/// CRC-8: polynomial 0x07, initial value 0, no reflection or final xor.
pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 7
            } else {
                crc << 1
            };
        }
    }
    crc
}
pub fn seal(mut packet: Packet) -> Packet {
    packet[19] = crc8(&packet[..19]);
    packet
}
pub fn command(cmd: u8) -> Packet {
    let mut p = [0; 20];
    p[0] = cmd;
    seal(p)
}
pub fn check(bytes: &[u8]) -> Result<Packet> {
    let p: Packet = bytes
        .try_into()
        .map_err(|_| Error::Protocol(format!("expected 20 bytes, got {}", bytes.len())))?;
    if crc8(&p[..19]) != p[19] {
        return Err(Error::Protocol("CRC mismatch".into()));
    }
    Ok(p)
}
pub fn measure(settings: &SweepSettings, slot: Option<u8>) -> Result<Packet> {
    if settings.samples < 2 {
        return Err(Error::Invalid("at least two samples are required".into()));
    }
    let start = settings.start_hz / 1000;
    let span = settings
        .stop_hz
        .checked_sub(settings.start_hz)
        .ok_or_else(|| Error::Invalid("reversed endpoints".into()))?
        / 1000;
    let center: u32 = (start + span / 2)
        .try_into()
        .map_err(|_| Error::Invalid("frequency overflow".into()))?;
    let span: u32 = span
        .try_into()
        .map_err(|_| Error::Invalid("span overflow".into()))?;
    let intervals: u32 = (settings.samples - 1)
        .try_into()
        .map_err(|_| Error::Invalid("sample count overflow".into()))?;
    let mut p = command(if slot.is_some() { DATA } else { FRX });
    let offset = if let Some(slot) = slot {
        p[1] = slot;
        2
    } else {
        1
    };
    p[offset..offset + 4].copy_from_slice(&center.to_le_bytes());
    p[offset + 4..offset + 8].copy_from_slice(&span.to_le_bytes());
    p[offset + 8..offset + 12].copy_from_slice(&intervals.to_le_bytes());
    Ok(seal(p))
}
fn u16_at(p: &Packet, n: usize) -> u16 {
    u16::from_le_bytes([p[n], p[n + 1]])
}
fn u32_at(p: &Packet, n: usize) -> u32 {
    u32::from_le_bytes(p[n..n + 4].try_into().unwrap())
}
fn u64_at(p: &Packet, n: usize) -> u64 {
    u64::from_le_bytes(p[n..n + 8].try_into().unwrap())
}
fn string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.split(|b| *b == 0).next().unwrap_or_default()).into_owned()
}
/// Apply a full-information field. Unknown fields are ignored for compatibility.
pub fn apply_info(p: &Packet, info: &mut DeviceInfo) -> Result<u8> {
    match p[1] {
        0 => info.name = string(&p[2..19]),
        2 => {
            let scale = 10f64.powi(6 - (p[10] as i8) as i32);
            let min = u32_at(p, 2) as f64 * scale;
            let max = u32_at(p, 6) as f64 * scale;
            let intervals = u16_at(p, 11) as usize;
            if !min.is_finite() || min < 1.0 || max < min || max > u64::MAX as f64 || intervals == 0
            {
                return Err(Error::Protocol("invalid device capabilities".into()));
            }
            // This client targets the AA-650; never allow RF outside its rated range.
            info.min_hz = (min as u64).max(100_000);
            info.max_hz = (max as u64).min(650_000_000);
            info.max_intervals = intervals;
        }
        3 => {
            let end = p[2..19]
                .iter()
                .position(|b| *b == 0)
                .ok_or_else(|| Error::Protocol("unterminated serial".into()))?
                + 2;
            if end + 7 > 19 {
                return Err(Error::Protocol("truncated firmware version".into()));
            }
            info.serial = string(&p[2..end]);
            info.firmware = format!(
                "{}.{}.{}",
                u16_at(p, end + 1),
                u16_at(p, end + 3),
                u16_at(p, end + 5)
            );
        }
        _ => {}
    }
    Ok(p[1])
}
/// Packed signed impedance: 13-bit mantissa, 2-bit decimal exponent, sign bit.
pub fn unpack(value: u16) -> f64 {
    let magnitude = (value & 0x1fff) as f64 * 123.0 / 10f64.powi(((value >> 13) & 3) as i32 + 2);
    if value & 0x8000 != 0 {
        -magnitude
    } else {
        magnitude
    }
}
pub fn samples(p: &Packet, packed: bool, settings: &SweepSettings) -> Result<Vec<(usize, Sample)>> {
    let mut out = Vec::new();
    if packed {
        let id = i16::from_le_bytes([p[1], p[2]]);
        if id < 0 {
            return Err(Error::Protocol("negative sample index".into()));
        }
        // Odd block IDs in newer firmware identify the same even-index block.
        let first = (id as usize) & !1;
        for i in 0..4 {
            let index = first + i;
            if index >= settings.samples {
                continue;
            }
            let r = u16_at(p, 3 + 4 * i);
            let x = u16_at(p, 5 + 4 * i);
            // Firmware uses zero pairs as unused entries, including terminal padding.
            if r == 0 && x == 0 {
                continue;
            }
            let s = Sample {
                frequency_hz: settings.frequency(index),
                r: unpack(r),
                x: unpack(x),
            };
            s.validate()?;
            out.push((index, s));
        }
    } else {
        let frequency_hz = u64_at(p, 1) as f64;
        let s = Sample {
            frequency_hz,
            r: f32::from_le_bytes(p[9..13].try_into().unwrap()) as f64,
            x: f32::from_le_bytes(p[13..17].try_into().unwrap()) as f64,
        };
        s.validate()?;
        let span = (settings.stop_hz - settings.start_hz) as f64;
        let index = if span == 0.0 {
            // AntScope ignores the legacy trailing counter. Zero-span samples
            // have no frequency-derived index; the client assigns arrival order.
            0
        } else {
            let idx =
                (frequency_hz - settings.start_hz as f64) / span * (settings.samples - 1) as f64;
            if idx < -0.01 || idx > (settings.samples - 1) as f64 + 0.01 {
                return Err(Error::Protocol("sample outside requested range".into()));
            }
            idx.round() as usize
        };
        if index >= settings.samples || (settings.frequency(index) - frequency_hz).abs() > 2.0 {
            return Err(Error::Protocol("sample outside requested grid".into()));
        }
        out.push((index, s));
    }
    Ok(out)
}
#[derive(Default)]
pub struct RecordAssembler {
    current: Option<Record>,
    stage: u8,
}
impl RecordAssembler {
    /// Returns an assembled record; 0xff signals list completion to the caller.
    pub fn push(&mut self, p: &Packet, z0: f64) -> Result<Option<Record>> {
        match p[1] {
            0 => {
                let center = u64_at(p, 2);
                let span = u64_at(p, 10);
                let start = center
                    .checked_sub(span / 2)
                    .ok_or_else(|| Error::Protocol("invalid record frequency".into()))?;
                let stop = start
                    .checked_add(span)
                    .ok_or_else(|| Error::Protocol("record frequency overflow".into()))?;
                self.current = Some(Record {
                    slot: p[18],
                    name: String::new(),
                    settings: SweepSettings {
                        start_hz: start,
                        stop_hz: stop,
                        samples: 0,
                        z0,
                    },
                });
                self.stage = 1;
            }
            1 if self.stage == 1 => {
                let record = self.current.as_mut().unwrap();
                record.settings.samples = u16_at(p, 2) as usize;
                if record.settings.samples < 2 {
                    return Err(Error::Protocol("invalid record point count".into()));
                }
                record.name = string(&p[4..19]);
                self.stage = 2;
            }
            2 if self.stage == 2 => {
                let mut record = self.current.take().unwrap();
                record.name.push_str(&string(&p[2..19]));
                self.stage = 0;
                return Ok(Some(record));
            }
            0xff if self.stage == 0 => {}
            _ => return Err(Error::Protocol("out-of-order memory record".into())),
        }
        Ok(None)
    }
}
