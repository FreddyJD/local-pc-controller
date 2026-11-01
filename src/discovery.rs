use crate::{
    protocol::{self, Pairing, is_local},
    transport::{self, Lifetime},
};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use mdns_sd::{IfKind, IfPredicate, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SERVICE: &str = "_localnet._tcp.local.";
const HUMAN_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_DEVICES: usize = 64;

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
}

pub struct Pending {
    pub name: String,
    pub peer: IpAddr,
    pub code: String,
    pub verified: bool,
    pub expires: Instant,
    pub decision: Option<Result<Pairing, String>>,
    canceled: bool,
    life: Arc<Lifetime>,
}

impl Pending {
    pub fn cancel(&mut self) {
        self.canceled = true;
        self.life.stop();
    }
    pub fn ready(&self) -> bool {
        self.verified
            && !self.canceled
            && !self.life.stopped()
            && Instant::now() < self.expires
            && self.decision.is_none()
    }
}

#[derive(Default)]
pub struct DiscoveryState {
    pub devices: HashMap<String, Device>,
    pub pending: Option<Arc<Mutex<Pending>>>,
    pub status: String,
}

pub struct Discovery {
    pub ip: IpAddr,
    pub state: Arc<Mutex<DiscoveryState>>,
    pub accepting: Arc<AtomicBool>,
    life: Arc<Lifetime>,
    daemon: ServiceDaemon,
    fullname: String,
    worker: Option<JoinHandle<()>>,
}

fn txt(certificate: &[u8], name: &str) -> HashMap<String, String> {
    let mut properties = HashMap::from([("v".into(), "1".into()), ("name".into(), name.into())]);
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
    ensure!(property("v")? == "1", "Unsupported discovery version");
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
    })
}

impl Discovery {
    pub fn start(ip: IpAddr) -> Result<Self> {
        ensure!(
            is_local(ip) && !ip.is_loopback(),
            "Discovery needs a private LAN interface"
        );
        Self::start_inner(ip, true)
    }

    fn start_inner(ip: IpAddr, advertise: bool) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::new(ip, if advertise { 45778 } else { 0 }))?;
        listener.set_nonblocking(true)?;
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
                txt(identity.cert.der(), &computer_name()),
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
        let accepting = Arc::new(AtomicBool::new(true));
        let state = Arc::new(Mutex::new(DiscoveryState {
            status: format!(
                "Looking for computers on {ip}. Incoming requests require your approval."
            ),
            ..Default::default()
        }));
        let worker = {
            let (life, state, own, accepting) = (
                life.clone(),
                state.clone(),
                fullname.clone(),
                accepting.clone(),
            );
            thread::spawn(move || {
                let mut workers: Vec<(JoinHandle<()>, Arc<Lifetime>)> = Vec::new();
                let mut last_attempt = Instant::now() - Duration::from_secs(10);
                while !life.stopped() {
                    for _ in 0..128 {
                        match events.try_recv() {
                            Ok(ServiceEvent::ServiceResolved(service))
                                if service.get_fullname() != own =>
                            {
                                if let Ok(device) = device(&service) {
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
                                let mut state = state.lock().unwrap();
                                state.devices.clear();
                                state.status =
                                    "Discovery stopped. Disable and enable discovery to retry."
                                        .into();
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
                        Ok((socket, peer))
                            if is_local(peer.ip())
                                && workers.len() < 2
                                && accepting.load(Ordering::Relaxed)
                                && last_attempt.elapsed() >= Duration::from_secs(5) =>
                        {
                            last_attempt = Instant::now();
                            let (config, state, accepting) =
                                (config.clone(), state.clone(), accepting.clone());
                            let worker_life = Arc::new(Lifetime::default());
                            if worker_life.track(&socket).is_err() {
                                continue;
                            }
                            let cancel = worker_life.clone();
                            workers.push((
                                thread::spawn(move || {
                                    let _ = incoming(
                                        socket,
                                        peer.ip(),
                                        config,
                                        &state,
                                        &cancel,
                                        &accepting,
                                    );
                                }),
                                worker_life,
                            ));
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => {
                            state.lock().unwrap().status = format!("Discovery listener ended: {e}");
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(20));
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
            ip,
            state,
            accepting,
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
        if let Some(pending) = self.state.lock().unwrap().pending.as_ref() {
            pending.lock().unwrap().cancel();
        }
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.stop_browse(SERVICE);
        let _ = self.daemon.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    version: u16,
    name: String,
}

fn confirmation(key: &[u8], binding: &[u8], role: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(b"localnet-pair-v1");
    mac.update(binding);
    mac.update(role);
    mac.finalize().into_bytes().to_vec()
}

fn verify(key: &[u8], binding: &[u8], role: &[u8], proof: &[u8]) -> Result<()> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(b"localnet-pair-v1");
    mac.update(binding);
    mac.update(role);
    mac.verify_slice(proof)
        .map_err(|_| anyhow::anyhow!("Pairing code or TLS channel did not match"))
}

fn password() -> Result<String> {
    // Rejection sampling avoids modulo bias in the short, single-attempt pairing code.
    loop {
        let mut raw = [0; 4];
        getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("Random source failed: {e}"))?;
        let value = u32::from_le_bytes(raw);
        if value < 4_294_000_000 {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

fn incoming(
    socket: TcpStream,
    peer: IpAddr,
    config: Arc<ServerConfig>,
    state: &Arc<Mutex<DiscoveryState>>,
    life: &Arc<Lifetime>,
    accepting: &AtomicBool,
) -> Result<()> {
    let mut stream = transport::accept(socket, config)?;
    let binding =
        stream
            .conn
            .export_keying_material([0u8; 32], b"EXPORTER-Channel-Binding", Some(b""))?;
    stream.sock.renew();
    let request: Request = protocol::read_json(&mut stream)?;
    ensure!(
        request.version == 1
            && !request.name.is_empty()
            && request.name.len() <= 240
            && !request.name.chars().any(|c| c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
        "Invalid request"
    );
    let pending = Arc::new(Mutex::new(Pending {
        name: request.name,
        peer,
        code: password()?,
        verified: false,
        expires: Instant::now() + HUMAN_TIMEOUT,
        decision: None,
        canceled: false,
        life: life.clone(),
    }));
    {
        let mut state = state.lock().unwrap();
        ensure!(
            state.pending.is_none() && accepting.load(Ordering::Relaxed),
            "Computer is busy"
        );
        state.pending = Some(pending.clone());
    }
    let result = (|| -> Result<()> {
        protocol::write_json(&mut stream, &true)?;
        let remote = loop {
            ensure!(
                !life.stopped() && pending.lock().unwrap().expires > Instant::now(),
                "Request expired or canceled"
            );
            stream.sock.renew();
            let remote: Option<Vec<u8>> = protocol::read_json(&mut stream)?;
            if let Some(remote) = remote {
                break remote;
            }
            protocol::write_json(&mut stream, &true)?;
        };
        ensure!(remote.len() == 33, "Invalid pairing exchange");
        let code = pending.lock().unwrap().code.clone();
        let (pake, outbound) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &Identity::new(b"localnet-viewer-v1"),
            &Identity::new(b"localnet-host-v1"),
        );
        let key = pake
            .finish(&remote)
            .map_err(|e| anyhow::anyhow!("Pairing exchange failed: {e:?}"))?;
        protocol::write_json(&mut stream, &outbound)?;
        stream.sock.renew();
        let proof: Vec<u8> = protocol::read_json(&mut stream)?;
        verify(&key, &binding, b"viewer", &proof)?;
        protocol::write_json(&mut stream, &confirmation(&key, &binding, b"host"))?;
        pending.lock().unwrap().verified = true;
        loop {
            ensure!(!life.stopped(), "Request canceled");
            stream.sock.renew();
            ensure!(
                protocol::read_json::<bool>(&mut stream)?,
                "Invalid approval heartbeat"
            );
            let mut pending = pending.lock().unwrap();
            ensure!(
                !pending.canceled && Instant::now() < pending.expires,
                "Request expired or canceled"
            );
            let decision = pending.decision.take();
            if decision.is_some() {
                drop(pending);
                protocol::write_json(&mut stream, &decision)?;
                return Ok(());
            }
            ensure!(accepting.load(Ordering::Relaxed), "Computer is busy");
            drop(pending);
            protocol::write_json(&mut stream, &None::<Result<Pairing, String>>)?;
        }
    })();
    pending.lock().unwrap().canceled = true;
    let mut state = state.lock().unwrap();
    if state
        .pending
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &pending))
    {
        state.pending = None;
    }
    result
}

pub struct Outgoing {
    pub device: Device,
    pub code: String,
    pub state: Arc<Mutex<OutgoingState>>,
    life: Arc<Lifetime>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub struct OutgoingState {
    pub status: String,
    pub waiting_code: bool,
    pub entered_code: Option<String>,
    pub result: Option<Result<Pairing, String>>,
}

impl Outgoing {
    pub fn start(device: Device) -> Self {
        let state = Arc::new(Mutex::new(OutgoingState {
            status: "Requesting access...".into(),
            ..Default::default()
        }));
        let life = Arc::new(Lifetime::default());
        let worker = {
            let (state, life, device) = (state.clone(), life.clone(), device.clone());
            thread::spawn(move || {
                let result = outgoing(&device, &state, &life).map_err(|e| e.to_string());
                state.lock().unwrap().result = Some(result);
                life.stop();
            })
        };
        Self {
            device,
            code: String::new(),
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
) -> Result<Pairing> {
    let provisional = Pairing {
        version: protocol::VERSION,
        address: device.address,
        certificate: device.certificate.clone(),
        token: String::new(),
    };
    let mut stream =
        transport::connect(&provisional, transport::client_config(&provisional)?, life)?;
    let binding =
        stream
            .conn
            .export_keying_material([0u8; 32], b"EXPORTER-Channel-Binding", Some(b""))?;
    stream.sock.renew();
    protocol::write_json(
        &mut stream,
        &Request {
            version: 1,
            name: computer_name(),
        },
    )?;
    ensure!(
        protocol::read_json::<bool>(&mut stream)?,
        "Host rejected request"
    );
    {
        let mut state = state.lock().unwrap();
        state.waiting_code = true;
        state.status = "Enter the six-digit code shown on the other computer.".into();
    }
    let expires = Instant::now() + HUMAN_TIMEOUT;
    let code = loop {
        ensure!(
            !life.stopped() && Instant::now() < expires,
            "Request expired or canceled"
        );
        if let Some(code) = state.lock().unwrap().entered_code.take() {
            break code;
        }
        stream.sock.renew();
        protocol::write_json(&mut stream, &None::<Vec<u8>>)?;
        ensure!(
            protocol::read_json::<bool>(&mut stream)?,
            "Request rejected"
        );
        thread::sleep(Duration::from_millis(250));
    };
    ensure!(
        code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
        "Expected six digits"
    );
    let (pake, message) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &Identity::new(b"localnet-viewer-v1"),
        &Identity::new(b"localnet-host-v1"),
    );
    stream.sock.renew();
    protocol::write_json(&mut stream, &Some(message))?;
    let remote: Vec<u8> = protocol::read_json(&mut stream)?;
    ensure!(remote.len() == 33, "Invalid pairing exchange");
    let key = pake
        .finish(&remote)
        .map_err(|e| anyhow::anyhow!("Pairing exchange failed: {e:?}"))?;
    protocol::write_json(&mut stream, &confirmation(&key, &binding, b"viewer"))?;
    let proof: Vec<u8> = protocol::read_json(&mut stream)?;
    verify(&key, &binding, b"host", &proof)?;
    {
        let mut state = state.lock().unwrap();
        state.waiting_code = false;
        state.status = "Code verified. Waiting for Allow on the other computer...".into();
    }
    let decision = loop {
        ensure!(
            !life.stopped() && Instant::now() < expires,
            "Approval expired or canceled"
        );
        stream.sock.renew();
        protocol::write_json(&mut stream, &true)?;
        let decision: Option<Result<Pairing, String>> = protocol::read_json(&mut stream)?;
        if let Some(decision) = decision {
            break decision;
        }
        thread::sleep(Duration::from_millis(250));
    };
    let pair = decision.map_err(anyhow::Error::msg)?;
    ensure!(
        pair.address.ip() == device.address.ip(),
        "Approved host address changed"
    );
    Pairing::decode(&pair.encode()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_is_public_only_and_rejects_malformed_records() {
        let identity =
            rcgen::generate_simple_self_signed(vec![protocol::SERVER_NAME.into()]).unwrap();
        let properties = txt(identity.cert.der(), "Office PC");
        assert!(!properties.contains_key("token") && !properties.contains_key("code"));
        let service = ServiceInfo::new(
            SERVICE,
            "test",
            "test.local.",
            "192.168.1.20",
            45778,
            properties.clone(),
        )
        .unwrap();
        assert_eq!(
            device(&service.as_resolved_service()).unwrap().name,
            "Office PC"
        );
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
        let mut properties = properties;
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
    fn password_authentication_and_channel_binding() {
        let (a, ma) = Spake2::<Ed25519Group>::start_a(
            &Password::new(b"123456"),
            &Identity::new(b"a"),
            &Identity::new(b"b"),
        );
        let (b, mb) = Spake2::<Ed25519Group>::start_b(
            &Password::new(b"123456"),
            &Identity::new(b"a"),
            &Identity::new(b"b"),
        );
        let ka = a.finish(&mb).unwrap();
        let kb = b.finish(&ma).unwrap();
        let proof = confirmation(&ka, b"tls connection A", b"viewer");
        assert!(verify(&kb, b"tls connection A", b"viewer", &proof).is_ok());
        assert!(verify(&kb, b"tls connection B", b"viewer", &proof).is_err());
        assert!(verify(&kb, b"tls connection A", b"host", &proof).is_err());
        let (a, ma) = Spake2::<Ed25519Group>::start_a(
            &Password::new(b"123456"),
            &Identity::new(b"a"),
            &Identity::new(b"b"),
        );
        let (b, mb) = Spake2::<Ed25519Group>::start_b(
            &Password::new(b"123457"),
            &Identity::new(b"a"),
            &Identity::new(b"b"),
        );
        let ka = a.finish(&mb).unwrap();
        let kb = b.finish(&ma).unwrap();
        assert!(
            verify(
                &kb,
                b"tls",
                b"viewer",
                &confirmation(&ka, b"tls", b"viewer")
            )
            .is_err()
        );
    }

    #[test]
    fn consent_requires_code_and_approval_and_handles_denial_cancel() {
        for mode in ["allow", "wrong code", "deny", "cancel", "expire"] {
            let identity =
                rcgen::generate_simple_self_signed(vec![protocol::SERVER_NAME.into()]).unwrap();
            let config = Arc::new(
                ServerConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![identity.cert.der().clone()],
                    PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
                )
                .unwrap(),
            );
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let device = Device {
                name: "Test host".into(),
                address: listener.local_addr().unwrap(),
                certificate: identity.cert.der().to_vec(),
            };
            let state = Arc::new(Mutex::new(DiscoveryState::default()));
            let server_state = state.clone();
            let server = thread::spawn(move || {
                let socket = listener.accept().unwrap().0;
                let life = Arc::new(Lifetime::default());
                life.track(&socket).unwrap();
                incoming(
                    socket,
                    "127.0.0.1".parse().unwrap(),
                    config,
                    &server_state,
                    &life,
                    &AtomicBool::new(true),
                )
            });
            let outgoing = Outgoing::start(device);
            let until = Instant::now() + Duration::from_secs(5);
            let pending = loop {
                if let Some(pending) = state.lock().unwrap().pending.clone() {
                    break pending;
                }
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(5));
            };
            assert!(!pending.lock().unwrap().ready());
            if mode == "cancel" {
                pending.lock().unwrap().cancel();
            } else {
                let code = pending.lock().unwrap().code.clone();
                let entered = if mode == "wrong code" {
                    format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000)
                } else {
                    code
                };
                outgoing.state.lock().unwrap().entered_code = Some(entered);
                if mode != "wrong code" {
                    while !pending.lock().unwrap().verified {
                        assert!(Instant::now() < until);
                        thread::sleep(Duration::from_millis(5));
                    }
                    assert!(
                        outgoing.state.lock().unwrap().result.is_none(),
                        "Code alone must not grant access"
                    );
                    assert!(pending.lock().unwrap().ready());
                    if mode == "expire" {
                        pending.lock().unwrap().expires = Instant::now();
                    } else if mode == "deny" {
                        pending.lock().unwrap().decision = Some(Err("Denied by user".into()));
                    } else {
                        pending.lock().unwrap().decision = Some(Ok(Pairing {
                            version: protocol::VERSION,
                            address: "127.0.0.1:45777".parse().unwrap(),
                            certificate: vec![1],
                            token: "a".repeat(64),
                        }));
                    }
                }
            }
            while outgoing.state.lock().unwrap().result.is_none() {
                assert!(Instant::now() < until, "{mode} timed out");
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                outgoing
                    .state
                    .lock()
                    .unwrap()
                    .result
                    .take()
                    .unwrap()
                    .is_ok(),
                mode == "allow"
            );
            drop(outgoing);
            let server_result = server.join().unwrap();
            assert_eq!(server_result.is_ok(), matches!(mode, "allow" | "deny"));
            assert!(state.lock().unwrap().pending.is_none());
        }
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
        let discovery = Discovery::start_inner(ip, false).unwrap();
        let identity =
            rcgen::generate_simple_self_signed(vec![protocol::SERVER_NAME.into()]).unwrap();
        let id = protocol::random_token().unwrap();
        let mut info = ServiceInfo::new(
            SERVICE,
            &format!("test-{}", &id[..12]),
            &format!("test-{}.local.", &id[..12]),
            ip,
            45999,
            txt(identity.cert.der(), "Localnet discovery test"),
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
        assert_eq!(
            discovery.state.lock().unwrap().devices[&name].name,
            "Localnet discovery test"
        );
        assert!(discovery.state.lock().unwrap().pending.is_none());
        discovery.daemon.unregister(&name).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while discovery.state.lock().unwrap().devices.contains_key(&name) {
            assert!(Instant::now() < until, "mDNS removal was not observed");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
