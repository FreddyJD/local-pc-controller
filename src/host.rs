use crate::{
    capture,
    input::InputDevice,
    latest::{Latest, Poll},
    protocol::*,
    transport::{self, Lifetime, ServerStream},
};
use anyhow::{Context, Result, ensure};
use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
use std::{
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

const VIDEO_HEARTBEAT: Duration = Duration::from_millis(250);
const CAPTURE_STALL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
pub struct VideoSettings {
    pub fps: u32,
    pub width: u32,
    pub quality: u8,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            fps: 60,
            width: 1920,
            quality: 70,
        }
    }
}

#[derive(Default)]
pub struct HostState {
    pub status: String,
    pub sharing: bool,
    pub has_viewer: bool,
}

struct Launch {
    peer: IpAddr,
    life: Arc<Lifetime>,
    reply: SyncSender<std::result::Result<SessionTicket, RemoteError>>,
}

pub struct HostController {
    pub state: Arc<Mutex<HostState>>,
    pub settings: Arc<Mutex<VideoSettings>>,
    requests: SyncSender<Launch>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HostController {
    pub fn start(ip: IpAddr, settings: VideoSettings) -> Self {
        Self::start_with(ip, settings, Host::start)
    }

    fn start_with(
        ip: IpAddr,
        settings: VideoSettings,
        launcher: fn(IpAddr, IpAddr, VideoSettings) -> Result<Host>,
    ) -> Self {
        let (requests, receiver) = mpsc::sync_channel::<Launch>(1);
        let state = Arc::new(Mutex::new(HostState {
            status: "Ready for local connections. No pairing or approval required.".into(),
            ..Default::default()
        }));
        let settings = Arc::new(Mutex::new(settings));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker = {
            let (state, settings, stopped) = (state.clone(), settings.clone(), stopped.clone());
            thread::spawn(move || {
                let mut host: Option<Host> = None;
                while !stopped.load(Ordering::Relaxed) {
                    if host.as_ref().is_some_and(Host::finished) {
                        let last_status = host.as_ref().unwrap().status.lock().unwrap().clone();
                        host.take();
                        *state.lock().unwrap() = HostState {
                            status: format!("{last_status} Ready for another local connection."),
                            ..Default::default()
                        };
                    }
                    if let Some(host) = &host {
                        *state.lock().unwrap() = HostState {
                            sharing: true,
                            has_viewer: host.has_viewer(),
                            status: host.status.lock().unwrap().clone(),
                        };
                    }
                    match receiver.recv_timeout(Duration::from_millis(20)) {
                        Ok(request) if !request.life.stopped() => {
                            // Retire the old listener fully before accepting an immediate reconnect.
                            if host.as_ref().is_some_and(Host::finished) {
                                host.take();
                            }
                            let response = if host.is_some() {
                                Err(RemoteError::new(
                                    "busy",
                                    "This computer already has a viewer or a connection is starting. Disconnect it and retry.",
                                ))
                            } else {
                                state.lock().unwrap().status = format!(
                                    "Starting local screen sharing for {}...",
                                    request.peer
                                );
                                let current_settings = *settings.lock().unwrap();
                                match launcher(ip, request.peer, current_settings) {
                                    Ok(started)
                                        if !request.life.stopped()
                                            && !stopped.load(Ordering::Relaxed) =>
                                    {
                                        let ticket = started.ticket.clone();
                                        host = Some(started);
                                        Ok(ticket)
                                    }
                                    Ok(_) => Err(RemoteError::new(
                                        "canceled",
                                        "Connection canceled during startup",
                                    )),
                                    Err(e) => {
                                        Err(RemoteError::new("host_start_failed", format!("{e:#}")))
                                    }
                                }
                            };
                            if let Err(e) = &response {
                                state.lock().unwrap().status = e.to_string();
                            }
                            let success = response.is_ok();
                            if request.reply.send(response).is_err() && success {
                                host.take();
                            }
                        }
                        Ok(_) => {}
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                host.take();
                *state.lock().unwrap() = HostState {
                    status: "Local access disabled".into(),
                    ..Default::default()
                };
            })
        };
        Self {
            state,
            settings,
            requests,
            stopped,
            worker: Some(worker),
        }
    }

    pub fn request(
        &self,
        peer: IpAddr,
        life: Arc<Lifetime>,
    ) -> std::result::Result<Receiver<std::result::Result<SessionTicket, RemoteError>>, RemoteError>
    {
        if self.stopped.load(Ordering::Relaxed) {
            return Err(RemoteError::new("disabled", "Local access is disabled"));
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.requests
            .try_send(Launch { peer, life, reply })
            .map_err(|_| {
                RemoteError::new("busy", "Another connection is starting; retry shortly")
            })?;
        Ok(receiver)
    }
}

impl Drop for HostController {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub struct Host {
    pub ticket: SessionTicket,
    pub status: Arc<Mutex<String>>,
    life: Arc<Lifetime>,
    session: Arc<Mutex<Option<Arc<Session>>>>,
    worker: Option<JoinHandle<()>>,
    claimed: Arc<AtomicBool>,
    started: Instant,
}

struct Session {
    id: String,
    peer: IpAddr,
    life: Lifetime,
    video_claimed: Mutex<bool>,
    error: Mutex<Option<String>>,
}

impl Host {
    pub fn start(ip: IpAddr, peer: IpAddr, settings: VideoSettings) -> Result<Self> {
        ensure!(is_local(ip), "Select a private LAN interface");
        ensure!(is_local(peer), "Only private LAN peers are allowed");
        ensure!(
            (1..=60).contains(&settings.fps)
                && (640..=3840).contains(&settings.width)
                && (20..=95).contains(&settings.quality),
            "Invalid video settings"
        );
        capture::check_permissions().context(
            "Host permissions (on the Mac, use Check / request OS permissions in Localnet)",
        )?;
        let monitor = capture::primary().context("Primary display")?;
        let display = capture::info(&monitor).context("Display information")?;
        capture::jpeg(&monitor, 640, 50)
            .context("Initial screen capture (check Screen Recording permission on Mac)")?;
        let listener = TcpListener::bind(SocketAddr::new(ip, 45777))
            .context("Sharing port 45777 (another Localnet may already be running)")?;
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
        let pair = SessionTicket {
            version: VERSION,
            address: listener.local_addr()?,
            certificate: identity.cert.der().to_vec(),
            token: random_token()?,
        };
        let ticket = pair.clone();
        let expected_peer = peer;
        let status = Arc::new(Mutex::new(format!(
            "Sharing {} on {}. Waiting for your viewer.",
            display.name, pair.address
        )));
        let life = Arc::new(Lifetime::default());
        let session = Arc::new(Mutex::new(None::<Arc<Session>>));
        let claimed = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let worker = {
            let claimed = claimed.clone();
            let (life, session, status) = (life.clone(), session.clone(), status.clone());
            thread::spawn(move || {
                let mut workers: Vec<(JoinHandle<()>, Arc<Lifetime>)> = Vec::new();
                while !life.stopped() {
                    if !claimed.load(Ordering::Relaxed)
                        && started.elapsed() > Duration::from_secs(30)
                    {
                        life.stop();
                        break;
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
                        Ok((socket, peer)) if peer.ip() == expected_peer && workers.len() < 4 => {
                            let connection_life = Arc::new(Lifetime::default());
                            if connection_life.track(&socket).is_err() {
                                continue;
                            }
                            // Track unauthenticated sockets too, without retaining every socket for the full host lifetime.
                            let cancel = connection_life.clone();
                            let (config, token, session, status, display, host_life) = (
                                config.clone(),
                                pair.token.clone(),
                                session.clone(),
                                status.clone(),
                                display.clone(),
                                life.clone(),
                            );
                            let claimed = claimed.clone();
                            workers.push((
                                thread::spawn(move || {
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
                                        claimed,
                                        started,
                                    );
                                    cancel.stop();
                                    #[cfg(test)]
                                    if let Err(e) = result {
                                        eprintln!("Host test connection ended: {e:#}");
                                    }
                                    #[cfg(not(test))]
                                    let _ = result;
                                }),
                                connection_life,
                            ));
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20))
                        }
                        Err(e) => {
                            *status.lock().unwrap() = format!("Listener failed: {e}");
                            life.stop();
                            break;
                        }
                    }
                }
                if let Some(active) = session.lock().unwrap().as_ref() {
                    active.life.stop();
                }
                drop(listener);
                for (_, cancel) in &workers {
                    cancel.stop();
                }
                for (worker, _) in workers {
                    let _ = worker.join();
                }
            })
        };
        Ok(Self {
            ticket,
            status,
            life,
            session,
            worker: Some(worker),
            claimed,
            started,
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

    pub fn finished(&self) -> bool {
        self.life.stopped()
            || ((self.claimed.load(Ordering::Relaxed) && self.session.lock().unwrap().is_none())
                || (!self.claimed.load(Ordering::Relaxed)
                    && self.started.elapsed() > Duration::from_secs(30)))
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
    claimed: Arc<AtomicBool>,
    started: Instant,
) -> Result<()> {
    let cancellation = socket.try_clone()?;
    let mut stream = transport::accept(socket, config)?;
    stream.sock.renew();
    let hello: Hello = read_json(&mut stream)?;
    let admission = (|| -> Result<()> {
        ensure!(
            hello.version == VERSION,
            "Update Localnet on both computers (session protocol mismatch)"
        );
        ensure!(
            bool::from(hello.token.as_bytes().ct_eq(token.as_bytes())),
            "Invalid or expired session ticket"
        );
        ensure!(!host_life.stopped(), "Sharing stopped");
        Ok(())
    })();
    if let Err(e) = admission {
        match &hello.channel {
            Channel::Control => write_json(&mut stream, &Err::<Welcome, String>(e.to_string()))?,
            Channel::Video { .. } => write_frame_error(&mut stream, &e.to_string())?,
        }
        return Err(e);
    }
    match hello.channel {
        Channel::Control => {
            let active = Arc::new(Session {
                id: random_token()?,
                peer,
                life: Lifetime::default(),
                video_claimed: Mutex::new(false),
                error: Mutex::new(None),
            });
            active.life.track(&cancellation)?;
            let admission = (|| -> Result<()> {
                let mut slot = sessions.lock().unwrap();
                ensure!(
                    slot.is_none() && !host_life.stopped(),
                    "Host busy or stopped"
                );
                ensure!(
                    started.elapsed() < Duration::from_secs(30)
                        && claimed
                            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok(),
                    "Session ticket expired or already used; reconnect from Nearby computers"
                );
                *slot = Some(active.clone());
                Ok(())
            })();
            if let Err(e) = admission {
                write_json(&mut stream, &Err::<Welcome, String>(e.to_string()))?;
                return Err(e);
            }
            *status.lock().unwrap() =
                format!("Viewer connected from {peer}. Stop sharing to revoke access.");
            let result = control(&mut stream, &active, &display);
            let root_error = active.error.lock().unwrap().take();
            active.life.stop();
            host_life.stop();
            *sessions.lock().unwrap() = None;
            *status.lock().unwrap() = if let Some(error) = root_error {
                format!("Session ended: {error}")
            } else {
                match &result {
                    Ok(_) => "Viewer disconnected. Ready for another connection.".into(),
                    Err(e) => format!("Session ended: {e:#}. Ready for another connection."),
                }
            };
            result
        }
        Channel::Video { session: id } => {
            let admission = (|| -> Result<Arc<Session>> {
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
                Ok(active)
            })();
            let active = match admission {
                Ok(active) => active,
                Err(e) => {
                    write_frame_error(&mut stream, &e.to_string())?;
                    return Err(e);
                }
            };
            active.life.track(&cancellation)?;
            let result = video(&mut stream, &active, &display, settings);
            if let Err(e) = &result {
                *active.error.lock().unwrap() = Some(format!("Screen capture: {e:#}"));
            }
            active.life.stop();
            result
        }
    }
}

fn control(stream: &mut ServerStream, session: &Session, display: &DisplayInfo) -> Result<()> {
    let mut device = match InputDevice::new() {
        Ok(device) => device,
        Err(e) => {
            write_json(
                stream,
                &Err::<Welcome, String>(format!(
                    "Input unavailable: {e}. On Mac enable Accessibility for Localnet, then restart."
                )),
            )?;
            return Err(e);
        }
    };
    stream.sock.renew();
    write_json(
        stream,
        &Ok::<_, String>(Welcome {
            session: session.id.clone(),
            display: display.clone(),
        }),
    )?;
    while !session.life.stopped() {
        stream.sock.renew();
        let events: Vec<Input> = read_json(stream)?;
        ensure!(events.len() <= MAX_EVENTS, "Too many input events");
        let applied = (|| -> Result<()> {
            for event in events {
                ensure!(!session.life.stopped(), "Session stopped");
                device.apply(event, display)?;
            }
            Ok(())
        })();
        write_json(
            stream,
            &applied
                .as_ref()
                .map(|_| ())
                .map_err(|e| format!("Input injection failed: {e:#}")),
        )?;
        applied?;
    }
    Ok(())
}

fn video(
    stream: &mut ServerStream,
    session: &Session,
    expected: &DisplayInfo,
    settings: VideoSettings,
) -> Result<()> {
    stream.sock.renew();
    ensure!(read_json::<bool>(stream)?, "Invalid first frame request");
    let frames = Arc::new(Latest::default());
    let stop = Arc::new(Lifetime::default());
    let worker = {
        let (frames, stop, expected) = (frames.clone(), stop.clone(), expected.clone());
        thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    // Native capture resources stay on their creating thread for the entire session.
                    let mut capture = capture::VideoCapture::new(settings.width, settings.quality)?;
                    let interval = Duration::from_secs_f64(1.0 / f64::from(settings.fps));
                    while !stop.stopped() {
                        let started = Instant::now();
                        let actual = capture.display()?;
                        ensure!(
                            actual.width == expected.width
                                && actual.height == expected.height
                                && actual.x == expected.x
                                && actual.y == expected.y,
                            "Display changed. Restart sharing."
                        );
                        let bytes = capture.frame()?.to_vec();
                        ensure!(
                            !bytes.is_empty() && bytes.len() < MAX_FRAME,
                            "Capture frame exceeds limit"
                        );
                        if !frames.publish(bytes) {
                            break;
                        }
                        // Pace from this capture, never catch up in bursts after a slow capture.
                        while !stop.stopped() && started.elapsed() < interval {
                            thread::sleep(
                                interval
                                    .saturating_sub(started.elapsed())
                                    .min(Duration::from_millis(4)),
                            );
                        }
                    }
                    Ok(())
                }));
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("Screen capture: {e:#}")),
                Err(_) => Some("Screen capture worker panicked".into()),
            };
            frames.close(error);
        })
    };
    let result = send_video_frames(stream, &session.life, &frames, CAPTURE_STALL_TIMEOUT);
    if let Err(error) = &result {
        *session.error.lock().unwrap() = Some(format!("{error:#}"));
    }
    // Release remote input even if the native OS capture call is still blocked during cleanup.
    session.life.stop();
    stop.stop();
    frames.close(None);
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("Capture worker panicked"))?;
    result
}

fn send_video_frames(
    stream: &mut ServerStream,
    life: &Lifetime,
    frames: &Latest<Vec<u8>>,
    stall_timeout: Duration,
) -> Result<()> {
    let mut last_frame = Instant::now();
    while !life.stopped() {
        let pending = match frames.poll(life, VIDEO_HEARTBEAT) {
            Ok(Poll::Closed) => return Ok(()),
            Ok(Poll::Frame(bytes)) => {
                stream.sock.renew();
                write_frame(stream, &bytes).context("Sending video frame")?;
                last_frame = Instant::now();
                false
            }
            Ok(Poll::Pending) if last_frame.elapsed() < stall_timeout => {
                stream.sock.renew();
                write_frame_pending(stream).context("Sending capture heartbeat")?;
                true
            }
            result => {
                let error = match result {
                    Err(error) => error,
                    _ => anyhow::anyhow!(
                        "Screen capture produced no frame for {} seconds. Check whether the remote computer is awake and unlocked, and whether Localnet still has Screen Recording permission. Disconnect and retry.",
                        stall_timeout.as_secs()
                    ),
                };
                // Only mailbox failures reach here; transport errors never append data to a partial packet.
                stream.sock.renew();
                write_frame_error(stream, &format!("{error:#}"))?;
                stream.sock.renew();
                let _ = read_json::<bool>(stream);
                return Err(error);
            }
        };
        stream.sock.renew();
        ensure!(
            read_json::<bool>(stream).with_context(|| if pending {
                "Waiting for capture-heartbeat acknowledgment"
            } else {
                "Waiting for frame acknowledgment"
            })?,
            "Invalid frame request"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_test_listener() -> (TcpListener, Arc<ServerConfig>, SessionTicket) {
        let identity = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()]).unwrap();
        let config = Arc::new(
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
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
        let ticket = SessionTicket {
            version: VERSION,
            address: listener.local_addr().unwrap(),
            certificate: identity.cert.der().to_vec(),
            token: "a".repeat(64),
        };
        (listener, config, ticket)
    }

    #[test]
    fn delayed_capture_stays_connected_beyond_packet_timeout_and_resumes_frames() {
        let (listener, config, ticket) = video_test_listener();
        let frames = Arc::new(Latest::default());
        let server_frames = frames.clone();
        let life = Arc::new(Lifetime::default());
        let server_life = life.clone();
        let server = thread::spawn(move || {
            let socket = listener.accept().unwrap().0;
            server_life.track(&socket).unwrap();
            let mut stream = transport::accept(socket, config).unwrap();
            assert!(read_json::<bool>(&mut stream).unwrap());
            send_video_frames(
                &mut stream,
                &server_life,
                &server_frames,
                Duration::from_secs(15),
            )
        });
        let client_life = Lifetime::default();
        let mut stream = transport::connect(
            &ticket,
            transport::client_config(&ticket).unwrap(),
            &client_life,
        )
        .unwrap();
        write_json(&mut stream, &true).unwrap();
        let began = Instant::now();
        let mut heartbeats = 0;
        while began.elapsed() < Duration::from_millis(3300) {
            stream.sock.renew();
            assert!(read_frame(&mut stream).unwrap().is_none());
            write_json(&mut stream, &true).unwrap();
            heartbeats += 1;
        }
        assert!(heartbeats >= 10);
        for value in [b"first frame".as_slice(), b"second frame".as_slice()] {
            assert!(frames.publish(value.to_vec()));
            loop {
                stream.sock.renew();
                let packet = read_frame(&mut stream).unwrap();
                write_json(&mut stream, &true).unwrap();
                if let Some(frame) = packet {
                    assert_eq!(frame, value);
                    break;
                }
            }
        }
        life.stop();
        frames.close(None);
        assert!(server.join().unwrap().is_ok());
    }

    #[test]
    fn capture_stall_becomes_readable_error_and_missing_peer_ack_still_times_out() {
        for acknowledge in [true, false] {
            let (listener, config, ticket) = video_test_listener();
            let server = thread::spawn(move || {
                let mut stream = transport::accept(listener.accept().unwrap().0, config).unwrap();
                assert!(read_json::<bool>(&mut stream).unwrap());
                send_video_frames(
                    &mut stream,
                    &Lifetime::default(),
                    &Latest::default(),
                    Duration::from_millis(600),
                )
            });
            let mut stream = transport::connect(
                &ticket,
                transport::client_config(&ticket).unwrap(),
                &Lifetime::default(),
            )
            .unwrap();
            write_json(&mut stream, &true).unwrap();
            if acknowledge {
                loop {
                    stream.sock.renew();
                    let packet = read_frame(&mut stream);
                    write_json(&mut stream, &true).unwrap();
                    match packet {
                        Ok(None) => {}
                        Err(error) => {
                            assert!(error.is::<RemoteVideoError>());
                            assert!(
                                error
                                    .to_string()
                                    .contains("Screen capture produced no frame")
                            );
                            break;
                        }
                        _ => panic!("Unexpected JPEG"),
                    }
                }
            }
            let error = server.join().unwrap().unwrap_err();
            assert!(error.to_string().contains(if acknowledge {
                "Screen capture produced no frame"
            } else {
                "capture-heartbeat acknowledgment"
            }));
        }
    }
    fn permission_failure(_: IpAddr, _: IpAddr, _: VideoSettings) -> Result<Host> {
        anyhow::bail!("Enable Screen Recording and Accessibility for Localnet on Mac")
    }

    #[test]
    fn startup_worker_returns_permission_errors_and_retries_without_ui() {
        let controller = HostController::start_with(
            "127.0.0.1".parse().unwrap(),
            VideoSettings::default(),
            permission_failure,
        );
        for _ in 0..2 {
            let response = controller
                .request("127.0.0.1".parse().unwrap(), Arc::new(Lifetime::default()))
                .unwrap();
            let error = response
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .err()
                .unwrap();
            assert_eq!(error.code, "host_start_failed");
            assert!(error.message.contains("Screen Recording"));
        }
    }

    #[test]
    fn canceled_startup_does_not_leave_host_or_block_next_request() {
        fn delayed_failure(_: IpAddr, _: IpAddr, _: VideoSettings) -> Result<Host> {
            thread::sleep(Duration::from_millis(100));
            anyhow::bail!("Test startup canceled")
        }
        let controller = HostController::start_with(
            "127.0.0.1".parse().unwrap(),
            VideoSettings::default(),
            delayed_failure,
        );
        let life = Arc::new(Lifetime::default());
        let receiver = controller
            .request("127.0.0.1".parse().unwrap(), life.clone())
            .unwrap();
        life.stop();
        drop(receiver);
        thread::sleep(Duration::from_millis(150));
        let receiver = controller
            .request("127.0.0.1".parse().unwrap(), Arc::new(Lifetime::default()))
            .unwrap();
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        assert!(!controller.state.lock().unwrap().sharing);
    }

    #[test]
    #[ignore = "Requires an unlocked interactive desktop; captures only over loopback and does not send input"]
    fn desktop_loopback_capture_auth_and_reconnect() {
        let controller = HostController::start(
            "127.0.0.1".parse().unwrap(),
            VideoSettings {
                fps: 10,
                width: 640,
                quality: 50,
            },
        );
        let ctx = eframe::egui::Context::default();
        for _ in 0..2 {
            let pair = controller
                .request("127.0.0.1".parse().unwrap(), Arc::new(Lifetime::default()))
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let mut unauthorized = pair.clone();
            unauthorized.token = "0".repeat(64);
            let wrong_viewer = crate::viewer::Viewer::start(unauthorized, ctx.clone());
            let until = Instant::now() + Duration::from_secs(5);
            while wrong_viewer.connected() {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(20));
            }
            assert!(wrong_viewer.state.lock().unwrap().status.contains("ticket"));
            drop(wrong_viewer);
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
            let busy = controller
                .request("127.0.0.1".parse().unwrap(), Arc::new(Lifetime::default()))
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .err()
                .unwrap();
            assert_eq!(busy.code, "busy");
            viewer.stop();
            drop(viewer);
            let until = Instant::now() + Duration::from_secs(5);
            while controller.state.lock().unwrap().sharing && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(!controller.state.lock().unwrap().sharing);
        }
    }

    #[test]
    #[ignore = "Interactive desktop capture over loopback; verifies session ticket cannot reconnect; no input injection"]
    fn session_ticket_is_single_use() {
        let host = Host::start(
            "127.0.0.1".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            VideoSettings {
                fps: 60,
                width: 1920,
                quality: 70,
            },
        )
        .unwrap();
        let pair = host.ticket.clone();
        let ctx = eframe::egui::Context::default();
        let viewer = crate::viewer::Viewer::start(pair.clone(), ctx.clone());
        let until = Instant::now() + Duration::from_secs(10);
        while viewer.state.lock().unwrap().frames < 3 {
            assert!(
                viewer.connected(),
                "{}",
                viewer.state.lock().unwrap().status
            );
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(20));
        }
        assert!(host.has_viewer());
        viewer.stop();
        drop(viewer);
        let until = Instant::now() + Duration::from_secs(5);
        while !host.finished() {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(20));
        }
        let viewer = crate::viewer::Viewer::start(pair, ctx);
        let until = Instant::now() + Duration::from_secs(5);
        while viewer.connected() {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(viewer.state.lock().unwrap().frames, 0);
    }
}
