use eframe::egui;

pub fn app_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../packaging/icons/icon-256.png"))
        .expect("The bundled Localnet icon must be a valid PNG")
        .to_rgba8();
    egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

pub fn mark(ctx: &egui::Context) -> egui::TextureHandle {
    let image = image::load_from_memory(include_bytes!("../packaging/icons/mark.png"))
        .expect("The bundled Localnet mark must be a valid PNG")
        .to_rgba8();
    ctx.load_texture(
        "localnet-mark",
        egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            &image,
        ),
        egui::TextureOptions::LINEAR,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_icon_has_transparent_corners_and_opaque_center() {
        let icon = app_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba[3], 0);
        assert_eq!(icon.rgba[(128 * 256 + 128) * 4 + 3], 255);
    }

    #[test]
    fn native_icon_containers_have_valid_sizes_and_png_payloads() {
        let ico = include_bytes!("../packaging/icons/localnet.ico");
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        let count = u16::from_le_bytes(ico[4..6].try_into().unwrap()) as usize;
        assert_eq!(count, 7);
        for entry in ico[6..6 + count * 16].chunks_exact(16) {
            let size = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
            let image = image::load_from_memory(&ico[offset..offset + size]).unwrap();
            let width = if entry[0] == 0 {
                256
            } else {
                u32::from(entry[0])
            };
            assert_eq!(image.width(), width);
            assert_eq!(image.height(), width);
        }
        let icns = include_bytes!("../packaging/icons/localnet.icns");
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize,
            icns.len()
        );
        let mut offset = 8;
        let mut chunks = 0;
        while offset < icns.len() {
            let size =
                u32::from_be_bytes(icns[offset + 4..offset + 8].try_into().unwrap()) as usize;
            assert!(size > 8 && offset + size <= icns.len());
            let image = image::load_from_memory(&icns[offset + 8..offset + size]).unwrap();
            assert_eq!(image.width(), image.height());
            offset += size;
            chunks += 1;
        }
        assert_eq!(offset, icns.len());
        assert_eq!(chunks, 11);
    }
}
