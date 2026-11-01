use crate::{
    capture,
    input::InputDevice,
    protocol::*,
    transport::{self, Lifetime, ServerStream},
};
use anyhow::{Result, ensure};
use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
use std::{
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

#[derive(Clone, Copy)]
pub struct VideoSettings {
    pub fps: u32,
    pub width: u32,
    pub quality: u8,
}

pub struct Host {
    pub pairing: String,
    pub status: Arc<Mutex<String>>,
    life: Arc<Lifetime>,
    session: Arc<Mutex<Option<Arc<Session>>>>,
    worker: Option<JoinHandle<()>>,
}

struct Session {
    id: String,
    peer: IpAddr,
    life: Lifetime,
    video_claimed: Mutex<bool>,
}

impl Host {
    pub fn start(ip: IpAddr, settings: VideoSettings) -> Result<Self> {
        ensure!(is_local(ip), "Select a private LAN interface");
        ensure!(
            (1..=30).contains(&settings.fps)
                && (640..=3840).contains(&settings.width)
                && (20..=95).contains(&settings.quality),
            "Invalid video settings"
        );
        capture::request_permissions()?;
        let monitor = capture::primary()?;
        let display = capture::info(&monitor)?;
        capture::jpeg(&monitor, 640, 50)?;
        let listener = TcpListener::bind(SocketAddr::new(ip, 45777))?;
        listener.set_nonblocking(true)?;
        let identity = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()])?;
        let config = Arc::new(
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .with_no_client_auth()
                .with_single_cert(
                    vec![identity.cert.der().clone()],
                    PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
                )?,
        );
        let pair = Pairing {
            version: VERSION,
            address: listener.local_addr()?,
            certificate: identity.cert.der().to_vec(),
            token: random_token()?,
        };
        let pairing = pair.encode()?;
        let status = Arc::new(Mutex::new(format!(
            "Sharing {} on {}. Waiting for your viewer.",
            display.name, pair.address
        )));
        let life = Arc::new(Lifetime::default());
        let session = Arc::new(Mutex::new(None::<Arc<Session>>));
        let worker = {
            let (life, session, status) = (life.clone(), session.clone(), status.clone());
            thread::spawn(move || {
                let mut workers: Vec<JoinHandle<()>> = Vec::new();
                while !life.stopped() {
                    let mut index = 0;
                    while index < workers.len() {
                        if workers[index].is_finished() {
                            let _ = workers.swap_remove(index).join();
                        } else {
                            index += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((socket, peer)) if is_local(peer.ip()) && workers.len() < 4 => {
                            let (config, token, session, status, display, host_life) = (
                                config.clone(),
                                pair.token.clone(),
                                session.clone(),
                                status.clone(),
                                display.clone(),
                                life.clone(),
                            );
                            workers.push(thread::spawn(move || {
                                // Authentication failures intentionally do not reveal credentials or disrupt UI status.
                                let result = connection(
                                    socket,
                                    peer.ip(),
                                    config,
                                    token,
                                    session,
                                    status,
                                    display,
                                    settings,
                                    host_life,
                                );
                                #[cfg(test)]
                                if let Err(e) = result {
                                    eprintln!("Host test connection ended: {e:#}");
                                }
                                #[cfg(not(test))]
                                let _ = result;
                            }));
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20))
                        }
                        Err(e) => {
                            *status.lock().unwrap() = format!("Listener failed: {e}");
                            break;
                        }
                    }
                }
                if let Some(active) = session.lock().unwrap().as_ref() {
                    active.life.stop();
                }
                for worker in workers {
                    let _ = worker.join();
                }
            })
        };
        Ok(Self {
            pairing,
            status,
            life,
            session,
            worker: Some(worker),
        })
    }

    pub fn stop(&self) {
        self.life.stop();
        if let Some(session) = self.session.lock().unwrap().as_ref() {
            session.life.stop();
        }
    }

    pub fn has_viewer(&self) -> bool {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|session| !session.life.stopped())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn connection(
    socket: TcpStream,
    peer: IpAddr,
    config: Arc<ServerConfig>,
    token: String,
    sessions: Arc<Mutex<Option<Arc<Session>>>>,
    status: Arc<Mutex<String>>,
    display: DisplayInfo,
    settings: VideoSettings,
    host_life: Arc<Lifetime>,
) -> Result<()> {
    let cancellation = socket.try_clone()?;
    let mut stream = transport::accept(socket, config)?;
    stream.sock.renew();
    let hello: Hello = read_json(&mut stream)?;
    ensure!(
        hello.version == VERSION && bool::from(hello.token.as_bytes().ct_eq(token.as_bytes())),
        "Unauthorized"
    );
    ensure!(!host_life.stopped(), "Host stopped");
    match hello.channel {
        Channel::Control => {
            let active = Arc::new(Session {
                id: random_token()?,
                peer,
                life: Lifetime::default(),
                video_claimed: Mutex::new(false),
            });
            active.life.track(&cancellation)?;
            {
                let mut slot = sessions.lock().unwrap();
                ensure!(
                    slot.is_none() && !host_life.stopped(),
                    "Host busy or stopped"
                );
                *slot = Some(active.clone());
            }
            *status.lock().unwrap() =
                format!("Viewer connected from {peer}. Stop sharing to revoke access.");
            let result = control(&mut stream, &active, &display);
            active.life.stop();
            *sessions.lock().unwrap() = None;
            *status.lock().unwrap() = match &result {
                Ok(_) => "Viewer disconnected. Waiting for another connection.".into(),
                Err(e) => format!("Session ended: {e}. Waiting for another connection."),
            };
            result
        }
        Channel::Video { session: id } => {
            let active = sessions
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("No active session"))?;
            ensure!(
                active.peer == peer
                    && bool::from(id.as_bytes().ct_eq(active.id.as_bytes()))
                    && !active.life.stopped(),
                "Invalid video session"
            );
            {
                let mut claimed = active.video_claimed.lock().unwrap();
                ensure!(!*claimed, "Video already connected");
                *claimed = true;
            }
            active.life.track(&cancellation)?;
            let result = video(&mut stream, &active, &display, settings);
            active.life.stop();
            result
        }
    }
}

fn control(stream: &mut ServerStream, session: &Session, display: &DisplayInfo) -> Result<()> {
    let mut device = InputDevice::new()?;
    stream.sock.renew();
    write_json(
        stream,
        &Welcome {
            session: session.id.clone(),
            display: display.clone(),
        },
    )?;
    while !session.life.stopped() {
        stream.sock.renew();
        let events: Vec<Input> = read_json(stream)?;
        ensure!(events.len() <= MAX_EVENTS, "Too many input events");
        for event in events {
            ensure!(!session.life.stopped(), "Session stopped");
            device.apply(event, display)?;
        }
        write_json(stream, &true)?;
    }
    Ok(())
}

fn video(
    stream: &mut ServerStream,
    session: &Session,
    expected: &DisplayInfo,
    settings: VideoSettings,
) -> Result<()> {
    let monitor = capture::primary()?;
    let interval = Duration::from_secs_f64(1.0 / f64::from(settings.fps));
    let mut next = Instant::now();
    while !session.life.stopped() {
        stream.sock.renew();
        let request: bool = read_json(stream)?;
        ensure!(request, "Invalid frame request");
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        let actual = capture::info(&monitor)?;
        ensure!(
            actual.width == expected.width
                && actual.height == expected.height
                && actual.x == expected.x
                && actual.y == expected.y,
            "Display changed. Restart sharing."
        );
        ensure!(!session.life.stopped(), "Session stopped");
        let started = Instant::now();
        let bytes = capture::jpeg(&monitor, settings.width, settings.quality)?;
        write_packet(stream, &bytes, MAX_FRAME)?;
        next = started + interval;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Requires an unlocked interactive desktop; captures only over loopback and does not send input"]
    fn desktop_loopback_capture_auth_and_reconnect() {
        let host = Host::start(
            "127.0.0.1".parse().unwrap(),
            VideoSettings {
                fps: 10,
                width: 640,
                quality: 50,
            },
        )
        .unwrap();
        let pair = Pairing::decode(&host.pairing).unwrap();
        let ctx = eframe::egui::Context::default();
        let mut unauthorized = pair.clone();
        unauthorized.token = "0".repeat(64);
        let wrong_viewer = crate::viewer::Viewer::start(unauthorized, ctx.clone());
        let until = Instant::now() + Duration::from_secs(5);
        while wrong_viewer.connected() && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!wrong_viewer.connected());
        drop(wrong_viewer);
        for _ in 0..2 {
            let viewer = crate::viewer::Viewer::start(pair.clone(), ctx.clone());
            let until = Instant::now() + Duration::from_secs(10);
            while viewer.state.lock().unwrap().frames < 3 && Instant::now() < until {
                assert!(
                    viewer.connected(),
                    "{}",
                    viewer.state.lock().unwrap().status
                );
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                viewer.state.lock().unwrap().frames >= 3,
                "{}",
                viewer.state.lock().unwrap().status
            );
            assert!(viewer.state.lock().unwrap().frame.is_some());
            assert!(viewer.state.lock().unwrap().input_rtt_ms > 0.0);
            viewer.stop();
            drop(viewer);
            let until = Instant::now() + Duration::from_secs(5);
            while host.session.lock().unwrap().is_some() && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(host.session.lock().unwrap().is_none());
        }
        host.stop();
    }
}
