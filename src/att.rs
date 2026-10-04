//! Direct fixed-channel ATT client for peers whose request timing `BlueZ` rejects.
//! The reader answers peer requests immediately and keeps notifications queued
//! independently of caller cancellation. No daemon or dependency patches needed.
use crate::{
    Error, Result,
    protocol::{self, Packet},
    transport::{self, ConnectionOptions, Transport},
};
use async_trait::async_trait;
use bluer::{
    AdapterEvent, AddressType,
    l2cap::{Security, SecurityLevel, SeqPacket, Socket, SocketAddr},
};
use futures::StreamExt;
use std::{
    net::Shutdown,
    os::fd::{AsRawFd, BorrowedFd},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};
use uuid::Uuid;

pub(crate) struct DirectTransport {
    socket: Arc<SeqPacket>,
    reader: JoinHandle<()>,
    responses: mpsc::UnboundedReceiver<Result<Vec<u8>>>,
    notifications: mpsc::UnboundedReceiver<Result<(u16, Vec<u8>)>>,
    address: String,
    read_handle: u16,
    write_handle: u16,
    write_request: bool,
    return_handle: Option<(u16, bool)>,
    _session: bluer::Session,
}
#[derive(Debug)]
struct Characteristic {
    declaration: u16,
    value: u16,
    properties: u8,
    uuid: Uuid,
}
fn invalid(message: &str) -> Error {
    Error::Protocol(format!("ATT: {message}"))
}
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let data = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| invalid("truncated handle"))?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}
fn uuid(bytes: &[u8]) -> Result<Uuid> {
    match bytes.len() {
        2 => Ok(Uuid::from_u128(
            (u128::from(u16_at(bytes, 0)?) << 96) | 0x0000_1000_8000_0080_5f9b_34fb,
        )),
        16 => {
            let mut bytes: [u8; 16] = bytes.try_into().unwrap();
            bytes.reverse();
            Ok(Uuid::from_bytes(bytes))
        }
        _ => Err(invalid("invalid UUID width")),
    }
}
fn entries<'a>(response: &'a [u8], opcode: u8, widths: &[usize]) -> Result<Option<Vec<&'a [u8]>>> {
    if response.first() == Some(&1) {
        if response.len() != 5 {
            return Err(invalid("malformed error response"));
        }
        if response[1] != opcode - 1 {
            return Err(invalid("error for unexpected request"));
        }
        if response[4] == 0x0a {
            return Ok(None);
        }
        return Err(invalid(&format!(
            "request 0x{:02x} failed with code 0x{:02x}",
            response[1], response[4]
        )));
    }
    if response.len() < 2 || response[0] != opcode {
        return Err(invalid("unexpected discovery response"));
    }
    let width = response[1] as usize;
    if !widths.contains(&width)
        || response.len() == 2
        || !(response.len() - 2).is_multiple_of(width)
    {
        return Err(invalid("malformed discovery entries"));
    }
    Ok(Some(response[2..].chunks_exact(width).collect()))
}
// This client exports no local services. Answer the peer's optional UART
// discovery with Attribute Not Found, while accepting its MTU request.
fn peer_reply(packet: &[u8]) -> Option<Vec<u8>> {
    match packet.first().copied()? {
        0x02 if packet.len() == 3 => Some(vec![0x03, 23, 0]),
        0x1d if packet.len() >= 3 => Some(vec![0x1e]),
        opcode @ (0x04 | 0x06 | 0x08 | 0x10) if packet.len() >= 3 => {
            Some(vec![1, opcode, packet[1], packet[2], 0x0a])
        }
        opcode @ (0x0a | 0x0c | 0x12 | 0x16) if packet.len() >= 3 => {
            Some(vec![1, opcode, packet[1], packet[2], 0x01])
        }
        _ => None,
    }
}
fn spawn_reader(
    reader_socket: Arc<SeqPacket>,
    response_tx: mpsc::UnboundedSender<Result<Vec<u8>>>,
    notification_tx: mpsc::UnboundedSender<Result<(u16, Vec<u8>)>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0u8; 517];
        let failure = loop {
            let count = match reader_socket.recv(&mut buffer).await {
                Ok(0) => break "ATT peer closed the connection".to_owned(),
                Err(error) => break format!("ATT receive: {error}"),
                Ok(count) => count,
            };
            let packet = &buffer[..count];
            if let Some(reply) = peer_reply(packet)
                && let Err(error) = reader_socket.send(&reply).await
            {
                break format!("ATT peer response: {error}");
            }
            match packet[0] {
                0x1b | 0x1d if packet.len() >= 3 => {
                    let handle = u16::from_le_bytes([packet[1], packet[2]]);
                    if notification_tx
                        .send(Ok((handle, packet[3..].to_vec())))
                        .is_err()
                    {
                        return;
                    }
                }
                0x01 | 0x03 | 0x05 | 0x07 | 0x09 | 0x0b | 0x0d | 0x11 | 0x13 | 0x17 | 0x19
                    if response_tx.send(Ok(packet.to_vec())).is_err() =>
                {
                    return;
                }
                _ => {}
            }
        };
        let _ = response_tx.send(Err(Error::Bluetooth(failure.clone())));
        let _ = notification_tx.send(Err(Error::Bluetooth(failure)));
    })
}
impl DirectTransport {
    pub(crate) async fn connect(options: &ConnectionOptions) -> Result<Self> {
        timeout(options.timeout, Self::connect_inner(options))
            .await
            .map_err(|_| Error::Timeout("direct ATT connection and discovery"))?
    }
    async fn connect_inner(options: &ConnectionOptions) -> Result<Self> {
        let address: bluer::Address = options
            .address
            .parse()
            .map_err(|_| Error::Invalid("invalid Bluetooth address".into()))?;
        let session = bluer::Session::new().await?;
        let adapter = transport::adapter(&session, options.adapter.as_deref()).await?;
        adapter
            .set_discovery_filter(transport::discovery_filter(Some(address)))
            .await?;
        let mut discovery = adapter.discover_devices().await?;
        let device = loop {
            let device = adapter.device(address)?;
            if device.name().await.ok().flatten().is_some() {
                break device;
            }
            match discovery.next().await {
                Some(AdapterEvent::DeviceAdded(a)) if a == address => break adapter.device(a)?,
                Some(_) => {}
                None => return Err(Error::Disconnected),
            }
        };
        let socket = Socket::new_seq_packet()?;
        socket.set_security(Security {
            level: SecurityLevel::Low,
            key_size: 0,
        })?;
        socket.bind(SocketAddr {
            addr: adapter.address().await?,
            addr_type: AddressType::LePublic,
            psm: 0,
            cid: 4,
        })?;
        let socket = Arc::new(
            socket
                .connect(SocketAddr {
                    addr: address,
                    addr_type: device.address_type().await?,
                    psm: 0,
                    cid: 4,
                })
                .await
                .map_err(|e| Error::Bluetooth(format!("ATT socket connect: {e}")))?,
        );
        // bluer 0.17 can consume writable readiness cached before connect and
        // return while SO_ERROR is still zero. A fresh registration waits for
        // actual connection completion instead of reusing that stale event.
        // SAFETY: socket owns this descriptor and remains alive through the
        // immediate duplication; the duplicate has independent ownership.
        let descriptor =
            unsafe { BorrowedFd::borrow_raw(socket.as_raw_fd()) }.try_clone_to_owned()?;
        let completion = tokio::io::unix::AsyncFd::new(descriptor)?;
        let _ready = completion.writable().await?;
        socket
            .peer_addr()
            .map_err(|error| Error::Bluetooth(format!("ATT connection completion: {error}")))?;
        drop(discovery);
        let (response_tx, responses) = mpsc::unbounded_channel();
        let (notification_tx, notifications) = mpsc::unbounded_channel();
        let reader = spawn_reader(socket.clone(), response_tx, notification_tx);
        let mut transport = Self {
            socket,
            reader,
            responses,
            notifications,
            address: address.to_string(),
            read_handle: 0,
            write_handle: 0,
            write_request: true,
            return_handle: None,
            _session: session,
        };
        transport.discover().await?;
        Ok(transport)
    }
    async fn exchange(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        timeout(Duration::from_secs(3), async {
            self.socket
                .send(packet)
                .await
                .map_err(|e| Error::Bluetooth(format!("ATT send: {e}")))?;
            self.responses.recv().await.ok_or(Error::Disconnected)?
        })
        .await
        .map_err(|_| Error::Timeout("ATT response"))?
    }
    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered discovery or UI dispatch stages together for review"
    )]
    async fn discover(&mut self) -> Result<()> {
        let wanted = Uuid::parse_str(protocol::SERVICE).unwrap();
        let mut start = 1u16;
        let range = loop {
            let mut request = vec![0x10];
            request.extend(start.to_le_bytes());
            request.extend(u16::MAX.to_le_bytes());
            request.extend(0x2800u16.to_le_bytes());
            let response = self.exchange(&request).await?;
            let Some(records) = entries(&response, 0x11, &[6, 20])? else {
                return Err(invalid("RigExpert service not found"));
            };
            let mut found = None;
            let mut last = 0;
            for record in records {
                let begin = u16_at(record, 0)?;
                let end = u16_at(record, 2)?;
                if begin < start || end < begin || begin <= last {
                    return Err(invalid("invalid service handle range"));
                }
                last = end;
                if uuid(&record[4..])? == wanted {
                    found = Some((begin, end));
                }
            }
            if let Some(range) = found {
                break range;
            }
            start = last
                .checked_add(1)
                .ok_or_else(|| invalid("RigExpert service not found"))?;
        };
        let mut characteristics = Vec::new();
        start = range.0;
        while start <= range.1 {
            let mut request = vec![0x08];
            request.extend(start.to_le_bytes());
            request.extend(range.1.to_le_bytes());
            request.extend(0x2803u16.to_le_bytes());
            let response = self.exchange(&request).await?;
            let Some(records) = entries(&response, 0x09, &[7, 21])? else {
                break;
            };
            let mut last = 0;
            for record in records {
                let declaration = u16_at(record, 0)?;
                let value = u16_at(record, 3)?;
                if declaration < start
                    || declaration <= last
                    || value <= declaration
                    || value > range.1
                {
                    return Err(invalid("invalid characteristic handles"));
                }
                last = declaration;
                characteristics.push(Characteristic {
                    declaration,
                    value,
                    properties: record[2],
                    uuid: uuid(&record[5..])?,
                });
            }
            let Some(next) = last.checked_add(1) else {
                break;
            };
            start = next;
        }
        let read_uuid = Uuid::parse_str(protocol::READ).unwrap();
        let write_uuid = Uuid::parse_str(protocol::WRITE).unwrap();
        let return_uuid = Uuid::parse_str(protocol::RETURN).unwrap();
        let index = characteristics
            .iter()
            .position(|c| c.uuid == read_uuid && c.properties & 0x10 != 0)
            .ok_or_else(|| invalid("RigExpert notification characteristic missing"))?;
        self.read_handle = characteristics[index].value;
        let descriptor_end = characteristics
            .get(index + 1)
            .map_or(range.1, |c| c.declaration - 1);
        let write = characteristics
            .iter()
            .find(|c| c.uuid == write_uuid && c.properties & 0x0c != 0)
            .ok_or_else(|| invalid("RigExpert command characteristic missing"))?;
        self.write_handle = write.value;
        self.write_request = write.properties & 0x08 != 0;
        self.return_handle = characteristics
            .iter()
            .find(|c| c.uuid == return_uuid && c.properties & 0x0c != 0)
            .map(|c| (c.value, c.properties & 0x08 != 0));
        let mut cursor = self
            .read_handle
            .checked_add(1)
            .ok_or_else(|| invalid("missing CCC descriptor"))?;
        let ccc = loop {
            if cursor > descriptor_end {
                return Err(invalid("missing CCC descriptor"));
            }
            let mut request = vec![0x04];
            request.extend(cursor.to_le_bytes());
            request.extend(descriptor_end.to_le_bytes());
            let response = self.exchange(&request).await?;
            // Find Information uses a format byte, rather than an entry length.
            let mut normalized = response;
            if normalized.first() == Some(&0x05) && normalized.len() >= 2 {
                normalized[1] = match normalized[1] {
                    1 => 4,
                    2 => 18,
                    _ => return Err(invalid("invalid descriptor format")),
                };
            }
            let Some(records) = entries(&normalized, 0x05, &[4, 18])? else {
                return Err(invalid("missing CCC descriptor"));
            };
            let mut found = None;
            let mut last = 0;
            for record in records {
                let handle = u16_at(record, 0)?;
                if handle < cursor || handle <= last || handle > descriptor_end {
                    return Err(invalid("invalid descriptor handle"));
                }
                last = handle;
                if uuid(&record[2..])? == uuid(&[0x02, 0x29])? {
                    found = Some(handle);
                }
            }
            if let Some(handle) = found {
                break handle;
            }
            cursor = last
                .checked_add(1)
                .ok_or_else(|| invalid("missing CCC descriptor"))?;
        };
        self.write_value(ccc, true, &[1, 0]).await
    }
    async fn write_value(&mut self, handle: u16, request: bool, data: &[u8]) -> Result<()> {
        if data.len() > 20 {
            return Err(invalid("write exceeds default MTU"));
        }
        let mut packet = vec![if request { 0x12 } else { 0x52 }];
        packet.extend(handle.to_le_bytes());
        packet.extend(data);
        if request {
            let response = self.exchange(&packet).await?;
            if response != [0x13] {
                return Err(invalid(&format!("write rejected: {response:02x?}")));
            }
        } else {
            timeout(Duration::from_secs(3), self.socket.send(&packet))
                .await
                .map_err(|_| Error::Timeout("ATT write"))??;
        }
        Ok(())
    }
}
impl Drop for DirectTransport {
    fn drop(&mut self) {
        self.reader.abort();
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
#[async_trait]
impl Transport for DirectTransport {
    fn packed(&self) -> bool {
        self.return_handle.is_some()
    }
    fn address(&self) -> String {
        self.address.clone()
    }
    async fn send(&mut self, packet: &Packet) -> Result<()> {
        self.write_value(self.write_handle, self.write_request, packet)
            .await
    }
    async fn receive(&mut self) -> Result<Vec<u8>> {
        loop {
            let (handle, data) = self
                .notifications
                .recv()
                .await
                .ok_or(Error::Disconnected)??;
            if handle == self.read_handle {
                return Ok(data);
            }
        }
    }
    async fn acknowledge(&mut self, crc: u8) -> Result<()> {
        if let Some((handle, request)) = self.return_handle {
            self.write_value(handle, request, &[crc]).await?;
        }
        Ok(())
    }
    async fn disconnect(&mut self) -> Result<()> {
        self.reader.abort();
        self.socket.shutdown(Shutdown::Both)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peer_requests_are_answered_without_pending_request_rejection() {
        assert_eq!(peer_reply(&[2, 158, 0]), Some(vec![3, 23, 0]));
        assert_eq!(
            peer_reply(&[6, 1, 0, 255, 255, 0, 40]),
            Some(vec![1, 6, 1, 0, 10])
        );
        assert_eq!(peer_reply(&[0x1d, 114, 0, 7]), Some(vec![0x1e]));
        assert_eq!(peer_reply(&[2]), None);
    }
    #[test]
    fn discovery_validates_wire_data_and_little_endian_uuids() {
        assert!(entries(&[0x11, 0], 0x11, &[6, 20]).is_err());
        assert!(entries(&[0x11, 6, 1], 0x11, &[6, 20]).is_err());
        assert!(
            entries(&[1, 16, 1, 0, 10], 0x11, &[6, 20])
                .unwrap()
                .is_none()
        );
        assert!(entries(&[1, 8, 1, 0, 10], 0x11, &[6, 20]).is_err());
        assert_eq!(
            uuid(&[0x02, 0x29]).unwrap().to_string(),
            "00002902-0000-1000-8000-00805f9b34fb"
        );
        let mut bytes = *Uuid::parse_str(protocol::SERVICE).unwrap().as_bytes();
        bytes.reverse();
        assert_eq!(uuid(&bytes).unwrap().to_string(), protocol::SERVICE);
    }
}
