use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rustls::pki_types::ServerName;
use socket2::TcpKeepalive;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt, BufReader, split},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Mutex, Semaphore, mpsc, oneshot},
    time::MissedTickBehavior,
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::{
    GLOBAL_CONFIG,
    device::{Device, DeviceId},
    plugin_config,
    protocol::{Identity, PacketType, ProtocolPacket},
};

pub const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);

pub const DEFAULT_LISTEN_PORT: u16 = 1716;
pub const DEFAULT_LISTEN_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(
    Ipv4Addr::UNSPECIFIED,
    DEFAULT_LISTEN_PORT,
));
pub const BROADCAST_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, DEFAULT_LISTEN_PORT));

/// Monotonically increasing counter — each accepted/initiated connection gets
/// a unique ID so that `Disconnected` events can be matched to the exact
/// connection that generated them, preventing stale disconnects from
/// incorrectly wiping a newer live connection out of `writer_map`.
static CONN_COUNTER: AtomicU64 = AtomicU64::new(0);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
static HANDSHAKE_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(32)));

fn spawn_handshake(future: impl std::future::Future<Output = ()> + Send + 'static) {
    let Ok(permit) = HANDSHAKE_SLOTS.clone().try_acquire_owned() else {
        warn!("handshake capacity reached; dropping connection");
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        if tokio::time::timeout(HANDSHAKE_TIMEOUT, future)
            .await
            .is_err()
        {
            warn!("identity/TLS handshake timed out");
        }
    });
}

async fn read_identity(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
) -> anyhow::Result<Identity> {
    // Read exactly through the newline so TLS bytes remain on the socket.
    let mut raw = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        anyhow::ensure!(raw.len() < 65536, "identity line too long");
        if byte == b'\n' {
            break;
        }
        raw.push(byte);
    }
    let packet = ProtocolPacket::from_raw(&raw)?;
    anyhow::ensure!(
        matches!(packet.packet_type, PacketType::Identity),
        "expected identity packet"
    );
    let identity: Identity = serde_json::from_value(packet.body)?;
    DeviceId(identity.device_id.clone()).validate()?;
    Ok(identity)
}

pub(crate) fn verify_payload_peer(
    device: &Device,
    certificates: Option<&[rustls::pki_types::CertificateDer<'_>]>,
) -> anyhow::Result<()> {
    let expected = device.payload_certificate()?;
    let actual = certificates
        .and_then(|certs| certs.first())
        .ok_or_else(|| anyhow::anyhow!("payload peer supplied no certificate"))?;
    anyhow::ensure!(
        actual.as_ref() == expected,
        "payload peer certificate mismatch"
    );
    Ok(())
}

#[derive(Debug)]
pub enum TransportEvent {
    IncomingPacket {
        addr: SocketAddr,
        id: DeviceId,
        raw: String,
        conn_id: u64,
    },
    NewConnection {
        addr: SocketAddr,
        id: DeviceId,
        name: String,
        certificate: Vec<u8>,
        accepted: oneshot::Sender<bool>,
        write_tx: mpsc::UnboundedSender<ProtocolPacket>,
        /// Unique ID for this connection instance.
        conn_id: u64,
    },
    /// Emitted when the reader loop ends (peer closed / broken pipe).
    /// `conn_id` must match the stored value for this device before core
    /// removes the writer_map entry; a mismatch means a newer connection
    /// has already replaced this one.
    Disconnected { id: DeviceId, conn_id: u64 },
}

/// Build a raw `kdeconnect.identity` packet ready to write to a socket.
/// Used for both pre-TLS and post-TLS identity exchange on both transports.
fn identity_raw(identity: &Identity) -> Vec<u8> {
    ProtocolPacket::new(
        PacketType::Identity,
        serde_json::to_value(identity).unwrap(),
    )
    .as_raw()
    .expect("Failed to serialize identity packet")
}

/// Completes the identity/TLS handshake once a TCP stream to the peer exists
/// (TCP: just accepted; UDP: just connected after discovery). Shared by both
/// transports since the sequence — send our identity pre-TLS, accept TLS as
/// server, send our filtered identity post-TLS, then hand off to
/// `handle_connection` — is otherwise identical between them.
///
/// Callers should spawn this rather than awaiting it inline, so a slow or
/// stalled peer can't block the accept/recv loop from handling others.
async fn complete_handshake(
    mut stream: TcpStream,
    our_identity: Arc<Identity>,
    server_config: Arc<rustls::ServerConfig>,
    peer: SocketAddr,
    id: DeviceId,
    name: String,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
) {
    if let Err(e) = stream.write_all(&identity_raw(&our_identity)).await {
        warn!(peer = ?peer, "[handshake] failed to send pre-TLS identity: {}", e);
        return;
    }
    let _ = stream.flush().await;

    let mut tls_stream = match TlsAcceptor::from(server_config).accept(stream).await {
        Ok(s) => s,
        Err(e) => {
            warn!(peer = ?peer, "[handshake] TLS accept failed: {}", e);
            return;
        }
    };
    let Some(certificate) = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| cert.as_ref().to_vec())
    else {
        warn!("control peer supplied no certificate");
        return;
    };
    info!(peer = ?peer, device_id = ?id, "[handshake] TLS established");

    // Filter capabilities based on per-device disabled plugins so the phone
    // immediately knows which packet types to stop sending.
    let filtered = filtered_identity_for_device(&id.0).await;
    if let Err(e) = tls_stream.write_all(&identity_raw(&filtered)).await {
        warn!(peer = ?peer, "[handshake] failed to send post-TLS identity: {}", e);
        return;
    }
    let _ = tls_stream.flush().await;

    let (reader, writer) = split(tls_stream);
    let (write_tx, write_rx) = mpsc::unbounded_channel::<ProtocolPacket>();
    let write_rx = Arc::new(Mutex::new(write_rx));
    let conn_id = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);

    let (accepted, acceptance) = oneshot::channel();
    if event_tx
        .send(TransportEvent::NewConnection {
            addr: peer,
            id: id.clone(),
            name,
            certificate,
            accepted,
            write_tx,
            conn_id,
        })
        .is_err()
    {
        return;
    }
    // Do not start reading packets until core has verified the certificate and
    // registered this connection. This also prevents pre-registration events.
    if acceptance.await != Ok(true) {
        return;
    }
    tokio::spawn(handle_connection(
        event_tx, reader, writer, write_rx, peer, id, conn_id,
    ));
}

/// Enable TCP keepalive so the OS detects a dead connection within ~60s
/// (30s idle + 3 × 10s probes) rather than waiting indefinitely.
fn apply_keepalive(stream: &TcpStream) {
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    let sock_ref = socket2::SockRef::from(stream);
    if let Err(e) = sock_ref.set_tcp_keepalive(&keepalive) {
        warn!("Failed to set TCP keepalive: {}", e);
    }
}

pub struct TcpTransport {
    listen_addr: SocketAddr,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    identity: Arc<Identity>,
    server_config: Arc<rustls::ServerConfig>,
}

impl TcpTransport {
    pub fn new(event_tx: &mpsc::UnboundedSender<TransportEvent>) -> Self {
        let config = GLOBAL_CONFIG.get().unwrap();
        let listen_addr = config.listen_addr;
        let event_tx = event_tx.clone();
        let identity = Arc::new(config.identity.clone());
        let server_config = config.key_store.server_config.clone();

        Self {
            listen_addr,
            event_tx,
            identity,
            server_config,
        }
    }

    pub async fn listen(&self) -> anyhow::Result<()> {
        use socket2::{Domain, Protocol, Socket, Type};

        // SO_REUSEADDR prevents "address already in use" when the service
        // restarts before the OS has released the port from the previous instance.
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&self.listen_addr.into())?;
        socket.listen(128)?;
        let listener = TcpListener::from_std(std::net::TcpListener::from(socket))?;
        info!("TCP listener bound to {}", self.listen_addr);

        self.accept_connections(listener).await
    }

    async fn accept_connections(&self, listener: TcpListener) -> anyhow::Result<()> {
        loop {
            match listener.accept().await {
                Ok((mut stream, peer)) => {
                    info!(peer = ?peer, "[tcp] new connection");
                    apply_keepalive(&stream);

                    let identity = self.identity.clone();
                    let server_config = self.server_config.clone();
                    let event_tx = self.event_tx.clone();
                    spawn_handshake(async move {
                        let peer_identity = match read_identity(&mut stream).await {
                            Ok(identity) => identity,
                            Err(e) => {
                                warn!(?peer, "invalid pre-TLS identity: {}", e);
                                return;
                            }
                        };
                        if identity.device_id == peer_identity.device_id {
                            return;
                        }
                        complete_handshake(
                            stream,
                            identity,
                            server_config,
                            peer,
                            DeviceId(peer_identity.device_id),
                            peer_identity.device_name,
                            event_tx,
                        )
                        .await;
                    });
                }
                Err(e) => {
                    warn!("[tcp] accept error: {}", e);
                }
            }
        }
    }
}

pub struct UdpTransport {
    socket: Arc<UdpSocket>,
    discovery_interval: Duration,
    #[allow(dead_code)]
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    identity: Arc<Identity>,
    server_config: Arc<rustls::ServerConfig>,
}

impl UdpTransport {
    pub async fn new(event_tx: &mpsc::UnboundedSender<TransportEvent>) -> Self {
        let config = GLOBAL_CONFIG.get().unwrap();

        let socket = {
            let mut attempts = 0u32;
            loop {
                match UdpSocket::bind(config.listen_addr).await {
                    Ok(s) => break s,
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 10 {
                            // Port still held — another instance is almost certainly
                            // running. Exit cleanly so the caller (the real owner) is
                            // not disrupted, rather than panicking into the journal.
                            tracing::error!(
                                "UDP port {} still in use after {} attempts — \
                                 another instance may be running, exiting: {}",
                                config.listen_addr.port(),
                                attempts,
                                e
                            );
                            std::process::exit(1);
                        }
                        tracing::warn!(
                            "UDP bind failed (attempt {}), retrying in 1s: {}",
                            attempts,
                            e
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        };
        let _ = socket.set_broadcast(true);
        let socket = Arc::new(socket);

        let discovery_interval = config.discovery_interval;
        let event_tx = event_tx.clone();
        let identity = Arc::new(config.identity.clone());
        let server_config = config.key_store.server_config.clone();

        Self {
            socket,
            discovery_interval,
            event_tx,
            identity,
            server_config,
        }
    }

    pub async fn send_identity(&self) -> anyhow::Result<()> {
        if std::env::var("KDECONNECT_DISABLE_UDP_BROADCAST").is_ok() {
            warn!("UDP broadcast disabled by environment variable");
            return Ok(());
        }

        debug!("Broadcasting UDP identity packet");
        let interval = self.discovery_interval;
        let udp_socket = self.socket.clone();

        let packet = identity_raw(&self.identity);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(interval.as_secs()));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                match udp_socket.send_to(packet.as_slice(), BROADCAST_ADDR).await {
                    Ok(size) => {
                        debug!(addr = ?BROADCAST_ADDR, packet.size = size, "Sending udp broadcast")
                    }
                    Err(e) => warn!(addr = ?BROADCAST_ADDR, "Failed to send UDP packet: {}", e),
                }
                interval.tick().await;
            }
        });

        Ok(())
    }

    pub async fn listen(&self) -> anyhow::Result<()> {
        loop {
            let mut buf = vec![0u8; 8192];

            match self.socket.recv_from(&mut buf).await {
                Ok((len, mut peer)) => {
                    let raw = &buf[..len];
                    let packet = match ProtocolPacket::from_raw(raw) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("[udp] Failed to parse UDP packet: {}", e);
                            continue;
                        }
                    };

                    let Ok(peer_identity) = serde_json::from_value::<Identity>(packet.body) else {
                        continue;
                    };

                    if self.identity.device_id == peer_identity.device_id {
                        warn!("[udp] skipping the same device");
                        continue;
                    }

                    let id = DeviceId(peer_identity.device_id.clone());
                    if id.validate().is_err() {
                        continue;
                    }
                    let name = peer_identity.device_name.clone();

                    if let Some(new_port) = peer_identity.tcp_port {
                        peer.set_port(new_port);
                        info!(peer = ?peer, device_id = ?id, device_name = name, "Device supports TCP");
                    }

                    // Connecting and the handshake are spawned so a slow or
                    // unreachable peer can't block the UDP recv loop.
                    let identity = self.identity.clone();
                    let server_config = self.server_config.clone();
                    let event_tx = self.event_tx.clone();
                    spawn_handshake(async move {
                        let stream = match TcpStream::connect(peer).await {
                            Ok(s) => {
                                apply_keepalive(&s);
                                s
                            }
                            Err(e) => {
                                warn!(peer = ?peer, "[udp] TCP connect failed: {}", e);
                                return;
                            }
                        };
                        complete_handshake(
                            stream,
                            identity,
                            server_config,
                            peer,
                            id,
                            name,
                            event_tx,
                        )
                        .await;
                    });
                }
                Err(e) => {
                    warn!("[udp] recv_from error: {}", e);
                }
            }
        }
    }
}

async fn handle_connection<R, W>(
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    reader: R,
    mut writer: W,
    write_rx: Arc<Mutex<mpsc::UnboundedReceiver<ProtocolPacket>>>,
    peer: SocketAddr,
    id: DeviceId,
    conn_id: u64,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut buffer = String::new();

    let read = async {
        loop {
            match reader.read_line(&mut buffer).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = buffer.trim();
                    if !trimmed.is_empty()
                        && event_tx
                            .send(TransportEvent::IncomingPacket {
                                addr: peer,
                                id: id.clone(),
                                raw: trimmed.to_string(),
                                conn_id,
                            })
                            .is_err()
                    {
                        break;
                    }
                    buffer.clear();
                }
                Err(e) => {
                    warn!(?peer, "control read failed: {}", e);
                    break;
                }
            }
        }
    };
    let write = async {
        while let Some(msg) = write_rx.lock().await.recv().await {
            let Ok(raw) = msg.as_raw() else {
                break;
            };
            if writer.write_all(&raw).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    };
    // Dropping a superseded writer also ends its reader; no stale connection
    // remains able to deliver packets after a replacement or local disconnect.
    tokio::select! { _ = read => {}, _ = write => {} }
    let _ = event_tx.send(TransportEvent::Disconnected { id, conn_id });
}

/// Build a filtered identity for a specific device, removing capabilities
/// for plugins that have been disabled for that device. Called at handshake
/// time so the phone receives accurate capabilities on every connection.
async fn filtered_identity_for_device(device_id: &str) -> Identity {
    let base = &GLOBAL_CONFIG.get().unwrap().identity;
    let disabled = plugin_config::load_disabled_plugins(device_id).await;

    if disabled.is_empty() {
        return base.clone();
    }

    // Map plugin IDs to the capability strings they own.
    // (incoming_caps, outgoing_caps)
    let cap_map: &[(&str, &[&str], &[&str])] = &[
        ("battery",             &["kdeconnect.battery"],                                                    &["kdeconnect.battery.request"]),
        ("clipboard",           &["kdeconnect.clipboard", "kdeconnect.clipboard.connect"],                  &["kdeconnect.clipboard"]),
        ("connectivity_report", &["kdeconnect.connectivity_report"],                                        &[]),
        ("contacts",            &["kdeconnect.contacts.response_uids_timestamps",
                                   "kdeconnect.contacts.response_vcards"],                                  &["kdeconnect.contacts.request_all_uids_timestamps",
                                                                                                              "kdeconnect.contacts.request_vcards_by_uid"]),
        ("findmyphone",         &[],                                                                        &["kdeconnect.findmyphone.request"]),
        ("mpris",               &["kdeconnect.mpris", "kdeconnect.mpris.request"],                          &["kdeconnect.mpris", "kdeconnect.mpris.request"]),
        ("notification",        &["kdeconnect.notification"],                                               &["kdeconnect.notification.request"]),
        ("ping",                &["kdeconnect.ping"],                                                       &["kdeconnect.ping"]),
        ("runcommand",          &["kdeconnect.runcommand.request"],                                         &["kdeconnect.runcommand"]),
        ("sftp",                &["kdeconnect.sftp"],                                                       &["kdeconnect.sftp.request"]),
        ("share",               &["kdeconnect.share.request"],                                              &["kdeconnect.share.request", "kdeconnect.share.request.update"]),
        ("sms",                 &["kdeconnect.sms.messages", "kdeconnect.sms.attachment_file"],             &["kdeconnect.sms.request",
                                                                                                              "kdeconnect.sms.request_conversations",
                                                                                                              "kdeconnect.sms.request_conversation",
                                                                                                              "kdeconnect.sms.request_attachment"]),
        ("telephony",           &["kdeconnect.telephony"],                                                  &["kdeconnect.telephony.request_mute"]),                                                                                                      
    ];

    let mut remove_inc: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut remove_out: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (plugin_id, inc, out) in cap_map {
        if disabled.contains(*plugin_id) {
            remove_inc.extend(inc.iter().copied());
            remove_out.extend(out.iter().copied());
        }
    }

    Identity {
        device_id: base.device_id.clone(),
        device_name: base.device_name.clone(),
        device_type: base.device_type,
        protocol_version: base.protocol_version,
        tcp_port: base.tcp_port,
        incoming_capabilities: base
            .incoming_capabilities
            .iter()
            .filter(|c| !remove_inc.contains(c.as_str()))
            .cloned()
            .collect(),
        outgoing_capabilities: base
            .outgoing_capabilities
            .iter()
            .filter(|c| !remove_out.contains(c.as_str()))
            .cloned()
            .collect(),
    }
}

pub(crate) async fn prepare_listener_for_payload() -> Result<TcpListener, String> {
    for port in 1739..1769 {
        if let Ok(listener) =
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).await
        {
            return Ok(listener);
        }
    }
    Err("no free port for payload, failed.".to_string())
}

pub(crate) async fn receive_payload(
    device: &Device,
    addr: &SocketAddr,
    save_path: &mut tokio::fs::File,
) -> anyhow::Result<()> {
    let config = GLOBAL_CONFIG.get().unwrap();
    let client_config = config.key_store.client_config.clone();
    debug!("client config created.");

    let stream = TcpStream::connect(&addr).await?;

    let domain = ServerName::try_from(device.device_id.0.as_str())?.to_owned();

    let mut stream = tokio_rustls::TlsConnector::from(client_config)
        .connect(domain, stream)
        .await?;

    debug!("connected");

    verify_payload_peer(device, stream.get_ref().1.peer_certificates())?;
    tokio::io::copy(&mut stream, save_path).await?;
    save_path.flush().await?;
    stream.flush().await?;
    stream.shutdown().await?;

    info!("successfully received payload");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{NoCertificateVerification, certificate_generator};
    use crate::device::PairState;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

    fn tls_configs() -> (
        Arc<rustls::ServerConfig>,
        Arc<rustls::ClientConfig>,
        Vec<u8>,
    ) {
        let key = rcgen::KeyPair::generate().unwrap().serialize_pem();
        let cert = certificate_generator(&key, "phone").unwrap();
        let der = cert.der().to_vec();
        let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = Arc::new(NoCertificateVerification::new((*provider).clone()));
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(verifier.clone())
            .with_single_cert(vec![CertificateDer::from(der.clone())], key.clone_key())
            .unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(vec![CertificateDer::from(der.clone())], key)
            .unwrap();
        (Arc::new(server), Arc::new(client), der)
    }

    #[tokio::test]
    async fn payload_pin_is_checked_in_both_tls_directions() {
        let (server_config, _, server_cert) = tls_configs();
        let (_, client_config, client_cert) = tls_configs();
        let (server_socket, client_socket) = tokio::io::duplex(16384);
        let acceptor = TlsAcceptor::from(server_config);
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let (server, client) = tokio::join!(
            acceptor.accept(server_socket),
            connector.connect(ServerName::try_from("phone").unwrap(), client_socket),
        );
        let server = server.unwrap();
        let client = client.unwrap();
        let expected_client = Device {
            pair_state: PairState::Paired,
            paired_certificate: Some(client_cert),
            ..Device::default()
        };
        let expected_server = Device {
            pair_state: PairState::Paired,
            paired_certificate: Some(server_cert),
            ..Device::default()
        };
        assert!(
            verify_payload_peer(&expected_client, server.get_ref().1.peer_certificates()).is_ok()
        );
        assert!(
            verify_payload_peer(&expected_server, client.get_ref().1.peer_certificates()).is_ok()
        );
        assert!(
            verify_payload_peer(&expected_server, server.get_ref().1.peer_certificates()).is_err()
        );
        assert!(
            verify_payload_peer(&expected_client, client.get_ref().1.peer_certificates()).is_err()
        );
        assert!(verify_payload_peer(&expected_client, None).is_err());
        assert!(
            verify_payload_peer(&Device::default(), server.get_ref().1.peer_certificates())
                .is_err()
        );
    }

    fn identity() -> Identity {
        Identity {
            device_id: "desktop".into(),
            device_name: "desktop".into(),
            device_type: crate::protocol::DeviceType::Desktop,
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            tcp_port: Some(1716),
            incoming_capabilities: vec![],
            outgoing_capabilities: vec![],
        }
    }

    #[tokio::test]
    async fn identity_reader_does_not_consume_tls_bytes() {
        let mut input = identity_raw(&identity());
        input.extend_from_slice(b"TLS bytes");
        let mut input = input.as_slice();
        assert_eq!(
            read_identity(&mut input).await.unwrap().device_id,
            "desktop"
        );
        assert_eq!(input, b"TLS bytes");
        let mut invalid = identity();
        invalid.device_id = "../escape".into();
        assert!(
            read_identity(&mut identity_raw(&invalid).as_slice())
                .await
                .is_err()
        );
        assert!(
            read_identity(&mut b"{not json}\n".as_slice())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn identity_length_is_bounded() {
        let bytes = vec![b'x'; 65537];
        let error = read_identity(&mut bytes.as_slice()).await.unwrap_err();
        assert!(error.to_string().contains("too long"));
    }

    #[tokio::test]
    async fn silent_client_does_not_block_next_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let transport = TcpTransport {
            listen_addr: addr,
            event_tx: tx,
            identity: Arc::new(identity()),
            server_config: tls_configs().0,
        };
        let listener_task =
            tokio::spawn(async move { transport.accept_connections(listener).await });
        let mut silent = TcpStream::connect(addr).await.unwrap();
        let mut second = TcpStream::connect(addr).await.unwrap();
        second.write_all(b"{invalid}\n").await.unwrap();
        let mut byte = [0];
        // The second client is accepted and rejected promptly while the first
        // is still withholding its identity; the old listener stalled here.
        let read = tokio::time::timeout(Duration::from_secs(2), second.read(&mut byte)).await;
        assert_eq!(read.unwrap().unwrap(), 0);
        let expired = tokio::time::timeout(
            HANDSHAKE_TIMEOUT + Duration::from_secs(2),
            silent.read(&mut byte),
        )
        .await;
        listener_task.abort();
        let _ = listener_task.await;
        assert_eq!(expired.unwrap().unwrap(), 0);
    }

    #[tokio::test]
    async fn closing_writer_ends_reader_and_reports_connection_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let (_peer, stream) = tokio::io::duplex(1024);
        let (reader, writer) = split(stream);
        let task = tokio::spawn(handle_connection(
            tx,
            reader,
            writer,
            Arc::new(Mutex::new(writer_rx)),
            "127.0.0.1:1716".parse().unwrap(),
            DeviceId("phone".into()),
            42,
        ));
        drop(writer_tx);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(TransportEvent::Disconnected { conn_id: 42, .. })
        ));
    }
}
