use crate::{
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
            let result = (|| -> Result<()> {
                let mut decoder = FrameDecoder::new()?;
                video.sock.renew();
                write_json(&mut video, &true)?;
                while !life.stopped() {
                    let start = Instant::now();
                    video.sock.renew();
                    let bytes = match read_frame(&mut video) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            first_error
                                .lock()
                                .unwrap()
                                .get_or_insert(format!("Video frame: {error:#}"));
                            let _ = write_json(&mut video, &true);
                            return Err(error.context("Video frame"));
                        }
                    };
                    // Finish decoding before requesting again so a terminal error can be acknowledged explicitly.
                    let frame = decoder.decode(&bytes)?;
                    write_json(&mut video, &true)?;
                    let mut state = state.lock().unwrap();
                    state.frame = Some(frame);
                    state.frames += 1;
                    state.frame_roundtrip_ms = start.elapsed().as_secs_f64() * 1000.0;
                    drop(state);
                    ctx.request_repaint();
                }
                Ok(())
            })();
            if let Err(e) = &result {
                first_error.lock().unwrap().get_or_insert(format!("{e:#}"));
            }
            life.stop();
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
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_FRAME,
            "Invalid frame size"
        );
        let header = self.decoder.read_header(bytes)?;
        ensure!(
            header.width > 0 && header.height > 0 && header.width <= 3840 && header.height <= 2160,
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
