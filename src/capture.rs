use crate::protocol::DisplayInfo;
use anyhow::{Result, ensure};
use image::{DynamicImage, codecs::jpeg::JpegEncoder, imageops::FilterType};
use xcap::Monitor;

pub fn primary() -> Result<Monitor> {
    for monitor in Monitor::all()? {
        if monitor.is_primary()? {
            return Ok(monitor);
        }
    }
    anyhow::bail!("No primary display found")
}

pub fn info(monitor: &Monitor) -> Result<DisplayInfo> {
    let info = DisplayInfo {
        name: monitor.name()?,
        mac: cfg!(target_os = "macos"),
        x: monitor.x()?,
        y: monitor.y()?,
        width: monitor.width()?,
        height: monitor.height()?,
    };
    ensure!(info.width > 0 && info.height > 0, "Display has zero size");
    Ok(info)
}

pub fn jpeg(monitor: &Monitor, max_width: u32, quality: u8) -> Result<Vec<u8>> {
    let image = DynamicImage::ImageRgba8(monitor.capture_image()?);
    let image = if image.width() > max_width || image.height() > 2160 {
        image.resize(max_width, 2160, FilterType::Triangle)
    } else {
        image
    };
    let rgb = image.to_rgb8();
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality).encode_image(&rgb)?;
    Ok(bytes)
}

pub fn request_permissions() -> Result<()> {
    permissions(true)
}

pub fn check_permissions() -> Result<()> {
    permissions(false)
}

fn permissions(prompt: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    if !objc2_core_graphics::CGPreflightScreenCaptureAccess() {
        if prompt {
            objc2_core_graphics::CGRequestScreenCaptureAccess();
        }
        anyhow::bail!(
            "Enable Localnet in System Settings > Privacy & Security > Screen & System Audio Recording, then restart the app."
        );
    }
    let _ = enigo::Enigo::new(&enigo::Settings {
        open_prompt_to_get_permissions: prompt, ..Default::default()
    }).map_err(|e| anyhow::anyhow!("Input permission: {e}. On Mac, enable Localnet in Privacy & Security > Accessibility, then restart."))?;
    Ok(())
}

pub struct VideoCapture {
    monitor: Monitor,
    compressor: turbojpeg::Compressor,
    output: turbojpeg::OutputBuf<'static>,
    width: u32,
    #[cfg(target_os = "windows")]
    dxgi: Option<windows_capture::dxgi_duplication_api::DxgiDuplicationApi>,
    #[cfg(target_os = "windows")]
    staging: Option<windows_capture::d3d11::StagingTexture>,
}

impl VideoCapture {
    pub fn new(width: u32, quality: u8) -> Result<Self> {
        let monitor = primary()?;
        let mut compressor = turbojpeg::Compressor::new()?;
        compressor.set_quality(i32::from(quality))?;
        compressor.set_subsamp(turbojpeg::Subsamp::None)?;
        #[cfg(target_os = "windows")]
        let dxgi = windows_capture::monitor::Monitor::primary()
            .ok()
            .and_then(|monitor| {
                windows_capture::dxgi_duplication_api::DxgiDuplicationApi::new_options(
                    monitor,
                    &[windows_capture::dxgi_duplication_api::DxgiDuplicationFormat::Bgra8],
                )
                .ok()
            });
        #[cfg(target_os = "windows")]
        let staging = dxgi.as_ref().and_then(|dxgi| {
            windows_capture::d3d11::StagingTexture::new(
                dxgi.device(),
                dxgi.width(),
                dxgi.height(),
                dxgi.duplication_desc().ModeDesc.Format,
            )
            .ok()
        });
        Ok(Self {
            monitor,
            compressor,
            output: turbojpeg::OutputBuf::new_owned(),
            width,
            #[cfg(target_os = "windows")]
            dxgi,
            #[cfg(target_os = "windows")]
            staging,
        })
    }

    pub fn display(&self) -> Result<DisplayInfo> {
        info(&self.monitor)
    }

    pub fn frame(&mut self) -> Result<&[u8]> {
        #[cfg(target_os = "windows")]
        if let Some(dxgi) = &mut self.dxgi {
            let result = (|| -> Result<()> {
                let mut frame = dxgi.acquire_next_frame(1)?;
                let mut buffer = if let Some(staging) = &mut self.staging {
                    frame.buffer_with(staging)?
                } else {
                    frame.buffer()?
                };
                let width = buffer.width();
                let height = buffer.height();
                let pitch = buffer.row_pitch() as usize;
                if width <= self.width && height <= 2160 {
                    self.compressor.compress(
                        turbojpeg::Image {
                            pixels: buffer.as_raw_buffer(),
                            width: width as usize,
                            height: height as usize,
                            pitch,
                            format: turbojpeg::PixelFormat::BGRA,
                        },
                        &mut self.output,
                    )?;
                } else {
                    let packed = pack_bgra(buffer.as_raw_buffer(), width, height, pitch)?;
                    let image = image::RgbaImage::from_raw(width, height, packed)
                        .ok_or_else(|| anyhow::anyhow!("Invalid captured frame"))?;
                    let resized = DynamicImage::ImageRgba8(image)
                        .resize(self.width, 2160, FilterType::Triangle)
                        .to_rgba8();
                    self.compressor.compress(
                        turbojpeg::Image {
                            pixels: &resized,
                            width: resized.width() as usize,
                            height: resized.height() as usize,
                            pitch: resized.width() as usize * 4,
                            format: turbojpeg::PixelFormat::RGBA,
                        },
                        &mut self.output,
                    )?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => return Ok(&self.output),
                Err(e)
                    if matches!(
                        e.downcast_ref::<windows_capture::dxgi_duplication_api::Error>(),
                        Some(windows_capture::dxgi_duplication_api::Error::Timeout)
                    ) && !self.output.is_empty() =>
                {
                    return Ok(&self.output);
                }
                Err(e)
                    if matches!(
                        e.downcast_ref::<windows_capture::dxgi_duplication_api::Error>(),
                        Some(windows_capture::dxgi_duplication_api::Error::Timeout)
                    ) => {}
                Err(_) => {
                    self.dxgi = None;
                    self.staging = None;
                }
            }
        }
        let image = DynamicImage::ImageRgba8(self.monitor.capture_image()?);
        let image = if image.width() > self.width || image.height() > 2160 {
            image.resize(self.width, 2160, FilterType::Triangle)
        } else {
            image
        };
        let rgba = image.to_rgba8();
        self.compressor.compress(
            turbojpeg::Image {
                pixels: &rgba,
                width: rgba.width() as usize,
                height: rgba.height() as usize,
                pitch: rgba.width() as usize * 4,
                format: turbojpeg::PixelFormat::RGBA,
            },
            &mut self.output,
        )?;
        Ok(&self.output)
    }
}

#[cfg(any(target_os = "windows", test))]
fn pack_bgra(bytes: &[u8], width: u32, height: u32, pitch: usize) -> Result<Vec<u8>> {
    let row = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("Frame overflow"))?;
    let total = pitch
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow::anyhow!("Frame overflow"))?;
    ensure!(
        width > 0 && height > 0 && pitch >= row && bytes.len() >= total,
        "Invalid frame stride"
    );
    let mut rgba = Vec::with_capacity(row * height as usize);
    for line in bytes.chunks(pitch).take(height as usize) {
        for pixel in line[..row].chunks_exact(4) {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_bgra_rows_are_converted_without_padding() {
        assert_eq!(
            pack_bgra(
                &[1, 2, 3, 0, 99, 99, 99, 99, 4, 5, 6, 0, 99, 99, 99, 99],
                1,
                2,
                8
            )
            .unwrap(),
            vec![3, 2, 1, 255, 6, 5, 4, 255]
        );
        assert!(pack_bgra(&[0; 4], 1, 2, 8).is_err());
    }

    #[test]
    #[ignore = "Captures local interactive desktop to memory only; release benchmark, no input injection"]
    fn benchmark_capture_and_encoding() {
        let monitor = primary().unwrap();
        let start = std::time::Instant::now();
        let image = monitor.capture_image().unwrap();
        eprintln!(
            "Screenshot capture: {:.2} ms; {}x{}",
            start.elapsed().as_secs_f64() * 1000.0,
            image.width(),
            image.height()
        );
        let rgb = DynamicImage::ImageRgba8(image.clone()).to_rgb8();
        let mut baseline = Vec::new();
        let start = std::time::Instant::now();
        for _ in 0..10 {
            baseline.clear();
            JpegEncoder::new_with_quality(&mut baseline, 70)
                .encode_image(&rgb)
                .unwrap();
        }
        eprintln!(
            "Original JPEG encode average: {:.2} ms",
            start.elapsed().as_secs_f64() * 100.0
        );
        let mut compressor = turbojpeg::Compressor::new().unwrap();
        compressor.set_quality(70).unwrap();
        compressor.set_subsamp(turbojpeg::Subsamp::None).unwrap();
        let mut output = turbojpeg::OutputBuf::new_owned();
        let start = std::time::Instant::now();
        for _ in 0..10 {
            compressor
                .compress(
                    turbojpeg::Image {
                        pixels: &image,
                        width: image.width() as usize,
                        height: image.height() as usize,
                        pitch: image.width() as usize * 4,
                        format: turbojpeg::PixelFormat::RGBA,
                    },
                    &mut output,
                )
                .unwrap();
        }
        eprintln!(
            "TurboJPEG encode average: {:.2} ms",
            start.elapsed().as_secs_f64() * 100.0
        );
        for _ in 0..3 {
            let mut decoder = crate::viewer::FrameDecoder::new().unwrap();
            let mut capture = VideoCapture::new(1920, 70).unwrap();
            #[cfg(target_os = "windows")]
            eprintln!("Native DXGI available: {}", capture.dxgi.is_some());
            for _ in 0..3 {
                let start = std::time::Instant::now();
                let frame = capture.frame().unwrap();
                let capture_ms = start.elapsed().as_secs_f64() * 1000.0;
                let start = std::time::Instant::now();
                decoder.decode(frame).unwrap();
                eprintln!(
                    "Capture+encode: {capture_ms:.2} ms; decode: {:.2} ms; {} bytes",
                    start.elapsed().as_secs_f64() * 1000.0,
                    frame.len()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}
