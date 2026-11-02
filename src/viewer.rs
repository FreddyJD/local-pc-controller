use crate::{
    latest::Latest,
    protocol::*,
    transport::{self, Lifetime},
};
use anyhow::{Context, Result, ensure};
use eframe::egui;
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct ViewerState {
    pub status: String,
    pub display: Option<DisplayInfo>,
    pub frame: Option<egui::ColorImage>,
    pub frames: u64,
    pub input_rtt_ms: f64,
    pub frame_roundtrip_ms: f64,
    pub received: u64,
    pub compressed_replaced: u64,
    pub decoded_replaced: u64,
    pub decode_ms: f64,
    pub receive_ms: f64,
    pub decoded_at: Option<Instant>,
    pub received_fps: f64,
    pub decoded_fps: f64,
    pub video_heartbeats: u64,
    pub capture_wait_ms: f64,
}

#[derive(Default)]
struct InputQueue {
    events: Mutex<VecDeque<Input>>,
    wake: Condvar,
}

impl InputQueue {
    fn push(&self, event: Input) -> Result<()> {
        let mut queue = self.events.lock().unwrap();
        if matches!(event, Input::ReleaseAll) {
            queue.clear();
        }
        if matches!(event, Input::Move { .. }) && matches!(queue.back(), Some(Input::Move { .. })) {
            queue.pop_back();
        }
        ensure!(queue.len() < 256, "Input backlog exceeded safety limit");
        queue.push_back(event);
        self.wake.notify_one();
        Ok(())
    }

    fn batch(&self) -> Vec<Input> {
        let queue = self.events.lock().unwrap();
        let mut queue = self
            .wake
            .wait_timeout_while(queue, Duration::from_millis(250), |q| q.is_empty())
            .unwrap()
            .0;
        let count = queue.len().min(MAX_EVENTS);
        queue.drain(..count).collect()
    }
}

pub struct Viewer {
    pub state: Arc<Mutex<ViewerState>>,
    life: Arc<Lifetime>,
    queue: Arc<InputQueue>,
    worker: Option<JoinHandle<()>>,
    user_stopped: Arc<std::sync::atomic::AtomicBool>,
}

impl Viewer {
    #[cfg(test)]
    pub(crate) fn test_session() -> Self {
        Self {
            state: Arc::new(Mutex::new(ViewerState::default())),
            life: Arc::new(Lifetime::default()),
            queue: Arc::new(InputQueue::default()),
            worker: None,
            user_stopped: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_inputs(&self) -> Vec<Input> {
        self.queue.batch()
    }

    pub fn start(pair: SessionTicket, ctx: egui::Context) -> Self {
        let state = Arc::new(Mutex::new(ViewerState {
            status: "Connecting over encrypted LAN...".into(),
            ..Default::default()
        }));
        let life = Arc::new(Lifetime::default());
        let queue = Arc::new(InputQueue::default());
        let user_stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = {
            let user_stopped = user_stopped.clone();
            let (state, life, queue) = (state.clone(), life.clone(), queue.clone());
            thread::spawn(move || {
                let result = session(pair, &state, &life, &queue, &ctx);
                life.stop();
                let mut state = state.lock().unwrap();
                state.status = if user_stopped.load(std::sync::atomic::Ordering::Relaxed) {
                    "Disconnected".into()
                } else {
                    match result {
                        Ok(_) => "Disconnected".into(),
                        Err(e) => format!("Disconnected: {e:#}"),
                    }
                };
                ctx.request_repaint();
            })
        };
        Self {
            state,
            life,
            queue,
            worker: Some(worker),
            user_stopped,
        }
    }

    pub fn send(&self, input: Input) {
        if !self.life.stopped()
            && let Err(e) = self.queue.push(input)
        {
            *self.state.lock().unwrap() = ViewerState {
                status: e.to_string(),
                ..Default::default()
            };
            self.stop();
        }
    }
    pub fn connected(&self) -> bool {
        !self.life.stopped()
    }
    pub fn stop(&self) {
        self.user_stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.life.stop();
        self.queue.wake.notify_all();
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn session(
    pair: SessionTicket,
    state: &Arc<Mutex<ViewerState>>,
    life: &Arc<Lifetime>,
    queue: &Arc<InputQueue>,
    ctx: &egui::Context,
) -> Result<()> {
    let config = transport::client_config(&pair)?;
    let mut control =
        transport::connect(&pair, config.clone(), life).context("Control TLS connection")?;
    control.sock.renew();
    write_json(
        &mut control,
        &Hello {
            version: VERSION,
            token: pair.token.clone(),
            channel: Channel::Control,
        },
    )?;
    let welcome: std::result::Result<Welcome, String> =
        read_json(&mut control).context("Remote input initialization")?;
    let welcome = welcome.map_err(anyhow::Error::msg)?;
    ensure!(
        welcome.session.len() == 64 && welcome.display.width > 0 && welcome.display.height > 0,
        "Invalid host metadata"
    );
    state.lock().unwrap().display = Some(welcome.display);
    let mut video = transport::connect(&pair, config, life).context("Video TLS connection")?;
    video.sock.renew();
    write_json(
        &mut video,
        &Hello {
            version: VERSION,
            token: pair.token,
            channel: Channel::Video {
                session: welcome.session,
            },
        },
    )?;
    state.lock().unwrap().status = "Connected. Click the remote screen to take control.".into();
    ctx.request_repaint();
    let first_error = Arc::new(Mutex::new(None::<String>));
    let video_worker = {
        let first_error = first_error.clone();
        let (state, life, ctx) = (state.clone(), life.clone(), ctx.clone());
        thread::spawn(move || {
            let compressed = Arc::new(Latest::<Vec<u8>>::default());
            let decode_worker = {
                let (compressed, state, life, ctx, first_error) = (
                    compressed.clone(),
                    state.clone(),
                    life.clone(),
                    ctx.clone(),
                    first_error.clone(),
                );
                thread::spawn(move || {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                            let mut decoder = FrameDecoder::new()?;
                            let mut rate = (Instant::now(), 0_u64);
                            while let Some(bytes) = compressed.take(&life)? {
                                let start = Instant::now();
                                let frame = decoder.decode(&bytes)?;
                                let mut state = state.lock().unwrap();
                                if state.frame.replace(frame).is_some() {
                                    state.decoded_replaced += 1;
                                }
                                state.frames += 1;
                                state.decode_ms = start.elapsed().as_secs_f64() * 1000.0;
                                state.decoded_at = Some(Instant::now());
                                if rate.0.elapsed() >= Duration::from_secs(1) {
                                    state.decoded_fps = (state.frames - rate.1) as f64
                                        / rate.0.elapsed().as_secs_f64();
                                    rate = (Instant::now(), state.frames);
                                }
                                drop(state);
                                ctx.request_repaint();
                            }
                            Ok(())
                        }));
                    let error = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(format!("Decode: {e:#}")),
                        Err(_) => Some("Decoder worker panicked".into()),
                    };
                    if let Some(error) = error {
                        first_error.lock().unwrap().get_or_insert(error);
                        life.stop();
                    }
                })
            };
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    let mut rate = (Instant::now(), 0_u64);
                    let mut last_frame = Instant::now();
                    video.sock.renew();
                    write_json(&mut video, &true)?;
                    while !life.stopped() {
                        let start = Instant::now();
                        video.sock.renew();
                        let bytes = match read_frame(&mut video) {
                            Ok(Some(bytes)) => bytes,
                            Ok(None) => {
                                let wait = last_frame.elapsed();
                                {
                                    let mut state = state.lock().unwrap();
                                    state.video_heartbeats += 1;
                                    state.capture_wait_ms = wait.as_secs_f64() * 1000.0;
                                    if wait >= Duration::from_secs(1) {
                                        state.status =
                                            "Connected. Waiting for the remote screen capture..."
                                                .into();
                                        state.received_fps = 0.0;
                                        state.decoded_fps = 0.0;
                                    }
                                }
                                video.sock.renew();
                                write_json(&mut video, &true)?;
                                ctx.request_repaint();
                                continue;
                            }
                            Err(error) => {
                                first_error
                                    .lock()
                                    .unwrap()
                                    .get_or_insert(format!("Video frame: {error:#}"));
                                // Only acknowledge a complete terminal error; a partial frame has lost framing.
                                if error.is::<RemoteVideoError>() {
                                    let _ = write_json(&mut video, &true);
                                }
                                return Err(error.context("Video frame"));
                            }
                        };
                        // This thread owns TLS; decoding runs independently with one replaceable JPEG slot.
                        if !compressed.publish(bytes) {
                            break;
                        }
                        last_frame = Instant::now();
                        video.sock.renew();
                        write_json(&mut video, &true)?;
                        let mut state = state.lock().unwrap();
                        state.received += 1;
                        state.capture_wait_ms = 0.0;
                        state.status = "Connected. Click the remote screen to take control.".into();
                        state.receive_ms = start.elapsed().as_secs_f64() * 1000.0;
                        state.frame_roundtrip_ms = state.receive_ms;
                        state.compressed_replaced = compressed.replaced();
                        if rate.0.elapsed() >= Duration::from_secs(1) {
                            state.received_fps =
                                (state.received - rate.1) as f64 / rate.0.elapsed().as_secs_f64();
                            rate = (Instant::now(), state.received);
                        }
                    }
                    Ok(())
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Video receiver worker panicked")));
            if let Err(e) = &result {
                first_error.lock().unwrap().get_or_insert(format!("{e:#}"));
            }
            life.stop();
            compressed.close(None);
            if decode_worker.join().is_err() {
                first_error
                    .lock()
                    .unwrap()
                    .get_or_insert("Decoder worker panicked".into());
            }
            result
        })
    };
    let result = (|| -> Result<()> {
        while !life.stopped() {
            let batch = queue.batch();
            if life.stopped() {
                break;
            }
            let start = Instant::now();
            control.sock.renew();
            write_json(&mut control, &batch)?;
            let ack: std::result::Result<(), String> =
                read_json(&mut control).context("Input acknowledgment")?;
            ack.map_err(anyhow::Error::msg)?;
            state.lock().unwrap().input_rtt_ms = start.elapsed().as_secs_f64() * 1000.0;
            ctx.request_repaint();
        }
        Ok(())
    })();
    if let Err(e) = &result {
        first_error.lock().unwrap().get_or_insert(format!("{e:#}"));
    }
    life.stop();
    let video_result = video_worker
        .join()
        .map_err(|_| anyhow::anyhow!("Video worker panicked"))?;
    if let Some(error) = first_error.lock().unwrap().take() {
        anyhow::bail!("{error}");
    }
    video_result?;
    result
}

pub struct FrameDecoder {
    decoder: turbojpeg::Decompressor,
    rgba: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Result<Self> {
        let mut decoder = turbojpeg::Decompressor::new()?;
        decoder.set_scan_limit(16)?;
        Ok(Self {
            decoder,
            rgba: Vec::new(),
        })
    }

    pub fn decode(&mut self, bytes: &[u8]) -> Result<egui::ColorImage> {
        self.decode_bounded(bytes, 3840, 2160)
    }

    pub fn decode_bounded(
        &mut self,
        bytes: &[u8],
        max_width: usize,
        max_height: usize,
    ) -> Result<egui::ColorImage> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_FRAME,
            "Invalid frame size"
        );
        let header = self.decoder.read_header(bytes)?;
        ensure!(
            header.width > 0
                && header.height > 0
                && header.width <= max_width
                && header.height <= max_height,
            "Frame dimensions exceed limit"
        );
        ensure!(
            !header.is_lossless && !header.is_arithmetic && !header.is_progressive,
            "Only baseline JPEG is accepted"
        );
        let length = header
            .width
            .checked_mul(header.height)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("Frame allocation overflow"))?;
        ensure!(length <= 32 * 1024 * 1024, "Frame allocation exceeds limit");
        self.rgba.resize(length, 0);
        self.decoder.decompress(
            bytes,
            turbojpeg::Image {
                pixels: &mut self.rgba,
                width: header.width,
                height: header.height,
                pitch: header.width * 4,
                format: turbojpeg::PixelFormat::RGBA,
            },
        )?;
        // JPEG has no alpha; TurboJPEG RGBA output is opaque, so premultiplication is unnecessary.
        Ok(egui::ColorImage::from_rgba_premultiplied(
            [header.width, header.height],
            &self.rgba,
        ))
    }
}

#[cfg(test)]
pub fn decode_frame(bytes: &[u8]) -> Result<egui::ColorImage> {
    FrameDecoder::new()?.decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moves_coalesce_without_crossing_clicks_and_release_clears_backlog() {
        let queue = InputQueue::default();
        queue.push(Input::Move { x: 0.1, y: 0.1 }).unwrap();
        queue.push(Input::Move { x: 0.2, y: 0.2 }).unwrap();
        queue
            .push(Input::Button {
                button: Button::Left,
                pressed: true,
            })
            .unwrap();
        queue.push(Input::Move { x: 0.3, y: 0.3 }).unwrap();
        assert_eq!(queue.batch().len(), 3);
        queue
            .push(Input::Key {
                name: "Enter".into(),
                pressed: true,
            })
            .unwrap();
        queue.push(Input::ReleaseAll).unwrap();
        assert!(matches!(queue.batch().as_slice(), [Input::ReleaseAll]));
    }
    #[test]
    fn invalid_jpeg_rejected() {
        assert!(decode_frame(b"not a JPEG").is_err());
    }

    #[test]
    fn remote_video_failure_survives_control_socket_shutdown() {
        use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
        use std::net::TcpListener;
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
        let server = thread::spawn(move || {
            let mut control =
                transport::accept(listener.accept().unwrap().0, config.clone()).unwrap();
            let _: Hello = read_json(&mut control).unwrap();
            write_json(
                &mut control,
                &Ok::<_, String>(Welcome {
                    session: "b".repeat(64),
                    display: DisplayInfo {
                        name: "Test Mac".into(),
                        mac: true,
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                    },
                }),
            )
            .unwrap();
            let mut video = transport::accept(listener.accept().unwrap().0, config).unwrap();
            let _: Hello = read_json(&mut video).unwrap();
            assert!(read_json::<bool>(&mut video).unwrap());
            write_frame_error(&mut video, "Screen Recording permission was revoked").unwrap();
            assert!(read_json::<bool>(&mut video).unwrap());
        });
        let viewer = Viewer::start(ticket, egui::Context::default());
        let until = Instant::now() + Duration::from_secs(5);
        while viewer.connected() {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(10));
        }
        // connected() may flip before the worker publishes its final diagnostic.
        loop {
            if viewer
                .state
                .lock()
                .unwrap()
                .status
                .contains("Screen Recording")
            {
                break;
            }
            assert!(
                Instant::now() < until,
                "{}",
                viewer.state.lock().unwrap().status
            );
            thread::sleep(Duration::from_millis(10));
        }
        server.join().unwrap();
    }

    #[test]
    fn viewer_keeps_input_alive_through_video_idle_and_resumes_decoding() {
        use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
        use std::{
            net::TcpListener,
            sync::atomic::{AtomicU64, Ordering},
        };
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
        let input_acks = Arc::new(AtomicU64::new(0));
        let acks = input_acks.clone();
        let server = thread::spawn(move || {
            let mut control =
                transport::accept(listener.accept().unwrap().0, config.clone()).unwrap();
            let _: Hello = read_json(&mut control).unwrap();
            write_json(
                &mut control,
                &Ok::<_, String>(Welcome {
                    session: "b".repeat(64),
                    display: DisplayInfo {
                        name: "Slow Mac".into(),
                        mac: true,
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                    },
                }),
            )
            .unwrap();
            let control_worker = thread::spawn(move || {
                loop {
                    control.sock.renew();
                    if read_json::<Vec<Input>>(&mut control).is_err() {
                        break;
                    }
                    if write_json(&mut control, &Ok::<(), String>(())).is_err() {
                        break;
                    }
                    acks.fetch_add(1, Ordering::Relaxed);
                }
            });
            let mut video = transport::accept(listener.accept().unwrap().0, config).unwrap();
            let _: Hello = read_json(&mut video).unwrap();
            assert!(read_json::<bool>(&mut video).unwrap());
            for _ in 0..15 {
                thread::sleep(Duration::from_millis(250));
                video.sock.renew();
                write_frame_pending(&mut video).unwrap();
                assert!(read_json::<bool>(&mut video).unwrap());
            }
            let rgb = image::RgbImage::from_pixel(8, 4, image::Rgb([25, 45, 80]));
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new(&mut bytes)
                .encode_image(&rgb)
                .unwrap();
            for _ in 0..4 {
                video.sock.renew();
                write_frame(&mut video, &bytes).unwrap();
                assert!(read_json::<bool>(&mut video).unwrap());
            }
            video.sock.renew();
            while read_json::<bool>(&mut video).is_ok() {}
            control_worker.join().unwrap();
        });
        let viewer = Viewer::start(ticket, egui::Context::default());
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let state = viewer.state.lock().unwrap();
            if state.video_heartbeats >= 13 {
                assert_eq!(state.received, 0);
                assert_eq!(state.frames, 0);
                assert!(state.capture_wait_ms >= 3000.0);
                assert!(state.status.contains("Waiting for the remote screen"));
                break;
            }
            drop(state);
            assert!(viewer.connected());
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(10));
        }
        while viewer.state.lock().unwrap().frames == 0 {
            assert!(
                viewer.connected(),
                "{}",
                viewer.state.lock().unwrap().status
            );
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(input_acks.load(Ordering::Relaxed) >= 8);
        assert!(
            viewer
                .state
                .lock()
                .unwrap()
                .status
                .starts_with("Connected. Click")
        );
        assert_eq!(viewer.state.lock().unwrap().capture_wait_ms, 0.0);
        viewer.stop();
        drop(viewer);
        server.join().unwrap();
    }

    #[test]
    fn jpeg_decode_roundtrip_and_dimension_limit() {
        let rgb = image::RgbImage::from_pixel(4, 3, image::Rgb([20, 80, 100]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode_image(&rgb)
            .unwrap();
        assert_eq!(decode_frame(&bytes).unwrap().size, [4, 3]);
        let large = image::RgbImage::new(3841, 1);
        bytes.clear();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode_image(&large)
            .unwrap();
        assert!(decode_frame(&bytes).is_err());
        let mut compressor = turbojpeg::Compressor::new().unwrap();
        compressor.set_quality(70).unwrap();
        compressor.set_subsamp(turbojpeg::Subsamp::None).unwrap();
        compressor.set_progressive(true).unwrap();
        let jpeg = compressor
            .compress_to_vec(turbojpeg::Image {
                pixels: &[20; 48],
                width: 4,
                height: 4,
                pitch: 12,
                format: turbojpeg::PixelFormat::RGB,
            })
            .unwrap();
        assert!(decode_frame(&jpeg).is_err());
    }
}
