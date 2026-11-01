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
    #[cfg(target_os = "macos")]
    if !objc2_core_graphics::CGPreflightScreenCaptureAccess() {
        objc2_core_graphics::CGRequestScreenCaptureAccess();
        anyhow::bail!(
            "Enable Localnet in System Settings > Privacy & Security > Screen & System Audio Recording, then restart the app."
        );
    }
    let _ = enigo::Enigo::new(&enigo::Settings {
        open_prompt_to_get_permissions: true, ..Default::default()
    }).map_err(|e| anyhow::anyhow!("Input permission: {e}. On Mac, enable Localnet in Privacy & Security > Accessibility, then restart."))?;
    Ok(())
}
