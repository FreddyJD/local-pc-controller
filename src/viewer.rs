use crate::{
    protocol::*,
    transport::{self, Lifetime},
};
use anyhow::{Result, ensure};
use eframe::egui;
use std::{
    collections::VecDeque,
    io::Cursor,
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
}

impl Viewer {
    #[cfg(test)]
    pub(crate) fn test_session() -> Self {
        Self {
            state: Arc::new(Mutex::new(ViewerState::default())),
            life: Arc::new(Lifetime::default()),
            queue: Arc::new(InputQueue::default()),
            worker: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_inputs(&self) -> Vec<Input> {
        self.queue.batch()
    }

    pub fn start(pair: Pairing, ctx: egui::Context) -> Self {
        let state = Arc::new(Mutex::new(ViewerState {
            status: "Connecting over encrypted LAN...".into(),
            ..Default::default()
        }));
        let life = Arc::new(Lifetime::default());
        let queue = Arc::new(InputQueue::default());
        let worker = {
            let (state, life, queue) = (state.clone(), life.clone(), queue.clone());
            thread::spawn(move || {
                let result = session(pair, &state, &life, &queue, &ctx);
                life.stop();
                let mut state = state.lock().unwrap();
                state.status = match result {
                    Ok(_) => "Disconnected".into(),
                    Err(e) => format!("Disconnected: {e}"),
                };
                ctx.request_repaint();
            })
        };
        Self {
            state,
            life,
            queue,
            worker: Some(worker),
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
    pair: Pairing,
    state: &Arc<Mutex<ViewerState>>,
    life: &Arc<Lifetime>,
    queue: &Arc<InputQueue>,
    ctx: &egui::Context,
) -> Result<()> {
    let config = transport::client_config(&pair)?;
    let mut control = transport::connect(&pair, config.clone(), life)?;
    control.sock.renew();
    write_json(
        &mut control,
        &Hello {
            version: VERSION,
            token: pair.token.clone(),
            channel: Channel::Control,
        },
    )?;
    let welcome: Welcome = read_json(&mut control)?;
    ensure!(
        welcome.session.len() == 64 && welcome.display.width > 0 && welcome.display.height > 0,
        "Invalid host metadata"
    );
    state.lock().unwrap().display = Some(welcome.display);
    let mut video = transport::connect(&pair, config, life)?;
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
    let video_worker = {
        let (state, life, ctx) = (state.clone(), life.clone(), ctx.clone());
        thread::spawn(move || {
            let result = (|| -> Result<()> {
                while !life.stopped() {
                    let start = Instant::now();
                    video.sock.renew();
                    write_json(&mut video, &true)?;
                    let bytes = read_packet(&mut video, MAX_FRAME)?;
                    let frame = decode_frame(&bytes)?;
                    let mut state = state.lock().unwrap();
                    state.frame = Some(frame);
                    state.frames += 1;
                    state.frame_roundtrip_ms = start.elapsed().as_secs_f64() * 1000.0;
                    drop(state);
                    ctx.request_repaint();
                }
                Ok(())
            })();
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
            let ack: bool = read_json(&mut control)?;
            ensure!(ack, "Host rejected input");
            state.lock().unwrap().input_rtt_ms = start.elapsed().as_secs_f64() * 1000.0;
            ctx.request_repaint();
        }
        Ok(())
    })();
    life.stop();
    let video_result = video_worker
        .join()
        .map_err(|_| anyhow::anyhow!("Video worker panicked"))?;
    video_result?;
    result
}

pub fn decode_frame(bytes: &[u8]) -> Result<egui::ColorImage> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(3840);
    limits.max_image_height = Some(2160);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.to_rgba8();
    ensure!(image.width() > 0 && image.height() > 0, "Empty video frame");
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        &image,
    ))
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
    }
}
