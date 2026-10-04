use crate::bands::{self, BANDS};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    prelude::*,
    widgets::{
        Axis, Block, Chart, Clear, Dataset, GraphType, List, ListItem, ListState, Paragraph, Tabs,
        Wrap,
        canvas::{Canvas, Line as CanvasLine, Points},
    },
};
use rigexpert::{
    Analyzer, ConnectionOptions, DeviceInfo, Error, Progress, Record, Result, Sample, Sweep,
    SweepSettings, SweepStatus,
    analysis::{self, CableSettings, Tdr},
    files::{self, Session},
};
use std::fmt::Write as _;
use std::{collections::VecDeque, io::IsTerminal, path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type PlotSeries = (String, Vec<(f64, f64)>, Color);

const TABS: [&str; 6] = ["Live", "Sweeps", "Smith", "TDR", "Cable", "Memory"];
const COLORS: [Color; 4] = [Color::Cyan, Color::Yellow, Color::Magenta, Color::Green];
#[derive(Debug)]
enum Work {
    Connect,
    Acquire(SweepSettings, CancellationToken),
    Records(f64, CancellationToken),
    Download(Record, CancellationToken),
    Shutdown,
}
enum Update {
    Connecting,
    Connected(DeviceInfo),
    Progress(Progress),
    Acquired(Sweep),
    Records(Vec<Record>),
    Failed(String),
    Disconnected(String),
    Loaded(Session),
    Saved(String),
    Shutdown,
}
async fn worker(
    options: ConnectionOptions,
    demo: bool,
    mut commands: mpsc::Receiver<Work>,
    events: mpsc::Sender<Update>,
) {
    let mut analyzer: Option<Analyzer> = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let command = tokio::select! {
            command=commands.recv()=>match command { Some(c)=>c,None=>break },
            _=heartbeat.tick(),if analyzer.is_some()=> {
                let a=analyzer.as_mut().unwrap();
                if let Err(e)=a.ping().await {
                    let _=a.disconnect().await; analyzer=None;
                    let _=events.send(Update::Disconnected(format!("Connection lost: {e}. Press r to reconnect."))).await;
                }
                continue;
            }
        };
        if matches!(command, Work::Shutdown) {
            break;
        }
        if matches!(command, Work::Connect) {
            if let Some(mut a) = analyzer.take() {
                let _ = a.disconnect().await;
            }
            let _ = events.send(Update::Connecting).await;
            match if demo {
                Analyzer::demo().await
            } else {
                Analyzer::connect(&options).await
            } {
                Ok(a) => {
                    let _ = events.send(Update::Connected(a.info().clone())).await;
                    analyzer = Some(a);
                }
                Err(e) => {
                    let _ = events
                        .send(Update::Failed(format!(
                            "{e}. Press r to retry; --demo works without Bluetooth."
                        )))
                        .await;
                }
            }
            continue;
        }
        let Some(a) = analyzer.as_mut() else {
            let _ = events
                .send(Update::Failed("Disconnected; press r to connect".into()))
                .await;
            continue;
        };
        let progress_events = events.clone();
        let progress = move |p| {
            let _ = progress_events.try_send(Update::Progress(p));
        };
        let outcome = match command {
            Work::Acquire(settings, cancel) => a
                .sweep(settings, &cancel, progress)
                .await
                .map(Update::Acquired),
            Work::Download(record, cancel) => a
                .download(&record, &cancel, progress)
                .await
                .map(Update::Acquired),
            Work::Records(z0, cancel) => a.records(z0, &cancel).await.map(Update::Records),
            _ => unreachable!(),
        };
        match outcome {
            Ok(event) => {
                let _ = events.send(event).await;
            }
            Err(e) => {
                let _ = events.send(Update::Failed(e.to_string())).await;
            }
        }
    }
    if let Some(mut a) = analyzer {
        let _ = a.disconnect().await;
    }
    let _ = events.send(Update::Shutdown).await;
}
#[derive(Clone, Copy)]
enum FileAction {
    Load,
    Save,
    Export,
}
enum Modal {
    Bands {
        selected: usize,
    },
    Settings {
        fields: Vec<(String, String)>,
        selected: usize,
        cable: bool,
    },
    File {
        action: FileAction,
        path: String,
    },
    Overwrite {
        action: FileAction,
        path: PathBuf,
    },
    Rename(String),
    Help,
}
#[expect(
    clippy::struct_excessive_bools,
    reason = "Connection, acquisition, file I/O and display toggles are independent state dimensions"
)]
struct App {
    session: Session,
    info: Option<DeviceInfo>,
    connecting: bool,
    tab: usize,
    selected: usize,
    live_index: Option<usize>,
    overlays: Vec<usize>,
    cursor: usize,
    tdr_cursor: usize,
    settings: SweepSettings,
    band_selected: usize,
    live_hz: u64,
    repeat: bool,
    busy: bool,
    cancel: CancellationToken,
    progress: Vec<Option<Sample>>,
    active_settings: Option<SweepSettings>,
    records: Vec<Record>,
    memory_selected: usize,
    metric: usize,
    zoom: usize,
    feet: bool,
    tdr: Option<std::result::Result<Tdr, String>>,
    open: Option<usize>,
    short: Option<usize>,
    target_x: f64,
    known_length_m: f64,
    logs: VecDeque<String>,
    modal: Option<Modal>,
    file_busy: bool,
}
impl App {
    fn new(session: Session) -> Self {
        let settings = session
            .sweeps
            .last()
            .map(|s| s.settings)
            .unwrap_or_default();
        let selected = session.sweeps.len().saturating_sub(1);
        let mut app = Self {
            session,
            info: None,
            connecting: true,
            tab: 1,
            selected,
            live_index: None,
            overlays: Vec::new(),
            cursor: 0,
            tdr_cursor: 0,
            settings,
            band_selected: BANDS
                .iter()
                .position(|band| {
                    let preset = band.settings(settings);
                    preset.start_hz == settings.start_hz && preset.stop_hz == settings.stop_hz
                })
                .unwrap_or(0),
            live_hz: 145_500_000,
            repeat: false,
            busy: false,
            cancel: CancellationToken::new(),
            progress: Vec::new(),
            active_settings: None,
            records: Vec::new(),
            memory_selected: 0,
            metric: 0,
            zoom: 1,
            feet: false,
            tdr: None,
            open: None,
            short: None,
            target_x: 50.0,
            known_length_m: 10.0,
            logs: VecDeque::new(),
            modal: None,
            file_busy: false,
        };
        app.recompute();
        app
    }
    fn log(&mut self, text: impl Into<String>) {
        self.logs.push_back(text.into());
        if self.logs.len() > 100 {
            self.logs.pop_front();
        }
    }
    fn sweep(&self) -> Option<&Sweep> {
        self.session.sweeps.get(self.selected)
    }
    fn recompute(&mut self) {
        self.tdr = self.sweep().map(|s| {
            analysis::tdr(s, self.session.cable.velocity_factor).map_err(|e| e.to_string())
        });
        if let Some(Ok(tdr)) = &self.tdr {
            self.tdr_cursor = self.tdr_cursor.min(tdr.points.len().saturating_sub(1));
        }
        self.cursor = self
            .cursor
            .min(self.sweep().map_or(0, |s| s.data.len().saturating_sub(1)));
    }
    fn apply_band(&mut self, selected: usize) -> Result<()> {
        let band = BANDS[selected];
        let settings = band.settings(self.settings);
        settings.validate(&self.info.clone().unwrap_or_default())?;
        self.settings = settings;
        self.band_selected = selected;
        self.live_hz = settings.start_hz.midpoint(settings.stop_hz) / 1000 * 1000;
        self.tab = 1;
        self.zoom = 1;
        self.log(format!(
            "{} · Sweep {} · Space starts.",
            band.label(),
            bands::range(settings.start_hz, settings.stop_hz)
        ));
        Ok(())
    }
    fn acquisition_settings(&self) -> SweepSettings {
        if self.tab == 0 {
            SweepSettings {
                start_hz: self.live_hz,
                stop_hz: self.live_hz,
                samples: 2,
                z0: self.settings.z0,
            }
        } else if self.tab == 3 {
            let info = self.info.clone().unwrap_or_default();
            SweepSettings {
                start_hz: 100_000,
                stop_hz: info.max_hz / 2000 * 2000,
                samples: info.max_intervals.min(500) + 1,
                z0: self.settings.z0,
            }
        } else {
            self.settings
        }
    }
    fn start(&mut self, commands: &mpsc::Sender<Work>) {
        if self.busy {
            self.cancel.cancel();
            self.repeat = false;
            self.log("Stopping measurement…");
            return;
        }
        if self.connecting || self.info.is_none() {
            self.log("Wait for a connection, or press r to retry");
            return;
        }
        let settings = self.acquisition_settings();
        self.start_settings(settings, commands);
    }
    fn start_settings(&mut self, settings: SweepSettings, commands: &mpsc::Sender<Work>) {
        if let Err(e) = settings.validate(self.info.as_ref().unwrap()) {
            self.log(e.to_string());
            return;
        }
        self.cancel = CancellationToken::new();
        self.progress = vec![None; settings.samples];
        if commands
            .try_send(Work::Acquire(settings, self.cancel.clone()))
            .is_ok()
        {
            self.busy = true;
            self.active_settings = Some(settings);
            self.log(format!("Acquiring {} samples", settings.samples));
        }
    }
    fn update(&mut self, update: Update, commands: &mpsc::Sender<Work>) {
        match update {
            Update::Connecting => {
                self.connecting = true;
                self.info = None;
                self.log("Connecting…");
            }
            Update::Connected(info) => {
                self.log(format!(
                    "Ready — press Space to start. {} firmware {}",
                    info.name, info.firmware
                ));
                self.info = Some(info);
                self.connecting = false;
            }
            Update::Progress(Progress::Sample { index, sample }) => {
                if let Some(p) = self.progress.get_mut(index) {
                    *p = Some(sample);
                }
            }
            Update::Progress(Progress::Warning(text)) => self.log(text),
            Update::Acquired(mut sweep) => {
                self.busy = false;
                let complete = sweep.status == SweepStatus::Complete;
                let is_live = sweep.settings.start_hz == sweep.settings.stop_hz;
                if sweep.name == "Measurement" {
                    sweep.name = format!(
                        "{} {}",
                        if is_live { "Live" } else { "Sweep" },
                        self.session.sweeps.len() + 1
                    );
                }
                self.log(format!(
                    "{}: {} samples, {:?}",
                    sweep.name,
                    sweep.data.len(),
                    sweep.status
                ));
                // Track the ongoing live reading independently of UI selection.
                // Renaming or importing freezes readings as saved measurements.
                if is_live && let Some(index) = self.live_index {
                    self.session.sweeps[index] = sweep;
                    self.selected = index;
                } else {
                    self.session.sweeps.push(sweep);
                    self.selected = self.session.sweeps.len() - 1;
                    if is_live {
                        self.live_index = Some(self.selected);
                    }
                }
                self.recompute();
                if self.repeat
                    && complete
                    && !self.cancel.is_cancelled()
                    && let Some(settings) = self.active_settings
                {
                    self.start_settings(settings, commands);
                }
            }
            Update::Records(records) => {
                self.busy = false;
                self.records = records;
                self.memory_selected = 0;
                self.log(format!("{} stored records", self.records.len()));
            }
            Update::Failed(text) => {
                self.busy = false;
                self.connecting = false;
                self.repeat = false;
                self.log(text);
            }
            Update::Disconnected(text) => {
                self.info = None;
                self.busy = false;
                self.connecting = false;
                self.repeat = false;
                self.log(text);
            }
            Update::Loaded(session) => {
                self.file_busy = false;
                self.session = session;
                self.selected = self.session.sweeps.len().saturating_sub(1);
                self.overlays.clear();
                self.live_index = None;
                if let Some(sweep) = self.sweep() {
                    self.settings = sweep.settings;
                }
                self.open = None;
                self.short = None;
                self.recompute();
                self.log("Loaded session/file");
            }
            Update::Saved(text) => {
                self.file_busy = false;
                self.log(text);
            }
            Update::Shutdown => {}
        }
    }
    fn settings_modal(&mut self, cable: bool) {
        let fields = if cable {
            let c = self.session.cable;
            [
                ("Cable Z0 (ohm)", c.impedance_ohm),
                ("Length (m)", c.length_m),
                ("Velocity factor", c.velocity_factor),
                ("Conductor loss (dB/m)", c.conductor_loss_db_per_m),
                ("Dielectric loss (dB/m)", c.dielectric_loss_db_per_m),
                ("Loss reference (Hz)", c.reference_hz),
                ("Stub target X (ohm)", self.target_x),
                ("Known cable length (m)", self.known_length_m),
            ]
            .into_iter()
            .map(|(k, v)| (k.into(), v.to_string()))
            .collect()
        } else {
            vec![
                (
                    "Start / live frequency".into(),
                    format!(
                        "{}Hz",
                        if self.tab == 0 {
                            self.live_hz
                        } else {
                            self.settings.start_hz
                        }
                    ),
                ),
                (
                    "Stop frequency".into(),
                    format!("{}Hz", self.settings.stop_hz),
                ),
                ("Samples".into(), self.settings.samples.to_string()),
                ("Reference Z0 (ohm)".into(), self.settings.z0.to_string()),
                ("Repeat (true/false)".into(), self.repeat.to_string()),
            ]
        };
        self.modal = Some(Modal::Settings {
            fields,
            selected: 0,
            cable,
        });
    }
    fn apply_fields(&mut self, fields: &[(String, String)], cable: bool) -> Result<()> {
        let number = |i: usize| -> Result<f64> {
            fields[i]
                .1
                .parse::<f64>()
                .map_err(|_| Error::Invalid(format!("invalid {}", fields[i].0)))
        };
        if cable {
            let settings = CableSettings {
                impedance_ohm: number(0)?,
                length_m: number(1)?,
                velocity_factor: number(2)?,
                conductor_loss_db_per_m: number(3)?,
                dielectric_loss_db_per_m: number(4)?,
                reference_hz: number(5)?,
            };
            settings.validate()?;
            let target_x = number(6)?;
            let known_length = number(7)?;
            if !target_x.is_finite() || !known_length.is_finite() || known_length <= 0.0 {
                return Err(Error::Invalid(
                    "stub reactance must be finite and known length positive".into(),
                ));
            }
            self.session.cable = settings;
            self.target_x = target_x;
            self.known_length_m = known_length;
            self.recompute();
        } else {
            let start = crate::frequency(&fields[0].1).map_err(Error::Invalid)?;
            let stop = crate::frequency(&fields[1].1).map_err(Error::Invalid)?;
            let samples = fields[2]
                .1
                .parse()
                .map_err(|_| Error::Invalid("invalid sample count".into()))?;
            let settings = SweepSettings {
                start_hz: start,
                stop_hz: if self.tab == 0 { start } else { stop },
                samples: if self.tab == 0 { 2 } else { samples },
                z0: number(3)?,
            };
            settings.validate(&self.info.clone().unwrap_or_default())?;
            let repeat = fields[4]
                .1
                .parse()
                .map_err(|_| Error::Invalid("Repeat must be true or false".into()))?;
            if self.tab == 0 {
                self.live_hz = start;
                self.settings.z0 = settings.z0;
            } else {
                self.settings = settings;
            }
            self.repeat = repeat;
        }
        Ok(())
    }
    fn file(
        &mut self,
        action: FileAction,
        path: PathBuf,
        overwrite: bool,
        events: &mpsc::Sender<Update>,
    ) {
        if self.file_busy {
            self.log("A file operation is already running");
            return;
        }
        if !matches!(action, FileAction::Load) && path.exists() && !overwrite {
            self.modal = Some(Modal::Overwrite { action, path });
            return;
        }
        let session = self.session.clone();
        let sweep = self.sweep().cloned();
        let z0 = self.settings.z0;
        let sender = events.clone();
        self.file_busy = true;
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || match action {
                FileAction::Load => files::load(&path, z0).map(Update::Loaded),
                FileAction::Save => files::save_session(&path, &session, overwrite)
                    .map(|()| Update::Saved(format!("Saved {}", path.display()))),
                FileAction::Export => {
                    let sweep =
                        sweep.ok_or_else(|| Error::Invalid("select a sweep to export".into()))?;
                    files::export(&path, &sweep, overwrite)
                        .map(|()| Update::Saved(format!("Exported {}", path.display())))
                }
            })
            .await;
            let update = match result {
                Ok(Ok(u)) => u,
                Ok(Err(e)) => Update::Saved(e.to_string()),
                Err(e) => Update::Saved(format!("File operation failed: {e}")),
            };
            let _ = sender.send(update).await;
        });
    }
    /// Returns true when the user exits.
    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered discovery or UI dispatch stages together for review"
    )]
    fn key(
        &mut self,
        key: KeyEvent,
        commands: &mpsc::Sender<Work>,
        events: &mpsc::Sender<Update>,
    ) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.cancel.cancel();
            return true;
        }
        if let Some(mut modal) = self.modal.take() {
            if key.code == KeyCode::Esc {
                return false;
            }
            let mut keep = true;
            match &mut modal {
                Modal::Bands { selected } => match key.code {
                    KeyCode::Down | KeyCode::Tab => *selected = (*selected + 1) % BANDS.len(),
                    KeyCode::Up | KeyCode::BackTab => {
                        *selected = (*selected + BANDS.len() - 1) % BANDS.len();
                    }
                    KeyCode::Home => *selected = 0,
                    KeyCode::End => *selected = BANDS.len() - 1,
                    KeyCode::PageDown => *selected = (*selected + 10).min(BANDS.len() - 1),
                    KeyCode::PageUp => *selected = selected.saturating_sub(10),
                    KeyCode::Enter => match self.apply_band(*selected) {
                        Ok(()) => keep = false,
                        Err(error) => self.log(error.to_string()),
                    },
                    _ => {}
                },
                Modal::Help => {
                    keep = false;
                }
                Modal::Settings {
                    fields,
                    selected,
                    cable,
                } => match key.code {
                    KeyCode::Tab | KeyCode::Down => *selected = (*selected + 1) % fields.len(),
                    KeyCode::BackTab | KeyCode::Up => {
                        *selected = (*selected + fields.len() - 1) % fields.len();
                    }
                    KeyCode::Enter => match self.apply_fields(fields, *cable) {
                        Ok(()) => keep = false,
                        Err(e) => self.log(e.to_string()),
                    },
                    KeyCode::Backspace => {
                        fields[*selected].1.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        fields[*selected].1.clear();
                    }
                    KeyCode::Char(ch) => fields[*selected].1.push(ch),
                    _ => {}
                },
                Modal::File { action, path } => match key.code {
                    KeyCode::Enter => {
                        let action = *action;
                        let path = PathBuf::from(path.as_str());
                        self.file(action, path, false, events);
                        keep = false;
                    }
                    KeyCode::Backspace => {
                        path.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        path.clear();
                    }
                    KeyCode::Char(c) => path.push(c),
                    _ => {}
                },
                Modal::Overwrite { action, path } => match key.code {
                    KeyCode::Char('y') => {
                        self.file(*action, path.clone(), true, events);
                        keep = false;
                    }
                    KeyCode::Char('n') => keep = false,
                    _ => {}
                },
                Modal::Rename(name) => match key.code {
                    KeyCode::Enter => {
                        if !name.trim().is_empty() {
                            if let Some(s) = self.session.sweeps.get_mut(self.selected) {
                                s.name = name.trim().into();
                                if self.live_index == Some(self.selected) {
                                    self.live_index = None;
                                }
                            }
                            keep = false;
                        }
                    }
                    KeyCode::Backspace => {
                        name.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        name.clear();
                    }
                    KeyCode::Char(c) => name.push(c),
                    _ => {}
                },
            }
            if keep {
                self.modal = Some(modal);
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('B') => {
                if self.busy {
                    self.log("Stop the measurement with Space before changing bands.");
                } else {
                    self.modal = Some(Modal::Bands {
                        selected: self.band_selected,
                    });
                }
            }
            KeyCode::Tab => {
                self.tab = (self.tab + 1) % TABS.len();
            }
            KeyCode::BackTab => {
                self.tab = (self.tab + TABS.len() - 1) % TABS.len();
            }
            KeyCode::Char(' ') => self.start(commands),
            KeyCode::Char('e') if !self.busy => self.settings_modal(self.tab == 4),
            KeyCode::Char('r') if !self.connecting => {
                self.cancel.cancel();
                self.repeat = false;
                self.connecting = true;
                let _ = commands.try_send(Work::Connect);
            }
            KeyCode::Char('p') => {
                self.repeat = !self.repeat;
                self.log(format!("Repeat: {}", self.repeat));
            }
            KeyCode::Char('s') => {
                self.modal = Some(Modal::File {
                    action: FileAction::Save,
                    path: "session.json".into(),
                });
            }
            KeyCode::Char('x') => {
                self.modal = Some(Modal::File {
                    action: FileAction::Export,
                    path: "sweep.csv".into(),
                });
            }
            KeyCode::Char('l') if !self.busy => {
                self.modal = Some(Modal::File {
                    action: FileAction::Load,
                    path: String::new(),
                });
            }
            KeyCode::Char('n') => {
                self.modal = Some(Modal::Rename(
                    self.sweep().map(|s| s.name.clone()).unwrap_or_default(),
                ));
            }
            KeyCode::Char('m') => {
                self.metric = (self.metric + 1) % if self.tab == 3 { 3 } else { 5 }
            }
            KeyCode::Char('+' | '=') => self.zoom = (self.zoom * 2).min(64),
            KeyCode::Char('-') => self.zoom = (self.zoom / 2).max(1),
            KeyCode::Char('u') => self.feet = !self.feet,
            KeyCode::Char('b') => {
                if self.overlays.contains(&self.selected) {
                    self.overlays.retain(|i| *i != self.selected);
                } else if self.overlays.len() < 3 {
                    self.overlays.push(self.selected);
                } else {
                    self.log("Select at most three comparison sweeps plus the current sweep");
                }
            }
            KeyCode::Char('o') => {
                self.open = Some(self.selected);
                self.log("Selected open-terminated sweep");
            }
            KeyCode::Char('k') => {
                self.short = Some(self.selected);
                self.log("Selected short-terminated sweep");
            }
            KeyCode::Char('a' | 'd') if self.tab == 4 => {
                if let Some(s) = self.sweep() {
                    match analysis::transform_sweep(
                        s,
                        &self.session.cable,
                        key.code == KeyCode::Char('d'),
                    ) {
                        Ok(s) => {
                            self.session.sweeps.push(s);
                            self.selected = self.session.sweeps.len() - 1;
                            self.recompute();
                        }
                        Err(e) => self.log(e.to_string()),
                    }
                }
            }
            KeyCode::Char('v') if self.tab == 4 => {
                let delay = self
                    .tdr
                    .as_ref()
                    .and_then(|t| t.as_ref().ok())
                    .and_then(|t| t.points.get(self.tdr_cursor))
                    .map(|p| {
                        2.0 * p.distance_m
                            / (analysis::SPEED_OF_LIGHT * self.session.cable.velocity_factor)
                    });
                match delay
                    .ok_or_else(|| Error::Invalid("select a TDR reflection first".into()))
                    .and_then(|d| analysis::velocity_factor(self.known_length_m, d))
                {
                    Ok(vf) => {
                        self.session.cable.velocity_factor = vf;
                        self.recompute();
                        self.log(format!("Estimated velocity factor {vf:.5}"));
                    }
                    Err(e) => self.log(e.to_string()),
                }
            }
            KeyCode::Char('g') if self.tab == 3 || self.tab == 4 => {
                if let Some(Ok(t)) = &self.tdr
                    && let Some(p) = t.peak(t.resolution_m)
                {
                    self.tdr_cursor = t
                        .points
                        .iter()
                        .position(|q| std::ptr::eq(q, p))
                        .unwrap_or(0);
                }
            }
            KeyCode::Char('f') if self.tab == 5 && !self.busy => {
                self.cancel = CancellationToken::new();
                if commands
                    .try_send(Work::Records(self.settings.z0, self.cancel.clone()))
                    .is_ok()
                {
                    self.busy = true;
                }
            }
            KeyCode::Enter if self.tab == 5 && !self.busy => {
                if let Some(record) = self.records.get(self.memory_selected) {
                    self.cancel = CancellationToken::new();
                    self.progress = vec![None; record.settings.samples];
                    if commands
                        .try_send(Work::Download(record.clone(), self.cancel.clone()))
                        .is_ok()
                    {
                        self.busy = true;
                    }
                }
            }
            KeyCode::Up | KeyCode::Down => {
                let down = key.code == KeyCode::Down;
                if self.tab == 5 {
                    self.memory_selected = if down {
                        (self.memory_selected + 1).min(self.records.len().saturating_sub(1))
                    } else {
                        self.memory_selected.saturating_sub(1)
                    };
                } else {
                    self.selected = if down {
                        (self.selected + 1).min(self.session.sweeps.len().saturating_sub(1))
                    } else {
                        self.selected.saturating_sub(1)
                    };
                    self.cursor = 0;
                    self.recompute();
                }
            }
            KeyCode::Left | KeyCode::Right => {
                let right = key.code == KeyCode::Right;
                if self.tab == 3 {
                    if let Some(Ok(t)) = &self.tdr {
                        let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                            16
                        } else {
                            1
                        };
                        self.tdr_cursor = if right {
                            (self.tdr_cursor + step).min(t.points.len() - 1)
                        } else {
                            self.tdr_cursor.saturating_sub(step)
                        };
                    }
                } else {
                    self.cursor = if right {
                        (self.cursor + 1)
                            .min(self.sweep().map_or(0, |s| s.data.len().saturating_sub(1)))
                    } else {
                        self.cursor.saturating_sub(1)
                    };
                }
            }
            _ => {}
        }
        false
    }
}
fn block(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .title(title.into())
        .border_style(Style::new().fg(Color::DarkGray))
}
fn number(v: f64) -> String {
    if v.is_nan() {
        "undefined".into()
    } else if v.is_infinite() {
        "∞".into()
    } else {
        format!("{v:.3}")
    }
}
fn metric_value(s: &Sample, z0: f64, metric: usize) -> f64 {
    match analysis::metrics(s, z0) {
        Ok(m) => match metric {
            0 => m.swr.min(20.0),
            1 => s.r,
            2 => m.return_loss_db.min(100.0),
            3 => m.magnitude,
            _ => m.phase_degrees,
        },
        Err(_) => f64::NAN,
    }
}
fn chart(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    series: &[PlotSeries],
    xbounds: [f64; 2],
    ylabel: &str,
) {
    let ys: Vec<_> = series
        .iter()
        .flat_map(|(_, points, _)| points.iter())
        .filter(|(x, y)| x.is_finite() && y.is_finite() && *x >= xbounds[0] && *x <= xbounds[1])
        .map(|(_, y)| *y)
        .collect();
    let mut ymin = ys.iter().copied().fold(f64::INFINITY, f64::min);
    let mut ymax = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !ymin.is_finite() || !ymax.is_finite() {
        ymin = 0.0;
        ymax = 1.0;
    }
    let pad = ((ymax - ymin) * 0.1).max(0.1);
    ymin -= pad;
    ymax += pad;
    let datasets = series
        .iter()
        .map(|(name, points, color)| {
            Dataset::default()
                .name(name.clone())
                .data(points)
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(*color))
        })
        .collect::<Vec<_>>();
    let chart = Chart::new(datasets)
        .block(block(title))
        .x_axis(
            Axis::default()
                .title("MHz / distance")
                .style(Style::new().fg(Color::Gray))
                .bounds(xbounds)
                .labels([format!("{:.2}", xbounds[0]), format!("{:.2}", xbounds[1])]),
        )
        .y_axis(
            Axis::default()
                .title(ylabel.to_string())
                .style(Style::new().fg(Color::Gray))
                .bounds([ymin, ymax])
                .labels([number(ymin), number(ymax)]),
        );
    frame.render_widget(chart, area);
}
impl App {
    #[expect(
        clippy::cast_precision_loss,
        clippy::float_cmp,
        reason = "RF frequencies and sample indices are represented approximately as f64; validated grid indices round back to integers; Exact equality identifies a zero-span axis or verifies exactly representable wire fixture values"
    )]
    fn visible_bounds(&self, data: &[Sample]) -> [f64; 2] {
        if data.is_empty() {
            return [
                self.settings.start_hz as f64 / 1e6,
                self.settings.stop_hz as f64 / 1e6,
            ];
        }
        let first = data[0].frequency_hz / 1e6;
        let last = data.last().unwrap().frequency_hz / 1e6;
        if first == last {
            return [first - 0.001, last + 0.001];
        }
        let width = (last - first) / self.zoom as f64;
        let center = data[self.cursor.min(data.len() - 1)].frequency_hz / 1e6;
        let start = (center - width / 2.0).clamp(first, last - width);
        [start, start + width]
    }
    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered discovery or UI dispatch stages together for review"
    )]
    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 50 || area.height < 16 {
            frame.render_widget(
                Paragraph::new("Resize to at least 50×16. q exits; Space stops measurement.")
                    .wrap(Wrap { trim: true })
                    .block(block("RigExpert")),
                area,
            );
            return;
        }
        let regions = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);
        let status = if self.connecting {
            "Connecting"
        } else if self.info.is_some() {
            "Connected"
        } else {
            "Disconnected"
        };
        let identity = self
            .info
            .as_ref()
            .map(|i| format!("{}  {}  fw {}", i.name, i.address, i.firmware))
            .unwrap_or_default();
        let received = self.progress.iter().filter(|p| p.is_some()).count();
        frame.render_widget(
            Paragraph::new(format!(
                "{status}  {identity}   {}{}",
                if self.busy {
                    format!("{received}/{}", self.progress.len())
                } else {
                    "Idle".into()
                },
                if self.repeat { "  Repeat" } else { "" }
            ))
            .block(block("RigExpert / BLE")),
            regions[0],
        );
        frame.render_widget(
            Tabs::new(TABS.to_vec())
                .select(self.tab)
                .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan))
                .block(block("Views")),
            regions[1],
        );
        let content =
            Layout::horizontal([Constraint::Length(24), Constraint::Min(20)]).split(regions[2]);
        let items = self
            .session
            .sweeps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let marks = format!(
                    "{}{}{}",
                    if self.overlays.contains(&i) { "B" } else { "" },
                    if self.open == Some(i) { "O" } else { "" },
                    if self.short == Some(i) { "S" } else { "" }
                );
                ListItem::new(format!(
                    "{} {}{}",
                    i + 1,
                    s.name,
                    if marks.is_empty() {
                        String::new()
                    } else {
                        format!(" [{marks}]")
                    }
                ))
            })
            .collect::<Vec<_>>();
        let mut list_state =
            ListState::default().with_selected(if self.session.sweeps.is_empty() {
                None
            } else {
                Some(self.selected)
            });
        frame.render_stateful_widget(
            List::new(items)
                .block(block("Saved sweeps ↑↓"))
                .highlight_style(Style::new().bg(Color::DarkGray))
                .highlight_symbol("› "),
            content[0],
            &mut list_state,
        );
        match self.tab {
            0 => self.draw_live(frame, content[1]),
            1 => self.draw_sweeps(frame, content[1]),
            2 => self.draw_smith(frame, content[1]),
            3 => self.draw_tdr(frame, content[1]),
            4 => self.draw_cable(frame, content[1]),
            _ => self.draw_memory(frame, content[1]),
        }
        frame.render_widget(
            Paragraph::new(self.logs.back().map(String::as_str).unwrap_or_default())
                .style(Style::new().fg(Color::Yellow))
                .block(block("Status")),
            regions[3],
        );
        frame.render_widget(Paragraph::new("Space start/stop · B bands · e settings · r reconnect · s save · x export · l load\nTab view · ←→ cursor · b compare · m metric · +/- zoom · ? help · q quit").style(Style::new().fg(Color::Gray)),regions[4]);
        if let Some(modal) = &self.modal {
            Self::draw_modal(frame, modal);
        }
    }
    fn sample_readout(&self) -> String {
        let Some(sweep) = self.sweep() else {
            return "No measurement yet. Space starts a sweep.".into();
        };
        let Some(sample) = sweep.data.get(self.cursor) else {
            return format!("{:?}", sweep.status);
        };
        match analysis::metrics(sample, sweep.settings.z0) {
            Ok(m) => format!(
                "{:.6} MHz   R {} Ω   X {} Ω   SWR {}   RL {} dB\n|Z| {} Ω   phase {}°   Z0 {} Ω   {:?}",
                sample.frequency_hz / 1e6,
                number(sample.r),
                number(sample.x),
                number(m.swr),
                number(m.return_loss_db),
                number(m.magnitude),
                number(m.phase_degrees),
                sweep.settings.z0,
                sweep.status
            ),
            Err(e) => e.to_string(),
        }
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "RF frequencies and sample indices are represented approximately as f64; validated grid indices round back to integers"
    )]
    fn draw_live(&self, frame: &mut Frame, area: Rect) {
        let sample = self
            .progress
            .iter()
            .rev()
            .flatten()
            .next()
            .copied()
            .or_else(|| self.sweep().and_then(|s| s.data.last().copied()));
        let text = if let Some(s) = sample {
            match analysis::metrics(&s, self.settings.z0) {
                Ok(m) => format!(
                    "Frequency        {:.6} MHz\nResistance       {} Ω\nReactance        {} Ω\nSWR              {}\nReturn loss      {} dB\nImpedance        {} Ω\nPhase            {}°\nSeries L         {}\nSeries C         {}\n\nConfigured live frequency: {:.6} MHz\nRepeat: {} (p toggles)\nSpace starts/stops. e edits frequency and reference impedance.",
                    s.frequency_hz / 1e6,
                    number(s.r),
                    number(s.x),
                    number(m.swr),
                    number(m.return_loss_db),
                    number(m.magnitude),
                    number(m.phase_degrees),
                    m.inductance_h
                        .map_or_else(|| "—".into(), |v| format!("{:.4} nH", v * 1e9)),
                    m.capacitance_f
                        .map_or_else(|| "—".into(), |v| format!("{:.4} pF", v * 1e12)),
                    self.live_hz as f64 / 1e6,
                    self.repeat
                ),
                Err(e) => e.to_string(),
            }
        } else {
            format!(
                "No data yet.\nLive frequency: {:.6} MHz\ne edits frequency; Space acquires; p toggles repeat.",
                self.live_hz as f64 / 1e6
            )
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .block(block("Live impedance")),
            area,
        );
    }
    fn plot_series(&self) -> Vec<PlotSeries> {
        let mut indices = vec![self.selected];
        indices.extend(
            self.overlays
                .iter()
                .copied()
                .filter(|i| *i != self.selected),
        );
        let mut series = Vec::new();
        for (n, i) in indices.into_iter().enumerate() {
            if let Some(s) = self.session.sweeps.get(i) {
                let points = s
                    .data
                    .iter()
                    .map(|v| {
                        (
                            v.frequency_hz / 1e6,
                            metric_value(v, s.settings.z0, self.metric),
                        )
                    })
                    .filter(|(_, y)| y.is_finite())
                    .collect();
                series.push((s.name.clone(), points, COLORS[n % 4]));
                if self.metric == 1 {
                    series.push((
                        format!("{} X", s.name),
                        s.data.iter().map(|v| (v.frequency_hz / 1e6, v.x)).collect(),
                        if n == 0 { Color::Red } else { COLORS[n % 4] },
                    ));
                }
            }
        }
        if self.busy {
            series.push((
                "Acquiring".into(),
                self.progress
                    .iter()
                    .flatten()
                    .map(|s| {
                        (
                            s.frequency_hz / 1e6,
                            metric_value(s, self.settings.z0, self.metric),
                        )
                    })
                    .collect(),
                Color::White,
            ));
        }
        series
    }
    fn draw_sweeps(&self, frame: &mut Frame, area: Rect) {
        let regions = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).split(area);
        let labels = [
            "SWR (display capped at 20)",
            "R / X",
            "Return loss (dB)",
            "|Z| (ohm)",
            "Phase (degrees)",
        ];
        let bounds =
            self.visible_bounds(self.sweep().map(|s| s.data.as_slice()).unwrap_or_default());
        chart(
            frame,
            regions[0],
            labels[self.metric],
            &self.plot_series(),
            bounds,
            labels[self.metric],
        );
        frame.render_widget(
            Paragraph::new(self.sample_readout())
                .wrap(Wrap { trim: true })
                .block(block(format!("Cursor · zoom {}×", self.zoom))),
            regions[1],
        );
    }
    fn draw_smith(&self, frame: &mut Frame, area: Rect) {
        let regions = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).split(area);
        let mut tracks = Vec::new();
        let mut indices = vec![self.selected];
        indices.extend(
            self.overlays
                .iter()
                .copied()
                .filter(|i| *i != self.selected),
        );
        for (n, i) in indices.into_iter().enumerate() {
            if let Some(s) = self.session.sweeps.get(i) {
                tracks.push((
                    s.data
                        .iter()
                        .filter_map(|v| {
                            analysis::reflection(v, s.settings.z0)
                                .ok()
                                .map(|g| (g.re, g.im))
                        })
                        .collect::<Vec<_>>(),
                    COLORS[n % 4],
                ));
            }
        }
        let cursor = self.sweep().and_then(|s| {
            s.data
                .get(self.cursor)
                .and_then(|v| analysis::reflection(v, s.settings.z0).ok())
        });
        frame.render_widget(
            Canvas::default()
                .block(block("Smith chart · normalized reflection"))
                .x_bounds([-1.05, 1.05])
                .y_bounds([-1.05, 1.05])
                .paint(|ctx| {
                    let mut grid = Vec::new();
                    for r in [0.0, 0.2, 0.5, 1.0, 2.0, 5.0] {
                        for n in 0..360 {
                            let a = f64::from(n) * std::f64::consts::PI / 180.0;
                            grid.push((r / (r + 1.0) + a.cos() / (r + 1.0), a.sin() / (r + 1.0)));
                        }
                    }
                    for x in [-5.0f64, -2.0, -1.0, -0.5, -0.2, 0.2, 0.5, 1.0, 2.0, 5.0] {
                        for n in 0..720 {
                            let a = f64::from(n) * std::f64::consts::PI / 360.0;
                            let point = (1.0 + a.cos() / x.abs(), 1.0 / x + a.sin() / x.abs());
                            if point.0 * point.0 + point.1 * point.1 <= 1.002 {
                                grid.push(point);
                            }
                        }
                    }
                    ctx.draw(&Points {
                        coords: &grid,
                        color: Color::DarkGray,
                    });
                    ctx.draw(&CanvasLine {
                        x1: -1.0,
                        y1: 0.0,
                        x2: 1.0,
                        y2: 0.0,
                        color: Color::DarkGray,
                    });
                    for (track, color) in &tracks {
                        ctx.draw(&Points {
                            coords: track,
                            color: *color,
                        });
                    }
                    if let Some(g) = cursor {
                        ctx.print(g.re, g.im, Span::styled("+", Style::new().fg(Color::White)));
                    }
                }),
            regions[0],
        );
        frame.render_widget(
            Paragraph::new(self.sample_readout())
                .wrap(Wrap { trim: true })
                .block(block("Cursor · b selects comparison")),
            regions[1],
        );
    }
    fn distance(&self, metres: f64) -> f64 {
        if self.feet { metres / 0.3048 } else { metres }
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "FFT indices are bounded by the transform size; fractional interpolation indices intentionally truncate"
    )]
    fn draw_tdr(&self, frame: &mut Frame, area: Rect) {
        if let Some(Ok(tdr)) = &self.tdr {
            let regions = Layout::vertical([Constraint::Min(3), Constraint::Length(5)]).split(area);
            let label = if self.feet { "ft" } else { "m" };
            let metric = self.metric % 3;
            let series = vec![(
                "TDR".into(),
                tdr.points
                    .iter()
                    .filter_map(|p| {
                        let y = match metric {
                            0 => Some(p.impulse),
                            1 => Some(p.step),
                            _ => p.impedance_ohm,
                        };
                        y.filter(|y| y.is_finite())
                            .map(|y| (self.distance(p.distance_m), y))
                    })
                    .collect(),
                Color::Cyan,
            )];
            let range = self.distance(tdr.range_m);
            let width = range / self.zoom as f64;
            let center = self.distance(tdr.points[self.tdr_cursor].distance_m);
            let start = (center - width / 2.0).clamp(0.0, range - width);
            chart(
                frame,
                regions[0],
                &format!(
                    "TDR {} · distance ({label})",
                    ["impulse", "step", "impedance"][metric]
                ),
                &series,
                [start, start + width],
                ["reflection", "reflection", "ohm"][metric],
            );
            let p = &tdr.points[self.tdr_cursor];
            frame.render_widget(Paragraph::new(format!("Cursor {:.4} {label} · impulse {} · step {} · Z {} Ω\nResolution ≈ {:.3} {label} · range {:.3} {label} · VF {:.4}\nSpace acquires broadband data · g strongest reflection · m metric\n←→ cursor (Shift: faster) · +/- zoom · u metres/feet",self.distance(p.distance_m),number(p.impulse),number(p.step),p.impedance_ohm.map_or_else(||"undefined".into(), number),self.distance(tdr.resolution_m),self.distance(tdr.range_m),tdr.velocity_factor)).block(block("Estimated TDR · DC extrapolation")),regions[1]);
        } else {
            let reason = self
                .tdr
                .as_ref()
                .and_then(|t| t.as_ref().err())
                .map_or("No sweep selected", String::as_str);
            frame.render_widget(Paragraph::new(format!("{reason}\n\nSpace acquires 100 kHz to device maximum.\nUse e in Cable to set velocity factor.\nTDR requires complete, low-start, uniform broadband data.")).wrap(Wrap { trim:true }).block(block("TDR")),area);
        }
    }
    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered discovery or UI dispatch stages together for review"
    )]
    fn draw_cable(&self, frame: &mut Frame, area: Rect) {
        let c = self.session.cable;
        let open_name = self
            .open
            .and_then(|i| self.session.sweeps.get(i))
            .map_or("not selected", |s| s.name.as_str());
        let short_name = self
            .short
            .and_then(|i| self.session.sweeps.get(i))
            .map_or("not selected", |s| s.name.as_str());
        let mut text = format!(
            "Cable: {} Ω · {:.3} m · VF {:.5}\nLoss: {} conductor + {} dielectric dB/m at {:.3} MHz\n\ne edit · a add cable · d remove cable (creates a new sweep)\no mark open sweep · k mark short sweep\nOpen: {open_name}\nShort: {short_name}\n",
            c.impedance_ohm,
            c.length_m,
            c.velocity_factor,
            c.conductor_loss_db_per_m,
            c.dielectric_loss_db_per_m,
            c.reference_hz / 1e6
        );
        if let (Some(o), Some(s)) = (
            self.open.and_then(|i| self.session.sweeps.get(i)),
            self.short.and_then(|i| self.session.sweeps.get(i)),
        ) {
            match analysis::characteristic_impedance(o, s) {
                Ok(z) => {
                    if let Some(z) = z.get(self.cursor.min(z.len().saturating_sub(1))) {
                        let _ = writeln!(
                            &mut text,
                            "Estimated cable Z0: {} + j{} Ω",
                            number(z.re),
                            number(z.im)
                        );
                    }
                }
                Err(e) => {
                    let _ = writeln!(&mut text, "{e}");
                }
            }
        }
        if let Some(s) = self.sweep() {
            if let Some(v) = s.data.get(self.cursor) {
                if self.open == Some(self.selected) || self.short == Some(self.selected) {
                    match analysis::cable_loss(v, c.impedance_ohm) {
                        Ok(loss) => {
                            let _ = writeln!(
                                &mut text,
                                "One-way loss estimate at cursor: {} dB",
                                number(loss)
                            );
                        }
                        Err(e) => {
                            let _ = writeln!(&mut text, "{e}");
                        }
                    }
                }
                let short = analysis::stub_length(
                    v.frequency_hz,
                    self.target_x,
                    c.impedance_ohm,
                    c.velocity_factor,
                    false,
                );
                let open = analysis::stub_length(
                    v.frequency_hz,
                    self.target_x,
                    c.impedance_ohm,
                    c.velocity_factor,
                    true,
                );
                if let (Ok(short), Ok(open)) = (short, open) {
                    let _ = write!(
                        &mut text,
                        "Stub target X {} Ω at {:.6} MHz\n  Short: {:.5} m · Open: {:.5} m (lossless)\n",
                        self.target_x,
                        v.frequency_hz / 1e6,
                        short,
                        open
                    );
                }
            }
            let crossings = analysis::resonances(s);
            let _ = writeln!(
                &mut text,
                "X=0 resonances: {}",
                crossings
                    .iter()
                    .take(8)
                    .map(|f| format!("{:.6} MHz", f / 1e6))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(Ok(tdr)) = &self.tdr
            && let Some(p) = tdr.points.get(self.tdr_cursor)
        {
            let _ = write!(
                &mut text,
                "\nSelected reflection: {:.4} m\nKnown length: {:.4} m; v estimates VF from TDR cursor\nSelect reflection in TDR with ←→ or g.",
                p.distance_m, self.known_length_m
            );
        } else {
            text.push_str("\nAcquire a broadband TDR sweep for cable length/VF.");
        }
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .block(block("Cable tools · ideal open/short estimates")),
            area,
        );
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "RF frequencies and sample indices are represented approximately as f64; validated grid indices round back to integers"
    )]
    fn draw_memory(&self, frame: &mut Frame, area: Rect) {
        let items = self
            .records
            .iter()
            .map(|r| {
                ListItem::new(format!(
                    "Slot {:3}  {}\n  {:.3}–{:.3} MHz · {} samples",
                    r.slot,
                    r.name,
                    r.settings.start_hz as f64 / 1e6,
                    r.settings.stop_hz as f64 / 1e6,
                    r.settings.samples
                ))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(if self.records.is_empty() {
            None
        } else {
            Some(self.memory_selected)
        });
        frame.render_stateful_widget(
            List::new(items)
                .block(block("Device memory · f refresh · Enter download"))
                .highlight_style(Style::new().bg(Color::DarkGray))
                .highlight_symbol("› "),
            area,
            &mut state,
        );
    }
    fn draw_band_picker(frame: &mut Frame, rect: Rect, selected: usize) {
        let border = block("Amateur bands · Shift+B");
        let inner = border.inner(rect);
        frame.render_widget(border, rect);
        let areas = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
        let items = BANDS.iter().map(|band| ListItem::new(band.label()));
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan))
                .highlight_symbol("› "),
            areas[0],
            &mut state,
        );
        let settings = BANDS[selected].settings(SweepSettings::default());
        frame.render_widget(Paragraph::new(format!(
            "Sweep: {}\n↑↓/PgUp/PgDn choose · Enter apply · Esc cancel\nRegional presets; national band limits may differ.",
            bands::range(settings.start_hz, settings.stop_hz)
        )), areas[1]);
    }
    fn draw_modal(frame: &mut Frame, modal: &Modal) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(90);
        let height = area.height.saturating_sub(2).min(22);
        let rect = Rect::new(
            (area.width - width) / 2,
            (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, rect);
        let (title, lines) = match modal {
            Modal::Bands { selected } => {
                Self::draw_band_picker(frame, rect, *selected);
                return;
            }
            Modal::Help => (
                "Help",
                vec![
                    "Space starts/stops measurements; p toggles repetition.".into(),
                    "Tab / Shift-Tab change view; ↑↓ choose sweeps or memory.".into(),
                    "←→ move frequency/TDR cursor; +/- zoom; m changes metric.".into(),
                    "B selects a band (Region 1/2); Enter applies; Space starts.".into(),
                    "e edits settings; Ctrl-U clears a field; Enter applies.".into(),
                    "b toggles comparison (up to three overlays); n renames.".into(),
                    "s saves a JSON session; x exports .csv/.s1p/.json.".into(),
                    "l loads a file; u switches metres/feet in TDR.".into(),
                    "Memory: f lists records; Enter downloads selected slot.".into(),
                    "Cable: o marks open sweep; k marks short sweep.".into(),
                    "a adds cable; d removes cable; e edits cable inputs.".into(),
                    "TDR: g selects strongest reflection; Shift-arrows move faster.".into(),
                    "Cable: v estimates VF from known length and TDR cursor.".into(),
                    "r reconnects; q or Ctrl-C stops and exits.".into(),
                    "Calibration is configured on the analyzer.".into(),
                    "Esc cancels dialogs. Any key closes this help.".into(),
                ],
            ),
            Modal::Settings {
                fields,
                selected,
                cable,
            } => {
                let mut lines = fields
                    .iter()
                    .enumerate()
                    .map(|(i, (name, value))| {
                        ratatui::text::Line::styled(
                            format!("{} {name}: {value}", if i == *selected { "›" } else { " " }),
                            if i == *selected {
                                Style::new().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default()
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                lines.push("".into());
                lines.push("Tab/↑↓ select · Ctrl-U clear · Enter apply · Esc cancel".into());
                (
                    if *cable {
                        "Cable settings"
                    } else {
                        "Measurement settings"
                    },
                    lines,
                )
            }
            Modal::File { action, path } => (
                match action {
                    FileAction::Load => "Load file",
                    FileAction::Save => "Save JSON session",
                    FileAction::Export => "Export selected sweep",
                },
                vec![
                    path.clone().into(),
                    "".into(),
                    "Type path · Ctrl-U clear · Enter submit · Esc cancel".into(),
                    "Formats: .json, .csv, .s1p".into(),
                ],
            ),
            Modal::Overwrite { path, .. } => (
                "Replace existing file?",
                vec![
                    path.display().to_string().into(),
                    "y replaces · n/Esc cancels".into(),
                ],
            ),
            Modal::Rename(name) => (
                "Rename sweep",
                vec![
                    name.clone().into(),
                    "Ctrl-U clear · Enter apply · Esc cancel".into(),
                ],
            ),
        };
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block(title)),
            rect,
        );
    }
}
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}
pub async fn run(options: ConnectionOptions, demo: bool, load: Option<PathBuf>) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::Invalid(
            "TUI requires a terminal; use info/sweep subcommands for scripts".into(),
        ));
    }
    let session = if let Some(path) = load {
        files::load(&path, 50.0)?
    } else {
        Session::default()
    };
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    let (commands, rx) = mpsc::channel(8);
    let (events, mut updates) = mpsc::channel(1024);
    let worker = tokio::spawn(worker(options, demo, rx, events.clone()));
    let mut app = App::new(session);
    let _ = commands.send(Work::Connect).await;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let outcome = async {
        loop {
            while let Ok(update) = updates.try_recv() {
                app.update(update, &commands);
            }
            terminal.draw(|frame| app.draw(frame))?;
            if event::poll(Duration::from_millis(20))?
                && let Event::Key(key) = event::read()?
                && app.key(key, &commands, &events)
            {
                break;
            }
            tokio::select! {
                _ = interrupt.recv() => break,
                _ = terminate.recv() => break,
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        Ok::<_, Error>(())
    }
    .await;
    app.cancel.cancel();
    let _ = commands.send(Work::Shutdown).await;
    // Drain notifications while shutting down so a full progress channel cannot
    // block the device task before BREAK and disconnect.
    let mut worker = worker;
    let cleanup = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! { result=&mut worker=>break result, _=updates.recv()=>{} }
        }
    })
    .await;
    if cleanup.is_err() {
        worker.abort();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn band_picker_applies_regional_ranges_only_on_confirmation() {
        let (commands, mut work) = mpsc::channel(8);
        let (events, _) = mpsc::channel(8);
        let mut app = App::new(Session::default());
        app.info = Some(DeviceInfo::default());
        app.connecting = false;
        app.settings.samples = 101;
        app.settings.z0 = 75.0;
        let original = app.settings;
        app.tab = 3;
        app.key(key(KeyCode::Char('B')), &commands, &events);
        app.key(key(KeyCode::Up), &commands, &events);
        assert_eq!(app.settings, original);
        app.key(key(KeyCode::Esc), &commands, &events);
        assert!(app.modal.is_none());
        assert_eq!(app.settings, original);
        for (region, stop_hz) in [
            (bands::Regions::One, 146_000_000),
            (bands::Regions::Two, 148_000_000),
        ] {
            let selected = BANDS
                .iter()
                .position(|band| band.name == "2 m" && band.region == region)
                .unwrap();
            app.modal = Some(Modal::Bands { selected });
            app.key(key(KeyCode::Enter), &commands, &events);
            assert!(app.modal.is_none());
            assert_eq!(
                app.settings,
                SweepSettings {
                    start_hz: 144_000_000,
                    stop_hz,
                    ..original
                }
            );
            assert_eq!(app.tab, 1);
            assert!(!app.busy);
            assert!(work.try_recv().is_err());
            app.key(key(KeyCode::Char('B')), &commands, &events);
            assert!(
                matches!(app.modal, Some(Modal::Bands { selected: index }) if index == selected)
            );
            app.key(key(KeyCode::Esc), &commands, &events);
        }
        app.key(key(KeyCode::Char(' ')), &commands, &events);
        assert!(
            matches!(work.try_recv().unwrap(), Work::Acquire(settings, _) if settings == app.settings)
        );
        app.key(key(KeyCode::Char('B')), &commands, &events);
        assert!(app.modal.is_none());
    }

    #[test]
    fn band_picker_navigation_wraps_and_rejects_unsupported_ranges() {
        let (commands, _) = mpsc::channel(8);
        let (events, _) = mpsc::channel(8);
        let mut app = App::new(Session::default());
        app.modal = Some(Modal::Bands { selected: 0 });
        app.key(key(KeyCode::Up), &commands, &events);
        assert!(
            matches!(app.modal, Some(Modal::Bands { selected }) if selected == BANDS.len() - 1)
        );
        app.key(key(KeyCode::Down), &commands, &events);
        assert!(matches!(app.modal, Some(Modal::Bands { selected: 0 })));
        app.key(key(KeyCode::PageDown), &commands, &events);
        assert!(matches!(app.modal, Some(Modal::Bands { selected: 10 })));
        app.key(key(KeyCode::PageUp), &commands, &events);
        assert!(matches!(app.modal, Some(Modal::Bands { selected: 0 })));
        app.key(key(KeyCode::End), &commands, &events);
        app.info = Some(DeviceInfo {
            max_hz: 200_000_000,
            ..Default::default()
        });
        let original = app.settings;
        app.key(key(KeyCode::Enter), &commands, &events);
        assert_eq!(app.settings, original);
        assert!(app.modal.is_some());
        app.key(key(KeyCode::Home), &commands, &events);
        assert!(matches!(app.modal, Some(Modal::Bands { selected: 0 })));
    }

    #[test]
    fn controls_and_repeat_keep_acquisition_settings() {
        let (commands, mut work) = mpsc::channel(8);
        let (events, _) = mpsc::channel(8);
        let mut app = App::new(Session::default());
        app.info = Some(DeviceInfo::default());
        app.connecting = false;
        app.repeat = true;
        assert!(!app.key(key(KeyCode::Char(' ')), &commands, &events));
        assert!(app.busy);
        let Work::Acquire(settings, token) = work.try_recv().unwrap() else {
            panic!("missing acquisition")
        };
        assert_eq!(settings, SweepSettings::default());
        assert!(!token.is_cancelled());
        // Switching to Live must not change an already running repeated sweep.
        app.tab = 0;
        let mut sweep = Sweep::new("Measurement", settings);
        sweep.status = SweepStatus::Complete;
        sweep.data = (0..settings.samples)
            .map(|i| Sample {
                frequency_hz: settings.frequency(i),
                r: 50.0,
                x: 0.0,
            })
            .collect();
        app.update(Update::Acquired(sweep), &commands);
        let Work::Acquire(repeated, stop) = work.try_recv().unwrap() else {
            panic!("missing repeat")
        };
        assert_eq!(repeated, settings);
        app.key(key(KeyCode::Char(' ')), &commands, &events);
        assert!(stop.is_cancelled());
        assert!(!app.repeat);
        app.update(Update::Disconnected("lost".into()), &commands);
        assert!(app.info.is_none());
        assert!(!app.busy);
    }
    #[tokio::test]
    async fn every_view_and_dialog_renders_at_multiple_sizes() {
        let mut analyzer = Analyzer::demo().await.unwrap();
        let token = CancellationToken::new();
        let records = analyzer.records(50.0, &token).await.unwrap();
        let sweep = analyzer
            .download(&records[1], &token, |_| {})
            .await
            .unwrap();
        let mut app = App::new(Session {
            sweeps: vec![sweep],
            ..Default::default()
        });
        app.info = Some(analyzer.info().clone());
        app.records = records;
        app.connecting = false;
        for (width, height) in [(120, 40), (80, 24), (50, 16), (30, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for tab in 0..6 {
                app.tab = tab;
                terminal.draw(|frame| app.draw(frame)).unwrap();
                if width >= 80 && tab == 2 {
                    let text = terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .map(ratatui::buffer::Cell::symbol)
                        .collect::<String>();
                    assert!(text.contains("Smith chart"));
                }
            }
            app.modal = Some(Modal::Bands {
                selected: BANDS.len() - 1,
            });
            terminal.draw(|frame| app.draw(frame)).unwrap();
            if width >= 50 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>();
                assert!(text.contains("70 cm"));
                assert!(text.contains("420–450 MHz"));
            }
            app.modal = Some(Modal::Help);
            terminal.draw(|frame| app.draw(frame)).unwrap();
            app.settings_modal(true);
            terminal.draw(|frame| app.draw(frame)).unwrap();
            app.modal = None;
        }
        assert!(app.tdr.as_ref().unwrap().is_ok());
    }
    #[test]
    fn dialogs_validate_settings_and_confirm_overwrite() {
        let (commands, _) = mpsc::channel(8);
        let (events, _) = mpsc::channel(8);
        let mut app = App::new(Session::default());
        app.settings_modal(false);
        if let Some(Modal::Settings { fields, .. }) = &mut app.modal {
            fields[2].1 = "1".into();
        }
        app.key(key(KeyCode::Enter), &commands, &events);
        assert!(app.modal.is_some());
        assert!(app.logs.back().unwrap().contains("samples"));
        app.key(key(KeyCode::Esc), &commands, &events);
        assert!(app.modal.is_none());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.json");
        std::fs::write(&path, "original").unwrap();
        app.file(FileAction::Save, path.clone(), false, &events);
        assert!(matches!(app.modal, Some(Modal::Overwrite { .. })));
        app.key(key(KeyCode::Char('n')), &commands, &events);
        assert!(app.modal.is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }
    #[tokio::test]
    async fn worker_connects_streams_and_shuts_down() {
        let (commands, rx) = mpsc::channel(8);
        let (events, mut updates) = mpsc::channel(64);
        let task = tokio::spawn(worker(ConnectionOptions::default(), true, rx, events));
        commands.send(Work::Connect).await.unwrap();
        assert!(matches!(updates.recv().await, Some(Update::Connecting)));
        assert!(matches!(updates.recv().await, Some(Update::Connected(_))));
        commands
            .send(Work::Acquire(
                SweepSettings {
                    samples: 3,
                    ..Default::default()
                },
                CancellationToken::new(),
            ))
            .await
            .unwrap();
        let mut samples = 0;
        loop {
            match updates.recv().await.unwrap() {
                Update::Progress(Progress::Sample { .. }) => samples += 1,
                Update::Acquired(s) => {
                    assert_eq!(s.status, SweepStatus::Complete);
                    break;
                }
                Update::Failed(e) => panic!("{e}"),
                _ => {}
            }
        }
        assert_eq!(samples, 3);
        commands.send(Work::Shutdown).await.unwrap();
        task.await.unwrap();
    }
}
