use async_trait::async_trait;
use rigexpert::{
    Analyzer, DeviceInfo, Error, Progress, Result, Sample, SweepSettings, SweepStatus, Transport,
    protocol::{self, Packet},
    transport::DemoTransport,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
#[test]
fn crc_and_command_wire_layout() {
    assert_eq!(protocol::crc8(b"123456789"), 0xf4);
    let p = protocol::measure(&SweepSettings::default(), None).unwrap();
    assert_eq!(
        &p[..13],
        &[
            0x7f, 0x68, 0x36, 0x02, 0x00, 0xd0, 0x07, 0, 0, 0xc8, 0, 0, 0
        ]
    );
    assert!(protocol::check(&p).is_ok());
    assert!(protocol::check(&p[..19]).is_err());
    let mut bad = p;
    bad[5] ^= 1;
    assert!(protocol::check(&bad).is_err());
    let saved = protocol::measure(&SweepSettings::default(), Some(42)).unwrap();
    assert_eq!(saved[0], 0x9f);
    assert_eq!(saved[1], 42);
    assert_eq!(&saved[2..14], &p[1..13]);
}
#[test]
#[allow(
    clippy::float_cmp,
    reason = "These fixtures require exact equality for representable wire values; stable and nightly differ on linting assertions"
)]
fn both_formats_and_padding() {
    let settings = SweepSettings {
        samples: 3,
        ..Default::default()
    };
    let mut p = protocol::command(protocol::FRX);
    p[1..9].copy_from_slice(&145_000_000u64.to_le_bytes());
    p[9..13].copy_from_slice(&50f32.to_le_bytes());
    p[13..17].copy_from_slice(&(-12.5f32).to_le_bytes());
    let points = protocol::samples(&protocol::seal(p), false, &settings).unwrap();
    assert_eq!(
        points,
        vec![(
            1,
            Sample {
                frequency_hz: 145_000_000.0,
                r: 50.0,
                x: -12.5
            }
        )]
    );
    p = protocol::command(protocol::FRX);
    // 0x2000 sets exponent 1, and the final pair is padding beyond the grid.
    for i in 0..4 {
        p[3 + 4 * i..5 + 4 * i].copy_from_slice(&(0x2000u16 + 400).to_le_bytes());
        p[5 + 4 * i..7 + 4 * i].copy_from_slice(&(0x8000u16 + 100).to_le_bytes());
    }
    let points = protocol::samples(&p, true, &settings).unwrap();
    assert_eq!(points.len(), 3);
    assert!((points[0].1.r - 49.2).abs() < 1e-10);
    assert_eq!(points[0].1.x, -123.0);
    p[1] = 1;
    assert_eq!(protocol::samples(&p, true, &settings).unwrap()[0].0, 0);
    p[1..3].copy_from_slice(&(-1i16).to_le_bytes());
    assert!(protocol::samples(&p, true, &settings).is_err());
}
#[test]
fn capabilities_and_memory_order() {
    let mut info = DeviceInfo::default();
    let mut p = protocol::command(protocol::INFO);
    p[1] = 2;
    p[2..6].copy_from_slice(&100u32.to_le_bytes());
    p[6..10].copy_from_slice(&650_000u32.to_le_bytes());
    p[10] = 3;
    p[11..13].copy_from_slice(&500u16.to_le_bytes());
    protocol::apply_info(&p, &mut info).unwrap();
    assert_eq!(info.min_hz, 100_000);
    assert_eq!(info.max_hz, 650_000_000);
    assert_eq!(info.max_intervals, 500);
    let mut assembler = protocol::RecordAssembler::default();
    p = protocol::command(protocol::LIST);
    p[1] = 2;
    assert!(assembler.push(&p, 50.0).is_err());
}
#[tokio::test]
#[allow(
    clippy::float_cmp,
    reason = "These fixtures require exact equality for representable wire values; stable and nightly differ on linting assertions"
)]
async fn demo_round_trip_and_memory() {
    let mut a = Analyzer::demo().await.unwrap();
    assert_eq!(a.info().serial, "DEMO");
    assert_eq!(a.info().firmware, "1.0.0");
    let cancel = CancellationToken::new();
    let mut count = 0;
    let sweep = a
        .sweep(
            SweepSettings {
                samples: 11,
                ..Default::default()
            },
            &cancel,
            |p| {
                if matches!(p, Progress::Sample { .. }) {
                    count += 1;
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(sweep.status, SweepStatus::Complete);
    assert_eq!(count, 11);
    assert_eq!(sweep.data[0].frequency_hz, 144_000_000.0);
    assert_eq!(sweep.data[10].frequency_hz, 146_000_000.0);
    let records = a.records(50.0, &cancel).await.unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[1].name, "Cable open");
    let sweep = a.download(&records[1], &cancel, |_| {}).await.unwrap();
    assert_eq!(sweep.data.len(), 501);
    assert_eq!(sweep.status, SweepStatus::Complete);
    a.disconnect().await.unwrap();
}
#[tokio::test]
async fn zero_span_has_two_distinct_samples() {
    let mut a = Analyzer::demo().await.unwrap();
    let s = SweepSettings {
        start_hz: 145_500_000,
        stop_hz: 145_500_000,
        samples: 2,
        z0: 50.0,
    };
    let result = a.sweep(s, &CancellationToken::new(), |_| {}).await.unwrap();
    assert_eq!(result.status, SweepStatus::Complete);
    assert_eq!(result.data.len(), 2);
}
#[derive(Clone, Copy)]
enum Fault {
    CorruptFirst,
    DuplicateFirst,
    DropLast,
    Disconnect,
    Packed,
    NoLegacyCounter,
}
struct FaultTransport {
    demo: DemoTransport,
    fault: Fault,
    active: bool,
    seen: usize,
    repeat: Option<Vec<u8>>,
    packets: VecDeque<Packet>,
    sent: Arc<Mutex<Vec<u8>>>,
}
#[async_trait]
impl Transport for FaultTransport {
    fn packed(&self) -> bool {
        matches!(self.fault, Fault::Packed)
    }
    fn address(&self) -> String {
        "test".into()
    }
    async fn send(&mut self, p: &Packet) -> Result<()> {
        self.sent.lock().unwrap().push(p[0]);
        if p[0] == protocol::FRX {
            self.active = true;
            self.seen = 0;
            if matches!(self.fault, Fault::Packed) {
                let mut packet = protocol::command(protocol::FRX);
                for i in 0..3 {
                    packet[3 + 4 * i..5 + 4 * i].copy_from_slice(&(0x2000u16 + 400).to_le_bytes());
                }
                self.packets.push_back(protocol::seal(packet));
                return Ok(());
            }
        }
        if p[0] == protocol::BREAK {
            self.active = false;
            self.repeat = None;
            self.packets.clear();
        }
        self.demo.send(p).await
    }
    async fn receive(&mut self) -> Result<Vec<u8>> {
        if let Some(p) = self.packets.pop_front() {
            return Ok(p.to_vec());
        }
        if let Some(p) = self.repeat.take() {
            return Ok(p);
        }
        let mut p = self.demo.receive().await?;
        if self.active && p[0] == protocol::FRX {
            self.seen += 1;
            match self.fault {
                Fault::CorruptFirst if self.seen == 1 => p[19] ^= 1,
                Fault::DuplicateFirst if self.seen == 1 => self.repeat = Some(p.clone()),
                Fault::DropLast if self.seen == 3 => return std::future::pending().await,
                Fault::Disconnect => return Err(Error::Disconnected),
                Fault::NoLegacyCounter => {
                    p[17..19].copy_from_slice(&65535u16.to_le_bytes());
                    let packet = protocol::seal(p.as_slice().try_into().unwrap());
                    p = packet.to_vec();
                }
                _ => {}
            }
        }
        Ok(p)
    }
    async fn disconnect(&mut self) -> Result<()> {
        self.demo.disconnect().await
    }
}
async fn faulty(fault: Fault) -> (Analyzer, Arc<Mutex<Vec<u8>>>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let transport = FaultTransport {
        demo: DemoTransport::new(),
        fault,
        active: false,
        seen: 0,
        repeat: None,
        packets: VecDeque::new(),
        sent: sent.clone(),
    };
    let mut a = Analyzer::with_transport(Box::new(transport)).await.unwrap();
    a.idle_timeout = Duration::from_millis(30);
    (a, sent)
}
#[tokio::test]
async fn faults_preserve_partial_and_stop() {
    for fault in [Fault::CorruptFirst, Fault::DropLast, Fault::Disconnect] {
        let (mut a, sent) = faulty(fault).await;
        let s = a
            .sweep(
                SweepSettings {
                    samples: 3,
                    ..Default::default()
                },
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert!(matches!(s.status, SweepStatus::Partial(_)));
        assert!(s.data.len() < 3);
        assert_eq!(sent.lock().unwrap().last(), Some(&protocol::BREAK));
    }
}
#[tokio::test]
async fn duplicate_and_packed_complete() {
    for fault in [Fault::DuplicateFirst, Fault::Packed] {
        let (mut a, _) = faulty(fault).await;
        let s = a
            .sweep(
                SweepSettings {
                    samples: 3,
                    ..Default::default()
                },
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(s.status, SweepStatus::Complete);
        assert_eq!(s.data.len(), 3);
    }
}
#[tokio::test]
async fn cancellation_preserves_progress_and_next_operation_works() {
    let mut a = Analyzer::demo().await.unwrap();
    let token = CancellationToken::new();
    let stop = token.clone();
    let settings = SweepSettings {
        samples: 21,
        ..Default::default()
    };
    let s = a
        .sweep(settings, &token, move |p| {
            if let Progress::Sample { index: 2, .. } = p {
                stop.cancel();
            }
        })
        .await
        .unwrap();
    assert_eq!(s.data.len(), 3);
    assert_eq!(s.status, SweepStatus::Partial("cancelled".into()));
    let s = a
        .sweep(settings, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(s.status, SweepStatus::Complete);
}
#[test]
fn invalid_settings() {
    let info = DeviceInfo::default();
    for settings in [
        SweepSettings {
            samples: 1,
            ..Default::default()
        },
        SweepSettings {
            samples: 502,
            ..Default::default()
        },
        SweepSettings {
            start_hz: 10,
            ..Default::default()
        },
        SweepSettings {
            start_hz: 144_001_000,
            ..Default::default()
        },
        SweepSettings {
            z0: f64::NAN,
            ..Default::default()
        },
    ] {
        assert!(settings.validate(&info).is_err());
    }
}

#[tokio::test]
async fn zero_span_does_not_depend_on_undocumented_legacy_counter() {
    let (mut a, _) = faulty(Fault::NoLegacyCounter).await;
    let settings = SweepSettings {
        start_hz: 145_500_000,
        stop_hz: 145_500_000,
        samples: 2,
        z0: 50.0,
    };
    let sweep = a
        .sweep(settings, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(sweep.status, SweepStatus::Complete);
    assert_eq!(sweep.data.len(), 2);
}
