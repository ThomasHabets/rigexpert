use crate::{
    DeviceInfo, Error, Progress, Record, Result, Sweep, SweepSettings, SweepStatus,
    protocol::{self, Packet},
    transport::{self, ConnectionOptions, Transport},
};
use std::time::Duration;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

/// Serialized analyzer session. Exclusive `&mut self` prevents overlapping commands.
pub struct Analyzer {
    transport: Box<dyn Transport>,
    info: DeviceInfo,
    pub idle_timeout: Duration,
    pub operation_timeout: Duration,
}
impl Analyzer {
    pub async fn connect(options: &ConnectionOptions) -> Result<Self> {
        Self::with_transport(Box::new(transport::BleTransport::connect(options).await?)).await
    }
    pub async fn demo() -> Result<Self> {
        Self::with_transport(Box::new(transport::DemoTransport::new())).await
    }
    pub async fn with_transport(transport: Box<dyn Transport>) -> Result<Self> {
        let info = transport::default_info(transport.as_ref());
        let mut analyzer = Self {
            transport,
            info,
            idle_timeout: Duration::from_secs(3),
            operation_timeout: Duration::from_secs(120),
        };
        if let Err(e) = analyzer.load_info().await {
            let _ = analyzer.disconnect().await;
            return Err(e);
        }
        Ok(analyzer)
    }
    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }
    async fn receive(&mut self) -> Result<Packet> {
        let bytes = self.transport.receive().await?;
        let p = protocol::check(&bytes)?;
        self.transport.acknowledge(p[19]).await?;
        Ok(p)
    }
    async fn load_info(&mut self) -> Result<()> {
        self.transport
            .send(&protocol::command(protocol::INFO))
            .await?;
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut fields = std::collections::BTreeSet::new();
        loop {
            match timeout(
                self.idle_timeout
                    .min(deadline.saturating_duration_since(Instant::now())),
                self.receive(),
            )
            .await
            {
                Ok(Ok(p)) if p[0] == protocol::INFO => {
                    let field = protocol::apply_info(&p, &mut self.info)?;
                    fields.insert(field);
                    if field == 7 && fields.contains(&0) && fields.contains(&2) {
                        break;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) if fields.contains(&0) && fields.contains(&2) => break,
                Err(_) => return Err(Error::Timeout("device information")),
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout("device information"));
            }
        }
        if !self.info.name.to_uppercase().contains("AA-650") {
            return Err(Error::Invalid(format!(
                "expected AA-650 ZOOM, received {}",
                self.info.name
            )));
        }
        Ok(())
    }
    /// Idle heartbeat; also provides a boundary after draining cancelled frames.
    pub async fn ping(&mut self) -> Result<()> {
        self.transport
            .send(&protocol::command(protocol::PING))
            .await?;
        timeout(self.idle_timeout, async {
            loop {
                let p = self.receive().await?;
                if p[0] == protocol::PING {
                    return Ok(());
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout("ping response"))?
    }
    pub async fn cancel(&mut self) -> Result<()> {
        self.transport
            .send(&protocol::command(protocol::BREAK))
            .await
    }
    pub async fn disconnect(&mut self) -> Result<()> {
        let stop = self.cancel().await;
        let disconnect = self.transport.disconnect().await;
        disconnect.and(stop)
    }
    pub async fn sweep(
        &mut self,
        settings: SweepSettings,
        cancel: &CancellationToken,
        progress: impl FnMut(Progress),
    ) -> Result<Sweep> {
        self.acquire(settings, None, "Measurement".into(), cancel, progress)
            .await
    }
    pub async fn download(
        &mut self,
        record: &Record,
        cancel: &CancellationToken,
        progress: impl FnMut(Progress),
    ) -> Result<Sweep> {
        self.acquire(
            record.settings,
            Some(record.slot),
            record.name.clone(),
            cancel,
            progress,
        )
        .await
    }
    async fn acquire(
        &mut self,
        settings: SweepSettings,
        slot: Option<u8>,
        name: String,
        cancel: &CancellationToken,
        mut progress: impl FnMut(Progress),
    ) -> Result<Sweep> {
        settings.validate(&self.info)?;
        if cancel.is_cancelled() {
            return Err(Error::Invalid("operation already cancelled".into()));
        }
        self.ping().await?;
        self.transport
            .send(&protocol::measure(&settings, slot)?)
            .await?;
        let mut sweep = Sweep::new(name, settings);
        sweep.device = Some(self.info.clone());
        let mut samples = vec![None; settings.samples];
        let deadline = Instant::now() + self.operation_timeout;
        let mut last_progress = Instant::now();
        let mut received = 0;
        let mut corrupt = 0;
        let expected_cmd = if slot.is_some() {
            protocol::DATA
        } else {
            protocol::FRX
        };
        let reason = loop {
            let wait = self
                .idle_timeout
                .saturating_sub(last_progress.elapsed())
                .min(deadline.saturating_duration_since(Instant::now()));
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => break Some("cancelled".to_string()),
                value = timeout(wait,self.receive()) => value,
            };
            match response {
                Err(_) => {
                    break Some(format!(
                        "measurement timed out: {received}/{} samples",
                        settings.samples
                    ));
                }
                Ok(Err(e @ Error::Protocol(_))) | Ok(Err(e @ Error::Invalid(_))) => {
                    corrupt += 1;
                    progress(Progress::Warning(e.to_string()));
                    if corrupt >= 20 {
                        break Some("too many invalid packets".into());
                    }
                }
                Ok(Err(e)) => break Some(e.to_string()),
                Ok(Ok(p)) if p[0] == expected_cmd => {
                    match protocol::samples(&p, self.info.packed, &settings) {
                        Ok(points) => {
                            for (index, sample) in points {
                                let index =
                                    if !self.info.packed && settings.start_hz == settings.stop_hz {
                                        received
                                    } else {
                                        index
                                    };
                                if samples[index].is_none() {
                                    received += 1;
                                    last_progress = Instant::now();
                                    samples[index] = Some(sample);
                                    progress(Progress::Sample { index, sample });
                                }
                            }
                        }
                        Err(e) => {
                            corrupt += 1;
                            progress(Progress::Warning(e.to_string()));
                            if corrupt >= 20 {
                                break Some("too many invalid measurements".into());
                            }
                        }
                    }
                    if received == settings.samples {
                        break None;
                    }
                }
                Ok(Ok(_)) => {}
            }
            if Instant::now() >= deadline {
                break Some("operation deadline exceeded".into());
            }
        };
        sweep.data = samples.into_iter().flatten().collect();
        sweep.status = match reason {
            None => SweepStatus::Complete,
            Some(reason) => SweepStatus::Partial(reason),
        };
        // Stop even on completion. A failed cleanup invalidates the session boundary.
        if let Err(e) = self.cancel().await {
            sweep.status = SweepStatus::Partial(format!("{:?}; stop failed: {e}", sweep.status));
        }
        Ok(sweep)
    }
    pub async fn records(&mut self, z0: f64, cancel: &CancellationToken) -> Result<Vec<Record>> {
        if !z0.is_finite() || z0 <= 0.0 {
            return Err(Error::Invalid("invalid reference impedance".into()));
        }
        self.ping().await?;
        self.transport
            .send(&protocol::command(protocol::LIST))
            .await?;
        let mut assembler = protocol::RecordAssembler::default();
        let mut records = Vec::new();
        let deadline = Instant::now() + self.operation_timeout;
        let outcome = loop {
            let value = tokio::select! {
                _ = cancel.cancelled() => break Err(Error::Invalid("memory listing cancelled".into())),
                value = timeout(self.idle_timeout.min(deadline.saturating_duration_since(Instant::now())),self.receive()) => value,
            };
            match value {
                Err(_) => break Err(Error::Timeout("memory listing")),
                Ok(Err(e)) => break Err(e),
                Ok(Ok(p)) if p[0] == protocol::LIST => {
                    match assembler.push(&p, z0) {
                        Ok(Some(record)) => {
                            if records.iter().any(|r: &Record| r.slot == record.slot) {
                                break Err(Error::Protocol("duplicate memory slot".into()));
                            }
                            records.push(record);
                        }
                        Ok(None) => {}
                        Err(e) => break Err(e),
                    }
                    if p[1] == 0xff {
                        break Ok(records);
                    }
                }
                Ok(Ok(_)) => {}
            }
            if Instant::now() >= deadline {
                break Err(Error::Timeout("memory listing"));
            }
        };
        let stop = self.cancel().await;
        match outcome {
            Ok(records) => {
                stop?;
                Ok(records)
            }
            Err(e) => Err(e),
        }
    }
}
