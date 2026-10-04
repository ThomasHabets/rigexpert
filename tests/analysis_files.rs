use num_complex::Complex64;
use rigexpert::{
    Sample, Sweep, SweepSettings, SweepStatus,
    analysis::{self, CableSettings},
    files::{self, Session},
};
fn sample(r: f64, x: f64) -> Sample {
    Sample {
        frequency_hz: 100_000_000.0,
        r,
        x,
    }
}
fn sweep_from_gamma(gamma: impl Fn(f64) -> Complex64) -> Sweep {
    let settings = SweepSettings {
        start_hz: 100_000,
        stop_hz: 650_000_000,
        samples: 501,
        z0: 50.0,
    };
    let mut s = Sweep::new("synthetic", settings);
    s.status = SweepStatus::Complete;
    s.data = (0..settings.samples)
        .map(|i| {
            let f = settings.frequency(i);
            let g = gamma(f);
            let z = 50.0 * (1.0 + g) / (1.0 - g);
            Sample {
                frequency_hz: f,
                r: z.re,
                x: z.im,
            }
        })
        .collect();
    s
}
#[test]
#[allow(
    clippy::float_cmp,
    reason = "These fixtures require exact equality for representable wire values; stable and nightly differ on linting assertions"
)]
fn loads_and_singular_metrics() {
    let m = analysis::metrics(&sample(50.0, 0.0), 50.0).unwrap();
    assert_eq!(m.swr, 1.0);
    assert_eq!(m.gamma, Complex64::new(0.0, 0.0));
    assert!(m.return_loss_db.is_infinite());
    assert!((analysis::metrics(&sample(100.0, 0.0), 50.0).unwrap().swr - 2.0).abs() < 1e-12);
    assert!(
        analysis::metrics(&sample(0.0, 0.0), 50.0)
            .unwrap()
            .swr
            .is_infinite()
    );
    let m = analysis::metrics(&sample(50.0, 10.0), 50.0).unwrap();
    assert!(m.inductance_h.unwrap() > 0.0);
    assert!(m.capacitance_f.is_none());
    assert!(analysis::metrics(&sample(50.0, 0.0), 0.0).is_err());
}
#[test]
fn cable_transform_round_trip_and_stub() {
    let c = CableSettings {
        conductor_loss_db_per_m: 0.01,
        dielectric_loss_db_per_m: 0.005,
        ..Default::default()
    };
    let load = sample(75.0, -20.0);
    let input = analysis::transform(&load, &c, false).unwrap();
    let recovered = analysis::transform(&input, &c, true).unwrap();
    assert!((recovered.r - load.r).abs() < 1e-9);
    assert!((recovered.x - load.x).abs() < 1e-9);
    let f = 100e6;
    let vf = 0.66;
    let quarter = analysis::SPEED_OF_LIGHT * vf / (4.0 * f);
    assert!((analysis::stub_length(f, 0.0, 50.0, vf, true).unwrap() - quarter).abs() < 1e-10);
    let l = analysis::stub_length(f, 50.0, 50.0, vf, false).unwrap();
    assert!((l - quarter / 2.0).abs() < 1e-10);
    assert!(analysis::stub_length(f, 50.0, 50.0, 1.1, false).is_err());
    let gamma = Complex64::new(0.5, 0.0);
    let z = 50.0 * (1.0 + gamma) / (1.0 - gamma);
    assert!(
        (analysis::cable_loss(&sample(z.re, z.im), 50.0).unwrap() - 3.010_299_956_639_812).abs()
            < 1e-10
    );
}
#[test]
fn paired_open_short_and_resonance() {
    let phase = |f: f64| {
        Complex64::from_polar(
            0.8,
            -4.0 * std::f64::consts::PI * f * 10.0 / (analysis::SPEED_OF_LIGHT * 0.66),
        )
    };
    let o = sweep_from_gamma(phase);
    let mut s = sweep_from_gamma(|f| -phase(f));
    for z in analysis::characteristic_impedance(&o, &s).unwrap() {
        assert!((z.re - 50.0).abs() < 1e-10);
        assert!(z.im.abs() < 1e-10);
    }
    s.data[20].frequency_hz += 5.0;
    assert!(analysis::characteristic_impedance(&o, &s).is_err());
    let mut sweep = Sweep::new(
        "resonance",
        SweepSettings {
            start_hz: 100_000_000,
            stop_hz: 102_000_000,
            samples: 3,
            z0: 50.0,
        },
    );
    sweep.data = vec![
        sample(50.0, -10.0),
        Sample {
            frequency_hz: 101_000_000.0,
            r: 50.0,
            x: 0.0,
        },
        Sample {
            frequency_hz: 102_000_000.0,
            r: 50.0,
            x: 10.0,
        },
    ];
    assert_eq!(analysis::resonances(&sweep), vec![101_000_000.0]);
}
#[test]
fn tdr_locates_known_cable_and_rejects_unsuitable_sweeps() {
    let vf = 0.66;
    let length = 10.0;
    let sweep = sweep_from_gamma(|f| {
        Complex64::from_polar(
            0.8,
            -4.0 * std::f64::consts::PI * f * length / (analysis::SPEED_OF_LIGHT * vf),
        )
    });
    let t = analysis::tdr(&sweep, vf).unwrap();
    let peak = t.peak(1.0).unwrap();
    assert!((peak.distance_m - length).abs() < t.resolution_m);
    assert!(peak.impulse > 0.0);
    let delay = 2.0 * length / (analysis::SPEED_OF_LIGHT * vf);
    assert!((analysis::velocity_factor(length, delay).unwrap() - vf).abs() < 1e-12);
    assert!((analysis::length_from_delay(delay, vf).unwrap() - length).abs() < 1e-12);
    let matched = sweep_from_gamma(|_| Complex64::new(0.0, 0.0));
    let t = analysis::tdr(&matched, vf).unwrap();
    assert!(t.points.iter().all(|p| p.impulse.abs() < 1e-12));
    assert!(
        t.points
            .iter()
            .all(|p| (p.impedance_ohm.unwrap() - 50.0).abs() < 1e-12)
    );
    let mut partial = sweep.clone();
    partial.status = SweepStatus::Partial("missing".into());
    assert!(analysis::tdr(&partial, vf).is_err());
    let mut bad = sweep.clone();
    bad.data[20].frequency_hz += 1000.0;
    assert!(analysis::tdr(&bad, vf).is_err());
    let mut narrow = sweep;
    narrow.settings.start_hz += 100_000_000;
    narrow.settings.stop_hz += 100_000_000;
    narrow.data.iter_mut().for_each(|p| p.frequency_hz += 100e6);
    assert!(analysis::tdr(&narrow, vf).is_err());
}
#[test]
fn session_csv_touchstone_round_trips_and_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let sweep = sweep_from_gamma(|f| Complex64::from_polar(0.2, f / 1e9));
    let session = Session {
        sweeps: vec![sweep.clone()],
        ..Default::default()
    };
    let json = temp.path().join("session.json");
    files::save_session(&json, &session, false).unwrap();
    assert!(files::save_session(&json, &session, false).is_err());
    assert_eq!(
        files::load_session(&json).unwrap().sweeps[0].data,
        sweep.data
    );
    files::save_session(&json, &session, true).unwrap();
    for extension in ["csv", "s1p"] {
        let path = temp.path().join(format!("sweep.{extension}"));
        files::export(&path, &sweep, false).unwrap();
        let loaded = files::load(&path, 75.0).unwrap();
        let restored = &loaded.sweeps[0];
        assert_eq!(restored.settings, sweep.settings);
        assert_eq!(restored.name, sweep.name);
        assert_eq!(restored.status, sweep.status);
        for (a, b) in restored.data.iter().zip(&sweep.data) {
            assert!((a.r - b.r).abs() < 1e-9);
            assert!((a.x - b.x).abs() < 1e-9);
            assert!((a.frequency_hz - b.frequency_hz).abs() < 1e-6);
        }
    }
    let mut partial = sweep.clone();
    partial.data.truncate(10);
    partial.status = SweepStatus::Partial("cancelled".into());
    let path = temp.path().join("partial.csv");
    files::export_csv(&path, &partial, false).unwrap();
    assert_eq!(
        files::import_csv(&path, 50.0).unwrap().status,
        partial.status
    );
}
#[test]
#[allow(
    clippy::float_cmp,
    reason = "These fixtures require exact equality for representable wire values; stable and nightly differ on linting assertions"
)]
fn external_touchstone_units_formats_and_invalid_files() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("external.s1p");
    for (options, data) in [
        ("# MHz S MA R 50", "100 0.5 0\n101 0.5 0"),
        (
            "# kHz S DB R 50",
            "100000 -6.020599913279624 0\n101000 -6.020599913279624 0",
        ),
        ("# Hz S RI R 50", "100000000 0.5 0\n101000000 0.5 0"),
    ] {
        std::fs::write(&path, format!("{options}\n{data}\n")).unwrap();
        let s = files::import_touchstone(&path).unwrap();
        assert_eq!(s.data[0].frequency_hz, 100e6);
        assert!((s.data[0].r - 150.0).abs() < 1e-10);
    }
    for text in [
        "# Hz S RI R 50\n100000000 NaN 0\n101000000 0.5 0",
        "[Version] 2.0",
        "# Hz Z RI R 50\n100000000 50 0",
        "# Hz S RI R 50\n100000000 1 0\n101000000 1 0",
    ] {
        std::fs::write(&path, text).unwrap();
        assert!(files::import_touchstone(&path).is_err());
    }
    let path = temp.path().join("future.json");
    std::fs::write(&path,"{\"version\":2,\"sweeps\":[],\"cable\":{\"impedance_ohm\":50,\"length_m\":10,\"velocity_factor\":0.66,\"conductor_loss_db_per_m\":0,\"dielectric_loss_db_per_m\":0,\"reference_hz\":100000000}}").unwrap();
    assert!(files::load_session(&path).is_err());
}
