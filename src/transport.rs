//! Linux `BlueZ` transport and deterministic simulated analyzer.
use crate::{
    DEFAULT_ADDRESS, DeviceInfo, Error, Result,
    protocol::{self, Packet},
};
use async_trait::async_trait;
use bluer::{
    AdapterEvent, DiscoveryFilter, DiscoveryTransport,
    gatt::{
        WriteOp,
        remote::{Characteristic, CharacteristicWriteRequest},
    },
};
use futures::{StreamExt, stream::BoxStream};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};
use tokio::time::{Instant, timeout};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ConnectionOptions {
    pub address: String,
    pub adapter: Option<String>,
    pub timeout: Duration,
}
impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            address: DEFAULT_ADDRESS.into(),
            adapter: None,
            timeout: Duration::from_secs(15),
        }
    }
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct DiscoveredDevice {
    pub address: String,
    pub name: String,
    pub connected: bool,
}

/// A transport owns its notification subscription. `receive` must be cancellation
/// safe; dropping a pending receive must not discard a notification.
#[async_trait]
pub trait Transport: Send {
    fn packed(&self) -> bool;
    fn address(&self) -> String;
    async fn send(&mut self, packet: &Packet) -> Result<()>;
    async fn receive(&mut self) -> Result<Vec<u8>>;
    async fn acknowledge(&mut self, _crc: u8) -> Result<()> {
        Ok(())
    }
    async fn disconnect(&mut self) -> Result<()>;
}
/// BLE transport using direct ATT to tolerate the analyzer's peer request timing.
pub struct BleTransport {
    inner: Box<dyn Transport>,
}
impl BleTransport {
    ///
    /// # Errors
    /// Returns an error if neither direct ATT nor the `BlueZ` GATT connection succeeds.
    pub async fn connect(options: &ConnectionOptions) -> Result<Self> {
        match crate::att::DirectTransport::connect(options).await {
            Ok(transport) => Ok(Self {
                inner: Box::new(transport),
            }),
            Err(direct_error) => {
                let bluez = BluezTransport::connect(options).await
                    .map_err(|bluez_error| Error::Bluetooth(format!(
                        "Direct ATT connection failed: {direct_error}; BlueZ fallback failed: {bluez_error}"
                    )))?;
                Ok(Self {
                    inner: Box::new(bluez),
                })
            }
        }
    }
}
#[async_trait]
impl Transport for BleTransport {
    fn packed(&self) -> bool {
        self.inner.packed()
    }
    fn address(&self) -> String {
        self.inner.address()
    }
    async fn send(&mut self, packet: &Packet) -> Result<()> {
        self.inner.send(packet).await
    }
    async fn receive(&mut self) -> Result<Vec<u8>> {
        self.inner.receive().await
    }
    async fn acknowledge(&mut self, crc: u8) -> Result<()> {
        self.inner.acknowledge(crc).await
    }
    async fn disconnect(&mut self) -> Result<()> {
        self.inner.disconnect().await
    }
}
struct BluezTransport {
    _session: bluer::Session,
    device: bluer::Device,
    write: Characteristic,
    return_crc: Option<Characteristic>,
    notifications: BoxStream<'static, Vec<u8>>,
    closed: bool,
}
pub(crate) async fn adapter(
    session: &bluer::Session,
    name: Option<&str>,
) -> Result<bluer::Adapter> {
    let adapter = match name {
        Some(n) => session.adapter(n)?,
        None => session.default_adapter().await?,
    };
    if !adapter.is_powered().await? {
        return Err(Error::Bluetooth(format!(
            "{} is powered off; run: bluetoothctl power on",
            adapter.name()
        )));
    }
    Ok(adapter)
}
pub(crate) fn discovery_filter(address: Option<bluer::Address>) -> DiscoveryFilter {
    // AA-650 advertisements do not include the RigExpert service UUID.
    DiscoveryFilter {
        transport: DiscoveryTransport::Le,
        pattern: address.map(|address| address.to_string()),
        ..Default::default()
    }
}
///
/// # Errors
/// Returns an error if the adapter is unavailable or powered off, discovery fails, or its deadline expires.
pub async fn scan(adapter_name: Option<&str>, duration: Duration) -> Result<Vec<DiscoveredDevice>> {
    timeout(duration + Duration::from_secs(5), async {
        let session = bluer::Session::new().await?;
        let adapter = adapter(&session, adapter_name).await?;
        let mut devices = BTreeMap::new();
        adapter.set_discovery_filter(discovery_filter(None)).await?;
        let mut discovery = adapter.discover_devices().await?;
        let deadline = Instant::now() + duration;
        loop {
            let addresses = adapter.device_addresses().await?;
            for address in addresses {
                let d = adapter.device(address)?;
                if let Ok(Some(name)) = d.name().await
                    && (name.to_ascii_uppercase().contains("AA-")
                        || name.to_ascii_uppercase().contains("RIGEXPERT"))
                {
                    devices.insert(
                        address,
                        DiscoveredDevice {
                            address: address.to_string(),
                            name,
                            connected: d.is_connected().await.unwrap_or(false),
                        },
                    );
                }
            }
            if timeout(
                deadline.saturating_duration_since(Instant::now()),
                discovery.next(),
            )
            .await
            .is_err()
            {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        Ok(devices.into_values().collect())
    })
    .await
    .map_err(|_| Error::Timeout("Bluetooth discovery"))?
}
impl BluezTransport {
    pub async fn connect(options: &ConnectionOptions) -> Result<Self> {
        let mut phase = "BlueZ adapter initialization";
        let mut cleanup = ConnectionCleanup(None);
        let outcome = match timeout(
            options.timeout,
            Self::connect_inner(options, &mut phase, &mut cleanup),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::Timeout(phase)),
        };
        // Await cleanup before returning an error: CLI callers may immediately
        // tear down their runtime, preventing a spawned Drop task from running.
        if outcome.is_err()
            && let Some(device) = cleanup.0.take()
        {
            let _ = timeout(Duration::from_secs(3), device.disconnect()).await;
        }
        outcome
    }
    async fn connect_inner(
        options: &ConnectionOptions,
        phase: &mut &'static str,
        cleanup: &mut ConnectionCleanup,
    ) -> Result<Self> {
        let address = options
            .address
            .parse()
            .map_err(|_| Error::Invalid("invalid Bluetooth address".into()))?;
        let session = bluer::Session::new().await?;
        let adapter = adapter(&session, options.adapter.as_deref()).await?;
        adapter
            .set_discovery_filter(discovery_filter(Some(address)))
            .await?;
        *phase = "AA-650 BLE advertisement";
        let mut discovery = adapter.discover_devices().await?;
        // Resolve the target afresh; a stale cached BlueZ path may not be usable.
        let device = loop {
            let d = adapter.device(address)?;
            if d.name().await.ok().flatten().is_some() {
                break d;
            }
            match discovery.next().await {
                Some(AdapterEvent::DeviceAdded(a)) if a == address => break adapter.device(a)?,
                Some(_) => {}
                None => return Err(Error::Disconnected),
            }
        };
        cleanup.0 = Some(device.clone());
        *phase = "AA-650 BLE link establishment";
        if !device.is_connected().await? {
            device.connect().await?;
        }
        drop(discovery);
        *phase = "RigExpert GATT service resolution";
        let discovered = async {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let mut read = None;
                let mut write = None;
                let mut return_crc = None;
                let mut seen = Vec::new();
                let services = device.services().await.map_err(|e| Error::Bluetooth(format!(
                    "{}: GATT service discovery failed on {}: {e}", options.address, adapter.name()
                )))?;
                for service in services {
                    let id = service.uuid().await?;
                    seen.push(format!("service {id}"));
                    if id != Uuid::parse_str(protocol::SERVICE).unwrap() { continue; }
                    for c in service.characteristics().await? {
                        let id = c.uuid().await?.to_string();
                        seen.push(format!("characteristic {id}"));
                        match id.as_str() {
                            protocol::READ => read = Some(c),
                            protocol::WRITE => write = Some(c),
                            protocol::RETURN => return_crc = Some(c),
                            _ => {},
                        }
                    }
                }
                if let (Some(read), Some(write)) = (read, write) {
                    *phase = "RigExpert GATT notification subscription";
                    let notifications = read.notify().await?.boxed();
                    return Ok::<_, Error>((write, return_crc, notifications));
                }
                if Instant::now() >= deadline {
                    return Err(Error::Protocol(format!(
                        "RigExpert GATT characteristics unavailable. Discovered: {}. Keep Bluetooth enabled on the analyzer and close competing clients; inspect with bluetoothctl info {}",
                        if seen.is_empty() { "no services".into() } else { seen.join(", ") }, options.address)));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }.await;
        match discovered {
            Ok((write, return_crc, notifications)) => {
                cleanup.0 = None;
                Ok(Self {
                    _session: session,
                    device,
                    write,
                    return_crc,
                    notifications,
                    closed: false,
                })
            }
            Err(e) => Err(e),
        }
    }
}
// Failed or cancelled connection attempts must not leave the peripheral busy.
struct ConnectionCleanup(Option<bluer::Device>);
impl Drop for ConnectionCleanup {
    fn drop(&mut self) {
        if let Some(device) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = timeout(Duration::from_secs(3), device.disconnect()).await;
            });
        }
    }
}
impl Drop for BluezTransport {
    fn drop(&mut self) {
        if !self.closed
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let device = self.device.clone();
            let write = self.write.clone();
            runtime.spawn(async move {
                let _ = write_characteristic(&write, &protocol::command(protocol::BREAK)).await;
                let _ = timeout(Duration::from_secs(3), device.disconnect()).await;
            });
        }
    }
}
async fn write_characteristic(c: &Characteristic, data: &[u8]) -> Result<()> {
    timeout(Duration::from_secs(3), async {
        let flags = c.flags().await?;
        if !flags.write && !flags.write_without_response {
            return Err(Error::Protocol(
                "GATT characteristic is not writable".into(),
            ));
        }
        let req = CharacteristicWriteRequest {
            op_type: if flags.write {
                WriteOp::Request
            } else {
                WriteOp::Command
            },
            ..Default::default()
        };
        c.write_ext(data, &req).await?;
        Ok(())
    })
    .await
    .map_err(|_| Error::Timeout("GATT write"))?
}
#[async_trait]
impl Transport for BluezTransport {
    fn packed(&self) -> bool {
        self.return_crc.is_some()
    }
    fn address(&self) -> String {
        self.device.address().to_string()
    }
    async fn send(&mut self, packet: &Packet) -> Result<()> {
        write_characteristic(&self.write, packet).await
    }
    async fn receive(&mut self) -> Result<Vec<u8>> {
        self.notifications.next().await.ok_or(Error::Disconnected)
    }
    async fn acknowledge(&mut self, crc: u8) -> Result<()> {
        if let Some(c) = &self.return_crc {
            write_characteristic(c, &[crc]).await?;
        }
        Ok(())
    }
    async fn disconnect(&mut self) -> Result<()> {
        timeout(Duration::from_secs(3), self.device.disconnect())
            .await
            .map_err(|_| Error::Timeout("disconnect"))??;
        self.closed = true;
        Ok(())
    }
}

pub struct DemoTransport {
    queue: VecDeque<Packet>,
    closed: bool,
}
impl Default for DemoTransport {
    fn default() -> Self {
        Self::new()
    }
}
impl DemoTransport {
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            closed: false,
        }
    }
    fn info(&mut self) {
        let mut p = protocol::command(protocol::INFO);
        p[1] = 0;
        p[2..13].copy_from_slice(b"AA-650 DEMO");
        self.queue.push_back(protocol::seal(p));
        p = protocol::command(protocol::INFO);
        p[1] = 2;
        p[2..6].copy_from_slice(&100u32.to_le_bytes());
        p[6..10].copy_from_slice(&650_000u32.to_le_bytes());
        p[10] = 3;
        p[11..13].copy_from_slice(&500u16.to_le_bytes());
        self.queue.push_back(protocol::seal(p));
        p = protocol::command(protocol::INFO);
        p[1] = 3;
        p[2..7].copy_from_slice(b"DEMO\0");
        p[7..9].copy_from_slice(&1u16.to_le_bytes());
        self.queue.push_back(protocol::seal(p));
        p = protocol::command(protocol::INFO);
        p[1] = 7;
        self.queue.push_back(protocol::seal(p));
    }
    fn records(&mut self) {
        for (slot, name) in [
            (0, b"Reference".as_slice()),
            (1, b"Cable open".as_slice()),
            (2, b"Cable short".as_slice()),
        ] {
            let mut p = protocol::command(protocol::LIST);
            p[1] = 0;
            p[2..10].copy_from_slice(&325_050_000u64.to_le_bytes());
            p[10..18].copy_from_slice(&649_900_000u64.to_le_bytes());
            p[18] = slot;
            self.queue.push_back(protocol::seal(p));
            p = protocol::command(protocol::LIST);
            p[1] = 1;
            p[2..4].copy_from_slice(&501u16.to_le_bytes());
            p[4..4 + name.len()].copy_from_slice(name);
            self.queue.push_back(protocol::seal(p));
            p = protocol::command(protocol::LIST);
            p[1] = 2;
            self.queue.push_back(protocol::seal(p));
        }
        let mut p = protocol::command(protocol::LIST);
        p[1] = 0xff;
        self.queue.push_back(protocol::seal(p));
    }
}
#[async_trait]
impl Transport for DemoTransport {
    fn packed(&self) -> bool {
        false
    }
    fn address(&self) -> String {
        "demo".into()
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "Simulated wire fixtures use bounded indices and intentionally round to the protocol float/integer widths"
    )]
    async fn send(&mut self, packet: &Packet) -> Result<()> {
        if self.closed {
            return Err(Error::Disconnected);
        }
        let p = protocol::check(packet)?;
        match p[0] {
            protocol::BREAK => self.queue.clear(),
            protocol::PING => self.queue.push_back(protocol::command(protocol::PING)),
            protocol::INFO => self.info(),
            protocol::LIST => self.records(),
            protocol::FRX | protocol::DATA => {
                let offset = if p[0] == protocol::DATA { 2 } else { 1 };
                let center = u64::from(u32::from_le_bytes(
                    p[offset..offset + 4].try_into().unwrap(),
                )) * 1000;
                let span = u64::from(u32::from_le_bytes(
                    p[offset + 4..offset + 8].try_into().unwrap(),
                )) * 1000;
                let intervals =
                    u32::from_le_bytes(p[offset + 8..offset + 12].try_into().unwrap()) as usize;
                if intervals == 0 || intervals > 500 {
                    return Err(Error::Invalid("invalid demo interval count".into()));
                }
                for i in 0..=intervals {
                    let f = center as f64 - span as f64 / 2.0
                        + span as f64 * i as f64 / intervals as f64;
                    let z = if p[0] == protocol::DATA && p[1] > 0 {
                        let gamma = num_complex::Complex64::from_polar(
                            if p[1] == 1 { 0.8 } else { -0.8 },
                            -4.0 * std::f64::consts::PI * f * 10.0 / (299_792_458.0 * 0.66),
                        );
                        50.0 * (1.0 + gamma) / (1.0 - gamma)
                    } else {
                        num_complex::Complex64::new(
                            50.0 + 10.0 * ((f - 145_500_000.0) / 2_000_000.0).sin().powi(2),
                            (f - 145_500_000.0) / 100_000.0,
                        )
                    };
                    let mut value = protocol::command(p[0]);
                    value[1..9].copy_from_slice(&(f.round() as u64).to_le_bytes());
                    value[9..13].copy_from_slice(&(z.re as f32).to_le_bytes());
                    value[13..17].copy_from_slice(&(z.im as f32).to_le_bytes());
                    value[17..19].copy_from_slice(&(i as u16).to_le_bytes());
                    self.queue.push_back(protocol::seal(value));
                }
            }
            _ => return Err(Error::Protocol("unsupported demo command".into())),
        }
        Ok(())
    }
    async fn receive(&mut self) -> Result<Vec<u8>> {
        if self.closed {
            return Err(Error::Disconnected);
        }
        // Sleep before popping so receive remains cancellation safe.
        if !self.queue.is_empty() {
            tokio::time::sleep(Duration::from_millis(3)).await;
            return Ok(self.queue.pop_front().unwrap().to_vec());
        }
        std::future::pending().await
    }
    async fn disconnect(&mut self) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}
/// Conservative fallback until the analyzer's full-info response is received.
pub fn default_info(transport: &dyn Transport) -> DeviceInfo {
    DeviceInfo {
        address: transport.address(),
        packed: transport.packed(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_accepts_analyzers_without_advertised_service_uuids() {
        let filter = discovery_filter(Some("04:91:62:ae:ba:ed".parse().unwrap()));
        assert_eq!(filter.transport, DiscoveryTransport::Le);
        assert_eq!(filter.pattern.as_deref(), Some("04:91:62:AE:BA:ED"));
        assert!(filter.uuids.is_empty());
        assert!(discovery_filter(None).pattern.is_none());
    }
}
