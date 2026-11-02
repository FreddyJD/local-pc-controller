use crate::{
    capture,
    discovery::{Device, fetch_preview},
    protocol::{MAX_PREVIEW, PREVIEW_HEIGHT, PREVIEW_WIDTH},
    transport::Lifetime,
    viewer::FrameDecoder,
};
use anyhow::Result;
use eframe::egui;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const REFRESH: Duration = Duration::from_secs(5);

type CachedScreenshot = Result<Arc<Vec<u8>>, String>;

// One global capture and one cached result (including failures) shared by all preview requesters.
pub struct PreviewCache {
    latest: Mutex<Option<(Instant, CachedScreenshot)>>,
    capturing: Mutex<()>,
    capture: fn() -> Result<Vec<u8>>,
}

impl Default for PreviewCache {
    fn default() -> Self {
        Self::new(capture::thumbnail)
    }
}

impl PreviewCache {
    pub fn new(capture: fn() -> Result<Vec<u8>>) -> Self {
        Self {
            latest: Mutex::new(None),
            capturing: Mutex::new(()),
            capture,
        }
    }

    pub fn get(&self) -> CachedScreenshot {
        let cache = self.latest.lock().unwrap();
        if let Some((updated, result)) = &*cache
            && updated.elapsed() < REFRESH
        {
            return result.clone();
        }
        drop(cache);
        let _capture = self
            .capturing
            .try_lock()
            .map_err(|_| "Screenshot preview is refreshing; retry shortly".to_owned())?;
        // Do not hold the cached-image mutex during an OS capture call.
        if let Some((updated, result)) = &*self.latest.lock().unwrap()
            && updated.elapsed() < REFRESH
        {
            return result.clone();
        }
        let result = (self.capture)()
            .and_then(|bytes| {
                anyhow::ensure!(
                    !bytes.is_empty() && bytes.len() <= MAX_PREVIEW,
                    "Invalid screenshot preview size"
                );
                Ok(Arc::new(bytes))
            })
            .map_err(|e| format!("{e:#}"));
        *self.latest.lock().unwrap() = Some((Instant::now(), result.clone()));
        result
    }
}

pub struct Thumbnail {
    pub device: Device,
    pub image: Option<egui::ColorImage>,
    pub error: Option<String>,
    pub updated: Option<Instant>,
    next: Instant,
}

struct Work {
    device: Device,
    life: Arc<Lifetime>,
}
struct Completed {
    device: Device,
    image: Result<egui::ColorImage, String>,
}

pub struct Previews {
    pub thumbnails: HashMap<SocketAddr, Thumbnail>,
    sender: Option<SyncSender<Work>>,
    receiver: Receiver<Completed>,
    running: Option<Arc<Lifetime>>,
    worker: Option<JoinHandle<()>>,
}

fn same_device(a: &Device, b: &Device) -> bool {
    a.address == b.address && a.certificate == b.certificate && a.version == b.version
}

impl Previews {
    pub fn new() -> Self {
        let (sender, jobs) = mpsc::sync_channel::<Work>(1);
        let (done, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let mut decoder = FrameDecoder::new().ok();
            while let Ok(work) = jobs.recv() {
                let result = fetch_preview(&work.device, &work.life)
                    .and_then(|bytes| {
                        decoder
                            .as_mut()
                            .ok_or_else(|| anyhow::anyhow!("Preview decoder unavailable"))?
                            .decode_bounded(&bytes, PREVIEW_WIDTH as usize, PREVIEW_HEIGHT as usize)
                    })
                    .map_err(|e| format!("{e:#}"));
                work.life.stop();
                if done
                    .try_send(Completed {
                        device: work.device,
                        image: result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            thumbnails: HashMap::new(),
            sender: Some(sender),
            receiver,
            running: None,
            worker: Some(worker),
        }
    }

    pub fn cancel(&mut self) {
        if let Some(life) = &self.running {
            life.stop();
        }
    }

    pub fn tick(&mut self, devices: &[Device], enabled: bool) {
        self.thumbnails
            .retain(|_, thumb| devices.iter().any(|d| same_device(d, &thumb.device)));
        for device in devices.iter().filter(|d| d.compatible()).take(64) {
            self.thumbnails
                .entry(device.address)
                .or_insert_with(|| Thumbnail {
                    device: device.clone(),
                    image: None,
                    error: None,
                    updated: None,
                    next: Instant::now(),
                });
        }
        while let Ok(done) = self.receiver.try_recv() {
            self.running = None;
            if let Some(thumb) = self.thumbnails.get_mut(&done.device.address)
                && same_device(&thumb.device, &done.device)
            {
                let success = done.image.is_ok();
                match done.image {
                    Ok(image) => {
                        thumb.image = Some(image);
                        thumb.error = None;
                        thumb.updated = Some(Instant::now());
                    }
                    Err(error) => thumb.error = Some(error),
                }
                thumb.next = Instant::now()
                    + if success {
                        REFRESH
                    } else {
                        Duration::from_secs(15)
                    };
            }
        }
        if !enabled {
            self.cancel();
            return;
        }
        if self.running.is_none() {
            let candidate = self
                .thumbnails
                .values()
                .filter(|thumb| thumb.next <= Instant::now())
                .min_by_key(|thumb| thumb.next);
            if let Some(thumb) = candidate {
                let life = Arc::new(Lifetime::default());
                if self
                    .sender
                    .as_ref()
                    .unwrap()
                    .try_send(Work {
                        device: thumb.device.clone(),
                        life: life.clone(),
                    })
                    .is_ok()
                {
                    self.running = Some(life);
                }
            }
        }
    }
}

impl Drop for Previews {
    fn drop(&mut self) {
        self.cancel();
        self.sender.take();
        // The result mailbox uses try_send; shutdown never waits for the UI to consume a result.
        while self.receiver.try_recv().is_ok() {}
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CAPTURES: AtomicUsize = AtomicUsize::new(0);
    fn fake_capture() -> Result<Vec<u8>> {
        CAPTURES.fetch_add(1, Ordering::SeqCst);
        Ok(vec![1])
    }
    fn fail_capture() -> Result<Vec<u8>> {
        anyhow::bail!("Screen Recording permission required")
    }

    #[test]
    fn previews_capture_only_on_demand_and_cache_successes_and_errors() {
        let cache = PreviewCache::new(fake_capture);
        assert_eq!(CAPTURES.load(Ordering::SeqCst), 0);
        assert!(Arc::ptr_eq(&cache.get().unwrap(), &cache.get().unwrap()));
        assert_eq!(CAPTURES.load(Ordering::SeqCst), 1);
        let cache = PreviewCache::new(fail_capture);
        assert!(cache.get().unwrap_err().contains("Screen Recording"));
        assert!(cache.latest.lock().unwrap().is_some());
    }

    #[test]
    fn preview_decoder_checks_thumbnail_bounds_before_allocation() {
        let image = image::RgbImage::new(481, 270);
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode_image(&image)
            .unwrap();
        assert!(
            FrameDecoder::new()
                .unwrap()
                .decode_bounded(&bytes, 480, 270)
                .is_err()
        );
    }

    #[test]
    fn removed_or_changed_devices_discard_old_results() {
        let mut previews = Previews::new();
        let original = Device {
            name: "PC".into(),
            address: "127.0.0.1:1".parse().unwrap(),
            certificate: vec![1],
            version: crate::protocol::VERSION,
        };
        previews.tick(std::slice::from_ref(&original), false);
        assert_eq!(previews.thumbnails.len(), 1);
        let mut changed = original.clone();
        changed.certificate = vec![2];
        previews.tick(std::slice::from_ref(&changed), false);
        assert_eq!(
            previews.thumbnails[&changed.address].device.certificate,
            vec![2]
        );
        previews.tick(&[], false);
        assert!(previews.thumbnails.is_empty());
    }
}
