use crate::{
    host::{HostController, VideoSettings},
    preview::PreviewCache,
    protocol::{self, RemoteError, SessionTicket, is_local},
    transport::{self, Lifetime},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use mdns_sd::{IfKind, IfPredicate, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SERVICE: &str = "_localnet._tcp.local.";
const MAX_DEVICES: usize = 64;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

pub fn computer_name() -> String {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| {
            if cfg!(target_os = "macos") {
                "Mac".into()
            } else {
                "Windows PC".into()
            }
        });
    let name: String = name
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(60)
        .collect();
    if name.is_empty() {
        "Computer".into()
    } else {
        name
    }
}

#[derive(Clone)]
pub struct Device {
    pub name: String,
    pub address: SocketAddr,
    pub certificate: Vec<u8>,
    pub version: u16,
}

impl Device {
    pub fn compatible(&self) -> bool {
        self.version == protocol::VERSION
    }
}

#[derive(Default)]
pub struct DiscoveryState {
    pub devices: HashMap<String, Device>,
    pub status: String,
}

pub struct Discovery {
    pub state: Arc<Mutex<DiscoveryState>>,
    pub controller: Arc<HostController>,
    life: Arc<Lifetime>,
    daemon: ServiceDaemon,
    fullname: String,
    worker: Option<JoinHandle<()>>,
}

fn txt(certificate: &[u8], name: &str) -> HashMap<String, String> {
    let mut properties = HashMap::from([
        ("v".into(), protocol::VERSION.to_string()),
        ("name".into(), name.into()),
    ]);
    let encoded = STANDARD.encode(certificate);
    let chunks: Vec<_> = encoded.as_bytes().chunks(180).collect();
    properties.insert("cert_count".into(), chunks.len().to_string());
    for (index, chunk) in chunks.into_iter().enumerate() {
        properties.insert(
            format!("cert{index}"),
            String::from_utf8(chunk.to_vec()).unwrap(),
        );
    }
    properties
}

fn device(service: &ResolvedService) -> Result<Device> {
    let property = |key: &str| -> Result<&str> {
        Ok(std::str::from_utf8(
            service
                .get_property_val(key)
                .flatten()
                .ok_or_else(|| anyhow::anyhow!("Missing discovery property"))?,
        )?)
    };
    let version = property("v")?.parse()?;
    let name = property("name")?;
    ensure!(
        !name.is_empty()
            && name.len() <= 240
            && name.chars().count() <= 60
            && !name.chars().any(|c| c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
        "Invalid name"
    );
    let count: usize = property("cert_count")?.parse()?;
    ensure!((1..=6).contains(&count), "Invalid certificate chunks");
    ensure!(
        service.get_properties().iter().count() == count + 3,
        "Unexpected discovery properties"
    );
    let mut encoded = String::new();
    for index in 0..count {
        let chunk = property(&format!("cert{index}"))?;
        ensure!(
            !chunk.is_empty() && chunk.len() <= 180,
            "Invalid certificate chunk"
        );
        encoded.push_str(chunk);
    }
    let certificate = STANDARD.decode(encoded)?;
    ensure!(
        !certificate.is_empty() && certificate.len() <= 800 && service.get_port() > 0,
        "Invalid discovery identity"
    );
    let mut addresses: Vec<_> = service
        .get_addresses()
        .iter()
        .map(|ip| ip.to_ip_addr())
        .filter(|ip| is_local(*ip) && !ip.is_loopback())
        .take(8)
        .collect();
    addresses.sort_by_key(|ip| (!ip.is_ipv4(), ip.to_string()));
    let ip = *addresses
        .first()
        .ok_or_else(|| anyhow::anyhow!("No private address"))?;
    Ok(Device {
        name: name.into(),
        address: SocketAddr::new(ip, service.get_port()),
        certificate,
        version,
    })
}

impl Discovery {
    pub fn start(ip: IpAddr, settings: VideoSettings) -> Result<Self> {
        ensure!(
            is_local(ip) && !ip.is_loopback(),
            "Discovery needs a private LAN interface"
        );
        Self::start_inner(ip, settings, true)
    }

    fn start_inner(ip: IpAddr, settings: VideoSettings, advertise: bool) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::new(ip, if advertise { 45778 } else { 0 }))
            .context("Discovery port 45778 (close any other Localnet instance)")?;
        listener.set_nonblocking(true)?;
        let (config, certificate) = identity()?;
        let daemon = ServiceDaemon::new()?;
        let setup = (|| -> Result<_> {
            let private = IfKind::Predicate(IfPredicate::new(move |i| i.ip() == ip));
            daemon.disable_interface(IfKind::All)?;
            daemon.enable_interface(private.clone())?;
            let id = protocol::random_token()?;
            let mut info = ServiceInfo::new(
                SERVICE,
                &format!("localnet-{}", &id[..16]),
                &format!("localnet-{}.local.", &id[..16]),
                ip,
                listener.local_addr()?.port(),
                txt(&certificate, &computer_name()),
            )?;
            info.set_interfaces(vec![private]);
            let fullname = info.get_fullname().to_owned();
            if advertise {
                daemon.register(info)?;
            }
            Ok((fullname, daemon.browse(SERVICE)?))
        })();
        let (fullname, events) = match setup {
            Ok(value) => value,
            Err(e) => {
                let _ = daemon.shutdown();
                return Err(e);
            }
        };
        let life = Arc::new(Lifetime::default());
        let controller = Arc::new(HostController::start(ip, settings));
        let previews = Arc::new(PreviewCache::default());
        let local_ips: Vec<_> = if_addrs::get_if_addrs()?
            .into_iter()
            .map(|interface| interface.ip())
            .collect();
        let state = Arc::new(Mutex::new(DiscoveryState {
            status: format!("Local access enabled on {ip}. Click a computer to connect."),
            ..Default::default()
        }));
        let worker = {
            let (life, state, own, controller) = (
                life.clone(),
                state.clone(),
                fullname.clone(),
                controller.clone(),
            );
            thread::spawn(move || {
                let mut workers: Vec<(JoinHandle<()>, Arc<Lifetime>)> = Vec::new();
                while !life.stopped() {
                    for _ in 0..128 {
                        match events.try_recv() {
                            Ok(ServiceEvent::ServiceResolved(service))
                                if service.get_fullname() != own =>
                            {
                                if let Ok(device) = device(&service) {
                                    if (advertise && local_ips.contains(&device.address.ip()))
                                        || device.certificate == certificate
                                    {
                                        continue;
                                    }
                                    let mut state = state.lock().unwrap();
                                    if state.devices.contains_key(service.get_fullname())
                                        || state.devices.len() < MAX_DEVICES
                                    {
                                        state.devices.insert(service.get_fullname().into(), device);
                                    }
                                }
                            }
                            Ok(ServiceEvent::ServiceRemoved(_, name)) => {
                                state.lock().unwrap().devices.remove(&name);
                            }
                            Ok(_) => {}
                            Err(mdns_sd::TryRecvError::Empty) => break,
                            Err(mdns_sd::TryRecvError::Disconnected) => {
                                state.lock().unwrap().devices.clear();
                                state.lock().unwrap().status =
                                    "Discovery stopped. Disable and enable local access to retry."
                                        .into();
                                life.stop();
                                break;
                            }
                        }
                    }
                    let mut index = 0;
                    while index < workers.len() {
                        if workers[index].0.is_finished() {
                            let _ = workers.swap_remove(index).0.join();
                        } else {
                            index += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((socket, peer)) if is_local(peer.ip()) && workers.len() < 4 => {
                            let (config, state, controller, previews) = (
                                config.clone(),
                                state.clone(),
                                controller.clone(),
                                previews.clone(),
                            );
                            let cancel = Arc::new(Lifetime::default());
                            if cancel.track(&socket).is_err() {
                                continue;
                            }
                            let worker_cancel = cancel.clone();
                            workers.push((
                                thread::spawn(move || {
                                    let result = incoming(
                                        socket,
                                        peer.ip(),
                                        config,
                                        &worker_cancel,
                                        &previews,
                                        |peer, life| controller.request(peer, life),
                                    );
                                    if let Err(e) = result {
                                        state.lock().unwrap().status =
                                            format!("Connection from {} ended: {e:#}", peer.ip());
                                    }
                                    worker_cancel.stop();
                                }),
                                cancel,
                            ));
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => {
                            state.lock().unwrap().status = format!("Discovery listener ended: {e}");
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                for (_, cancel) in &workers {
                    cancel.stop();
                }
                for (worker, _) in workers {
                    let _ = worker.join();
                }
            })
        };
        Ok(Self {
            state,
            controller,
            life,
            daemon,
            fullname,
            worker: Some(worker),
        })
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.life.stop();
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.stop_browse(SERVICE);
        let _ = self.daemon.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn identity() -> Result<(Arc<ServerConfig>, Vec<u8>)> {
    let identity = rcgen::generate_simple_self_signed(vec![protocol::SERVER_NAME.into()])?;
    let config = Arc::new(
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_single_cert(
                vec![identity.cert.der().clone()],
                PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
            )?,
    );
    Ok((config, identity.cert.der().to_vec()))
}

#[derive(Serialize, Deserialize)]
struct Request {
    version: u16,
    name: String,
    #[serde(default)]
    operation: Option<Operation>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum Operation {
    Connect,
    Preview,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Starting,
    Ready(SessionTicket),
    Error(RemoteError),
    Preview,
}

type LaunchResult = std::result::Result<SessionTicket, RemoteError>;

fn incoming(
    socket: TcpStream,
    peer: IpAddr,
    config: Arc<ServerConfig>,
    life: &Arc<Lifetime>,
    previews: &PreviewCache,
    launch: impl FnOnce(
        IpAddr,
        Arc<Lifetime>,
    ) -> std::result::Result<Receiver<LaunchResult>, RemoteError>,
) -> Result<()> {
    let mut stream = transport::accept(socket, config).context("Discovery TLS handshake")?;
    stream.sock.renew();
    let request: Request = protocol::read_json(&mut stream).context("Connection request")?;
    let result = (|| -> Result<()> {
        ensure!(
            request.version == protocol::VERSION,
            "Update Localnet on both computers (connection protocol mismatch)"
        );
        ensure!(
            is_local(peer)
                && !request.name.is_empty()
                && request.name.len() <= 240
                && !request.name.chars().any(char::is_control),
            "Invalid local connection request"
        );
        Ok(())
    })();
    if let Err(e) = result {
        protocol::write_json(
            &mut stream,
            &Reply::Error(RemoteError::new("incompatible_request", e.to_string())),
        )?;
        return Ok(());
    }
    match request.operation {
        Some(Operation::Preview) => {
            match previews.get() {
                Ok(bytes) => {
                    protocol::write_json(&mut stream, &Reply::Preview)?;
                    protocol::write_packet(&mut stream, &bytes, protocol::MAX_PREVIEW)?;
                }
                Err(error) => protocol::write_json(
                    &mut stream,
                    &Reply::Error(RemoteError::new("preview_unavailable", error)),
                )?,
            }
            return Ok(());
        }
        Some(Operation::Connect) => {}
        None => {
            protocol::write_json(
                &mut stream,
                &Reply::Error(RemoteError::new(
                    "missing_operation",
                    "Update Localnet on both computers",
                )),
            )?;
            return Ok(());
        }
    }
    let receiver = match launch(peer, life.clone()) {
        Ok(receiver) => receiver,
        Err(e) => {
            protocol::write_json(&mut stream, &Reply::Error(e))?;
            return Ok(());
        }
    };
    let expires = Instant::now() + STARTUP_TIMEOUT;
    loop {
        stream.sock.renew();
        let reply = match receiver.try_recv() {
            Ok(Ok(ticket)) => Reply::Ready(ticket),
            Ok(Err(e)) => Reply::Error(e),
            Err(TryRecvError::Empty) if Instant::now() < expires && !life.stopped() => {
                Reply::Starting
            }
            Err(TryRecvError::Empty) => Reply::Error(RemoteError::new(
                "startup_timeout",
                "Remote screen sharing did not start within 60 seconds. Check its Localnet status and OS permissions.",
            )),
            Err(TryRecvError::Disconnected) => Reply::Error(RemoteError::new(
                "startup_failed",
                "Remote sharing worker stopped during startup",
            )),
        };
        let terminal = !matches!(reply, Reply::Starting);
        protocol::write_json(&mut stream, &reply)?;
        if terminal {
            return Ok(());
        }
        ensure!(
            protocol::read_json::<bool>(&mut stream)?,
            "Connection canceled"
        );
    }
}

pub struct Outgoing {
    pub device: Device,
    pub state: Arc<Mutex<OutgoingState>>,
    life: Arc<Lifetime>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub struct OutgoingState {
    pub status: String,
    pub result: Option<Result<SessionTicket, String>>,
}

impl Outgoing {
    pub fn start(device: Device) -> Self {
        let state = Arc::new(Mutex::new(OutgoingState {
            status: "Connecting to local computer...".into(),
            ..Default::default()
        }));
        let life = Arc::new(Lifetime::default());
        let worker = {
            let (state, life, device) = (state.clone(), life.clone(), device.clone());
            thread::spawn(move || {
                let result = outgoing(&device, &state, &life)
                    .map_err(|e| format!("{} ({}): {e:#}", device.name, device.address.ip()));
                state.lock().unwrap().result = Some(result);
                life.stop();
            })
        };
        Self {
            device,
            state,
            life,
            worker: Some(worker),
        }
    }
}

impl Drop for Outgoing {
    fn drop(&mut self) {
        self.life.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn outgoing(
    device: &Device,
    state: &Arc<Mutex<OutgoingState>>,
    life: &Lifetime,
) -> Result<SessionTicket> {
    ensure!(
        device.compatible(),
        "Update Localnet on both computers to 0.3.0 (gallery preview protocol). Incompatible computer: {}.",
        device.name
    );
    let provisional = SessionTicket {
        version: protocol::VERSION,
        address: device.address,
        certificate: device.certificate.clone(),
        token: String::new(),
    };
    let mut stream = transport::connect(&provisional, transport::client_config(&provisional)?, life)
        .context("Connecting to discovery TLS on port 45778; check the remote app, firewall, and that both computers use the same build")?;
    stream.sock.renew();
    protocol::write_json(
        &mut stream,
        &Request {
            version: protocol::VERSION,
            name: computer_name(),
            operation: Some(Operation::Connect),
        },
    )?;
    let expires = Instant::now() + STARTUP_TIMEOUT;
    loop {
        ensure!(
            !life.stopped() && Instant::now() < expires,
            "Connection canceled or startup timed out"
        );
        stream.sock.renew();
        match protocol::read_json::<Reply>(&mut stream).context("Remote computer closed during automatic sharing startup; check its Localnet status and update both installations")? {
            Reply::Starting => {
                state.lock().unwrap().status = "Starting the other computer's screen sharing automatically...".into();
                thread::sleep(Duration::from_millis(100));
                protocol::write_json(&mut stream, &true)?;
            }
            Reply::Error(e) => anyhow::bail!("Remote {e}"),
            Reply::Ready(ticket) => {
                ticket.validate()?;
                ensure!(ticket.address.ip() == device.address.ip(), "Remote host address changed during startup");
                return Ok(ticket);
            }
            Reply::Preview => anyhow::bail!("Unexpected preview response to a connection request"),
        }
    }
}

pub fn fetch_preview(device: &Device, life: &Lifetime) -> Result<Vec<u8>> {
    ensure!(
        device.compatible(),
        "Update Localnet on both computers to 0.3.0"
    );
    let provisional = SessionTicket {
        version: protocol::VERSION,
        address: device.address,
        certificate: device.certificate.clone(),
        token: String::new(),
    };
    let mut stream =
        transport::connect(&provisional, transport::client_config(&provisional)?, life)?;
    stream.sock.renew();
    protocol::write_json(
        &mut stream,
        &Request {
            version: protocol::VERSION,
            name: computer_name(),
            operation: Some(Operation::Preview),
        },
    )?;
    match protocol::read_json::<Reply>(&mut stream)? {
        Reply::Preview => protocol::read_packet(&mut stream, protocol::MAX_PREVIEW),
        Reply::Error(error) => anyhow::bail!("{error}"),
        _ => anyhow::bail!("Unexpected response to screenshot preview request"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_thumbnail() -> Result<Vec<u8>> {
        let rgb = image::RgbImage::from_pixel(8, 4, image::Rgb([25, 45, 80]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes).encode_image(&rgb)?;
        Ok(bytes)
    }

    #[test]
    fn preview_request_never_launches_host_or_input() {
        let (config, certificate) = identity().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let device = Device {
            name: "Preview-only PC".into(),
            address: listener.local_addr().unwrap(),
            certificate,
            version: protocol::VERSION,
        };
        let server = thread::spawn(move || {
            let cache = PreviewCache::new(fake_thumbnail);
            for _ in 0..2 {
                incoming(listener.accept().unwrap().0, "127.0.0.1".parse().unwrap(), config.clone(), &Arc::new(Lifetime::default()), &cache,
                    |_,_| panic!("A screenshot must not launch screen control or require input permission")).unwrap();
            }
        });
        for _ in 0..2 {
            let life = Lifetime::default();
            let bytes = fetch_preview(&device, &life).unwrap();
            life.stop();
            assert_eq!(
                crate::viewer::FrameDecoder::new()
                    .unwrap()
                    .decode_bounded(&bytes, 480, 270)
                    .unwrap()
                    .size,
                [8, 4]
            );
        }
        server.join().unwrap();
    }

    #[test]
    #[ignore = "Captures one local thumbnail over loopback without creating a viewer or input device"]
    fn real_screenshot_preview_without_control_session() {
        let (config, certificate) = identity().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let device = Device {
            name: "Screenshot PC".into(),
            address: listener.local_addr().unwrap(),
            certificate,
            version: protocol::VERSION,
        };
        let server = thread::spawn(move || {
            incoming(
                listener.accept().unwrap().0,
                "127.0.0.1".parse().unwrap(),
                config,
                &Arc::new(Lifetime::default()),
                &PreviewCache::default(),
                |_, _| panic!("Preview must never start a full session"),
            )
            .unwrap();
        });
        let life = Lifetime::default();
        let bytes = fetch_preview(&device, &life).unwrap();
        life.stop();
        let image = crate::viewer::FrameDecoder::new()
            .unwrap()
            .decode_bounded(&bytes, 480, 270)
            .unwrap();
        assert!(image.size[0] <= 480 && image.size[1] <= 270);
        server.join().unwrap();
    }

    #[test]
    fn discovery_handles_old_versions_and_rejects_malformed_records() {
        let (_, certificate) = identity().unwrap();
        let mut properties = txt(&certificate, "Office PC");
        assert!(!properties.contains_key("token") && !properties.contains_key("code"));
        properties.insert("v".into(), "1".into());
        let service = ServiceInfo::new(
            SERVICE,
            "test",
            "test.local.",
            "192.168.1.20",
            45778,
            properties.clone(),
        )
        .unwrap();
        let found = device(&service.as_resolved_service()).unwrap();
        assert_eq!(found.name, "Office PC");
        assert!(!found.compatible());
        let service = ServiceInfo::new(
            SERVICE,
            "test",
            "test.local.",
            "127.0.0.1",
            45778,
            properties.clone(),
        )
        .unwrap();
        assert!(device(&service.as_resolved_service()).is_err());
        properties.insert("cert_count".into(), "999".into());
        let service = ServiceInfo::new(
            SERVICE,
            "test",
            "test.local.",
            "192.168.1.20",
            45778,
            properties,
        )
        .unwrap();
        assert!(device(&service.as_resolved_service()).is_err());
    }

    #[test]
    fn one_click_returns_ticket_or_actionable_startup_error_without_prompts() {
        for error in [
            None,
            Some(RemoteError::new(
                "host_start_failed",
                "Enable Screen Recording and Accessibility on Mac",
            )),
            Some(RemoteError::new("busy", "Another viewer is connected")),
        ] {
            let (config, certificate) = identity().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let device = Device {
                name: "Test Mac".into(),
                address: listener.local_addr().unwrap(),
                certificate,
                version: protocol::VERSION,
            };
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let server = thread::spawn(move || {
                let socket = listener.accept().unwrap().0;
                let life = Arc::new(Lifetime::default());
                life.track(&socket).unwrap();
                incoming(
                    socket,
                    "127.0.0.1".parse().unwrap(),
                    config,
                    &life,
                    &PreviewCache::default(),
                    |_, _| Ok(receiver),
                )
                .unwrap();
            });
            let outgoing = Outgoing::start(device);
            let response = match &error {
                Some(e) => Err(e.clone()),
                None => Ok(SessionTicket {
                    version: protocol::VERSION,
                    address: "127.0.0.1:45777".parse().unwrap(),
                    certificate: vec![1],
                    token: "a".repeat(64),
                }),
            };
            // Deliberately exceed a normal packet deadline: startup polling must keep the connection alive.
            thread::sleep(Duration::from_millis(3200));
            assert!(sender.send(response).is_ok());
            let until = Instant::now() + Duration::from_secs(5);
            let result = loop {
                if let Some(result) = outgoing.state.lock().unwrap().result.take() {
                    break result;
                }
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(10));
            };
            match error {
                None => assert!(result.is_ok()),
                Some(e) => assert!(result.err().unwrap().contains(&e.message)),
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn old_device_reports_update_instead_of_attempting_old_pairing_protocol() {
        let (_, certificate) = identity().unwrap();
        let device = Device {
            name: "Old Mac".into(),
            address: "127.0.0.1:1".parse().unwrap(),
            certificate,
            version: 1,
        };
        let error = outgoing(
            &device,
            &Arc::new(Mutex::new(OutgoingState::default())),
            &Lifetime::default(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("Update Localnet"));
    }

    #[test]
    fn incompatible_request_gets_explicit_error_and_never_starts_host() {
        let (config, certificate) = identity().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let ticket = SessionTicket {
            version: protocol::VERSION,
            address: listener.local_addr().unwrap(),
            certificate,
            token: String::new(),
        };
        let server = thread::spawn(move || {
            incoming(
                listener.accept().unwrap().0,
                "127.0.0.1".parse().unwrap(),
                config,
                &Arc::new(Lifetime::default()),
                &PreviewCache::default(),
                |_, _| {
                    panic!("An incompatible version must not start capture");
                },
            )
            .unwrap();
        });
        let mut stream = transport::connect(
            &ticket,
            transport::client_config(&ticket).unwrap(),
            &Lifetime::default(),
        )
        .unwrap();
        protocol::write_json(
            &mut stream,
            &Request {
                version: 1,
                name: "Old PC".into(),
                operation: Some(Operation::Preview),
            },
        )
        .unwrap();
        match protocol::read_json::<Reply>(&mut stream).unwrap() {
            Reply::Error(error) => assert!(error.message.contains("Update Localnet")),
            _ => panic!("Expected a protocol error"),
        }
        server.join().unwrap();
    }

    #[test]
    #[ignore = "Full automatic startup and video over loopback on an unlocked desktop; no input injection"]
    fn one_click_stream_disconnect_reconnect_without_ui_or_codes() {
        let (config, certificate) = identity().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let device = Device {
            name: "Loopback PC".into(),
            address: listener.local_addr().unwrap(),
            certificate,
            version: protocol::VERSION,
        };
        let controller = Arc::new(HostController::start(
            "127.0.0.1".parse().unwrap(),
            VideoSettings {
                fps: 30,
                width: 640,
                quality: 70,
            },
        ));
        let host = controller.clone();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let socket = listener.accept().unwrap().0;
                let life = Arc::new(Lifetime::default());
                life.track(&socket).unwrap();
                incoming(
                    socket,
                    "127.0.0.1".parse().unwrap(),
                    config.clone(),
                    &life,
                    &PreviewCache::default(),
                    |peer, life| host.request(peer, life),
                )
                .unwrap();
                life.stop();
            }
        });
        for _ in 0..2 {
            let outgoing = Outgoing::start(device.clone());
            let until = Instant::now() + Duration::from_secs(15);
            let ticket = loop {
                if let Some(result) = outgoing.state.lock().unwrap().result.take() {
                    break result.unwrap();
                }
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(10));
            };
            drop(outgoing);
            let viewer = crate::viewer::Viewer::start(ticket, eframe::egui::Context::default());
            while viewer.state.lock().unwrap().frames < 3 {
                assert!(
                    viewer.connected(),
                    "{}",
                    viewer.state.lock().unwrap().status
                );
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(10));
            }
            viewer.stop();
            drop(viewer);
            while controller.state.lock().unwrap().sharing {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(10));
            }
        }
        server.join().unwrap();
    }

    #[test]
    #[ignore = "Uses local mDNS on a private LAN interface; no screen capture or control"]
    fn mdns_discovers_public_metadata_and_removes_it() {
        let ip = if_addrs::get_if_addrs()
            .unwrap()
            .into_iter()
            .map(|i| i.ip())
            .find(|ip| ip.is_ipv4() && is_local(*ip) && !ip.is_loopback())
            .unwrap();
        let discovery = Discovery::start_inner(ip, VideoSettings::default(), false).unwrap();
        let (_, certificate) = identity().unwrap();
        let id = protocol::random_token().unwrap();
        let mut info = ServiceInfo::new(
            SERVICE,
            &format!("test-{}", &id[..12]),
            &format!("test-{}.local.", &id[..12]),
            ip,
            45999,
            txt(&certificate, "Localnet discovery test"),
        )
        .unwrap();
        info.set_requires_probe(false);
        let name = info.get_fullname().to_owned();
        discovery.daemon.register(info).unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        while !discovery.state.lock().unwrap().devices.contains_key(&name) {
            assert!(Instant::now() < until, "mDNS record was not discovered");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(discovery.state.lock().unwrap().devices[&name].compatible());
        discovery.daemon.unregister(&name).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while discovery.state.lock().unwrap().devices.contains_key(&name) {
            assert!(Instant::now() < until, "mDNS removal was not observed");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
