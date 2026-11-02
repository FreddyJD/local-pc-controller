use crate::{
    discovery::{Device, Discovery, Outgoing},
    host::VideoSettings,
    input::named_key,
    preview::Previews,
    protocol::{Button, Input, Modifiers, SessionTicket, is_local},
    viewer::Viewer,
};
use eframe::egui::{self, Color32, Event, Rect, TextureHandle, TextureOptions};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

const MAX_SESSIONS: usize = 4;

struct RemoteSession {
    name: String,
    address: SocketAddr,
    viewer: Viewer,
    texture: Option<TextureHandle>,
}

struct TileTexture {
    texture: TextureHandle,
    certificate: Vec<u8>,
}

pub struct Localnet {
    pub chrome: crate::chrome::Chrome,
    interfaces: Vec<(String, IpAddr)>,
    interface: usize,
    settings: VideoSettings,
    sessions: Vec<RemoteSession>,
    active: Option<usize>,
    error: String,
    remote_rect: Option<Rect>,
    controlling: bool,
    events: Vec<Event>,
    modifiers: egui::Modifiers,
    focused: bool,
    wheel: egui::Vec2,
    last_modifiers: Modifiers,
    discovery: Option<Discovery>,
    outgoing: Option<Outgoing>,
    gallery: bool,
    settings_open: bool,
    previews: Option<Previews>,
    thumbnails: HashMap<SocketAddr, TileTexture>,
    logo: egui::TextureHandle,
}

impl Localnet {
    pub fn new(ctx: &egui::Context) -> Self {
        ctx.set_theme(egui::Theme::Dark);
        ctx.set_visuals(egui::Visuals::dark());
        ctx.global_style_mut(|s| {
            s.text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
            s.text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
            s.text_styles
                .insert(egui::TextStyle::Heading, egui::FontId::proportional(24.0));
            s.spacing.item_spacing = egui::vec2(12.0, 12.0);
            s.spacing.button_padding = egui::vec2(12.0, 7.0);
            s.visuals.panel_fill = Color32::from_rgb(20, 21, 24);
            s.visuals.window_fill = Color32::from_rgb(26, 27, 31);
            s.visuals.override_text_color = Some(Color32::from_rgb(224, 225, 230));
            s.visuals.selection.bg_fill = Color32::from_rgb(76, 80, 145);
            s.visuals.widgets.inactive.bg_fill = Color32::from_rgb(34, 35, 40);
            s.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(30, 31, 36);
            s.visuals.widgets.inactive.bg_stroke =
                egui::Stroke::new(1.0, Color32::from_rgb(52, 53, 61));
            s.visuals.widgets.hovered.bg_fill = Color32::from_rgb(47, 48, 57);
            s.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(42, 43, 51);
        });
        // Load installed platform UI fonts, without bundling or downloading licensed system fonts.
        let fonts = if cfg!(target_os = "windows") {
            vec!["C:/Windows/Fonts/segoeui.ttf"]
        } else {
            vec![
                "/System/Library/Fonts/SFNS.ttf",
                "/System/Library/Fonts/Helvetica.ttc",
            ]
        };
        for path in fonts {
            if let Ok(bytes) = std::fs::read(path) {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "system-ui".into(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .insert(0, "system-ui".into());
                ctx.set_fonts(fonts);
                break;
            }
        }
        let mut interfaces: Vec<_> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| is_local(i.ip()) && !i.ip().is_loopback())
            .map(|i| (i.name.clone(), i.ip()))
            .collect();
        interfaces.push((
            "Loopback (this computer only)".into(),
            "127.0.0.1".parse().unwrap(),
        ));
        Self {
            chrome: Default::default(),
            interfaces,
            interface: 0,
            settings: VideoSettings {
                fps: 60,
                width: 1920,
                quality: 70,
            },
            sessions: Vec::new(),
            active: None,
            error: String::new(),
            remote_rect: None,
            controlling: false,
            events: Vec::new(),
            modifiers: Default::default(),
            focused: true,
            wheel: egui::Vec2::ZERO,
            last_modifiers: Modifiers::default(),
            discovery: None,
            outgoing: None,
            gallery: true,
            settings_open: false,
            previews: Some(Previews::new()),
            thumbnails: HashMap::new(),
            logo: crate::branding::mark(ctx),
        }
    }

    pub fn enable_discovery(&mut self) {
        match Discovery::start(self.interfaces[self.interface].1, self.settings) {
            Ok(discovery) => {
                self.discovery = Some(discovery);
                self.error.clear();
            }
            Err(e) => {
                self.error = format!(
                    "Local access unavailable: {e:#}. Check the selected interface and close other Localnet instances."
                )
            }
        }
    }

    fn send(&self, event: Input) {
        if let Some(session) = self.active.and_then(|index| self.sessions.get(index)) {
            session.viewer.send(event);
        }
    }

    fn release(&mut self) {
        self.send(Input::ReleaseAll);
        self.controlling = false;
        self.wheel = egui::Vec2::ZERO;
        self.last_modifiers = Modifiers::default();
    }

    fn sync_modifiers(&mut self, value: egui::Modifiers) {
        let current = remote_modifiers(value);
        if current != self.last_modifiers {
            self.send(Input::Modifiers(current));
            self.last_modifiers = current;
        }
    }

    fn select_session(&mut self, index: usize) {
        if index >= self.sessions.len() || (self.active == Some(index) && !self.gallery) {
            return;
        }
        self.release();
        // Events were captured against the old screen; never replay them into the new session.
        self.events.clear();
        self.remote_rect = None;
        self.active = Some(index);
        self.gallery = false;
    }

    fn disconnect_active(&mut self) {
        self.release();
        self.events.clear();
        self.remote_rect = None;
        if let Some(index) = self.active.take() {
            self.sessions.remove(index);
            self.active = (!self.sessions.is_empty())
                .then_some(index.min(self.sessions.len().saturating_sub(1)));
        }
        self.gallery = true;
    }

    fn show_gallery(&mut self) {
        self.release();
        self.events.clear();
        self.remote_rect = None;
        self.gallery = true;
    }

    fn shutdown(&mut self) {
        self.release();
        self.events.clear();
        self.active = None;
        self.remote_rect = None;
        self.sessions.clear();
        self.outgoing.take();
        self.discovery.take();
        self.previews.take();
        self.thumbnails.clear();
        self.gallery = true;
    }

    fn incoming_viewer(&self) -> bool {
        self.discovery
            .as_ref()
            .is_some_and(|discovery| discovery.controller.state.lock().unwrap().has_viewer)
    }

    fn connect_device(&mut self, device: Device) {
        if !device.compatible() {
            self.error = format!(
                "Update Localnet on {} and this computer to 0.3.0 to use screenshot previews and connect.",
                device.name
            );
            return;
        }
        if let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.address.ip() == device.address.ip())
        {
            if self.sessions[index].viewer.connected() {
                self.select_session(index);
                return;
            }
            self.select_session(index);
            self.disconnect_active();
        }
        if self.outgoing.is_some() || self.sessions.len() >= MAX_SESSIONS {
            self.error =
                "Wait for the current connection or disconnect a computer first (maximum four)."
                    .into();
            return;
        }
        self.release();
        self.events.clear();
        self.outgoing = Some(Outgoing::start(device));
        self.error.clear();
    }

    fn connect_pair(
        &mut self,
        pair: SessionTicket,
        name: String,
        ctx: &egui::Context,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.sessions.len() < MAX_SESSIONS
                && !self
                    .sessions
                    .iter()
                    .any(|session| session.address == pair.address),
            "Disconnect the existing session before connecting again (maximum four)"
        );
        self.release();
        self.events.clear();
        self.remote_rect = None;
        self.sessions.push(RemoteSession {
            name,
            address: pair.address,
            viewer: Viewer::start(pair, ctx.clone()),
            texture: None,
        });
        self.active = Some(self.sessions.len() - 1);
        self.gallery = false;
        Ok(())
    }

    fn point(&self, pos: egui::Pos2) -> Option<Input> {
        let rect = self.remote_rect?;
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return None;
        }
        Some(Input::Move {
            x: ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0),
            y: ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0),
        })
    }

    fn route_input(&mut self) {
        let events = std::mem::take(&mut self.events);
        if self.gallery
            || self.settings_open
            || !self.focused
            || self.incoming_viewer()
            || !self
                .active
                .and_then(|index| self.sessions.get(index))
                .is_some_and(|session| session.viewer.connected())
        {
            if self.controlling {
                self.release();
            }
            return;
        }
        for event in events {
            match event {
                Event::Key {
                    key: egui::Key::F12,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.ctrl && modifiers.shift => {
                    self.release();
                }
                Event::WindowFocused(false) | Event::PointerGone => {
                    if self.controlling {
                        self.release();
                    }
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let inside = self.remote_rect.is_some_and(|rect| rect.contains(pos));
                    if pressed && !inside {
                        if self.controlling {
                            self.release();
                        }
                        continue;
                    }
                    if inside && pressed {
                        self.controlling = true;
                    }
                    if !self.controlling {
                        continue;
                    }
                    self.sync_modifiers(modifiers);
                    if let Some(point) = self.point(pos) {
                        self.send(point);
                    }
                    let button = match button {
                        egui::PointerButton::Primary => Button::Left,
                        egui::PointerButton::Secondary => Button::Right,
                        egui::PointerButton::Middle => Button::Middle,
                        _ => continue,
                    };
                    self.send(Input::Button { button, pressed });
                }
                Event::PointerMoved(pos) if self.controlling => {
                    if let Some(point) = self.point(pos) {
                        self.send(point);
                    }
                }
                Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } if self.controlling => {
                    self.sync_modifiers(modifiers);
                    let scale = match unit {
                        egui::MouseWheelUnit::Point => 1.0 / 40.0,
                        egui::MouseWheelUnit::Line => 1.0,
                        egui::MouseWheelUnit::Page => 12.0,
                    };
                    self.wheel -= delta * scale;
                    let x = self.wheel.x.trunc().clamp(-100.0, 100.0) as i32;
                    let y = self.wheel.y.trunc().clamp(-100.0, 100.0) as i32;
                    self.wheel -= egui::vec2(x as f32, y as f32);
                    if x != 0 || y != 0 {
                        self.send(Input::Scroll { x, y });
                    }
                }
                Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } if self.controlling => {
                    self.sync_modifiers(modifiers);
                    let name = key.name();
                    if named_key(name).is_some() {
                        self.send(Input::Key {
                            name: name.into(),
                            pressed,
                        });
                    } else if pressed
                        && modifiers.command
                        && name.len() == 1
                        && let Some(letter) = name.chars().next().filter(char::is_ascii_alphabetic)
                    {
                        self.send(Input::Shortcut {
                            letter: letter.to_ascii_lowercase(),
                            shift: modifiers.shift,
                        });
                    }
                }
                Event::Text(text) | Event::Ime(egui::ImeEvent::Commit(text))
                    if self.controlling =>
                {
                    if !self.modifiers.command
                        && !self.modifiers.ctrl
                        && text.len() <= 4096
                        && !text.chars().any(char::is_control)
                    {
                        self.send(Input::Text(text));
                    }
                }
                Event::Copy if self.controlling => self.send(Input::Shortcut {
                    letter: 'c',
                    shift: false,
                }),
                Event::Cut if self.controlling => self.send(Input::Shortcut {
                    letter: 'x',
                    shift: false,
                }),
                Event::Paste(_) if self.controlling => self.send(Input::Shortcut {
                    letter: 'v',
                    shift: false,
                }),
                _ => {}
            }
        }
        if self.controlling {
            self.sync_modifiers(self.modifiers);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (left, right) = self.chrome.insets(&ctx);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::hover());
        ui.painter().rect_filled(
            Rect::from_min_max(rect.min, rect.max - egui::vec2(right, 0.0)),
            0.0,
            Color32::from_rgb(20, 21, 24),
        );
        let content = Rect::from_min_max(
            rect.min + egui::vec2(left, 0.0),
            rect.max - egui::vec2(right, 0.0),
        );
        let mut controls = Vec::new();
        let buttons_width = if self.gallery { 210.0 } else { 248.0 };
        let split = (content.right() - buttons_width).max(content.left() + 36.0);
        let navigation = Rect::from_min_max(content.min, egui::pos2(split - 8.0, content.bottom()));
        let actions = Rect::from_min_max(egui::pos2(split, content.top()), content.max);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(navigation)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                if !self.gallery {
                    let back = ui.button("<").on_hover_text("Computers");
                    controls.push(back.rect);
                    back.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Computers")
                    });
                    if back.clicked() {
                        self.show_gallery();
                    }
                    if let Some(index) = self.active {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&self.sessions[index].name).strong(),
                            )
                            .truncate(),
                        );
                    }
                } else {
                    ui.add(egui::Image::new(&self.logo).fit_to_exact_size(egui::vec2(23.0, 23.0)));
                    ui.label(egui::RichText::new("Localnet").size(15.0).strong());
                }
            },
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(actions)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                let settings = ui.button("...").on_hover_text("Settings");
                controls.push(settings.rect);
                settings.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Settings")
                });
                if settings.clicked() {
                    self.release();
                    self.events.clear();
                    self.settings_open = !self.settings_open;
                }
                if !self.gallery && self.active.is_some() {
                    let disconnect = ui.button("Disconnect");
                    controls.push(disconnect.rect);
                    if disconnect.clicked() {
                        self.disconnect_active();
                    }
                    let full = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                    let fullscreen = ui
                        .button(if full { "Shrink" } else { "Expand" })
                        .on_hover_text("Toggle full screen");
                    controls.push(fullscreen.rect);
                    if fullscreen.clicked() {
                        self.release();
                        self.events.clear();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
                    }
                } else {
                    let color = if self.discovery.is_some() {
                        Color32::from_rgb(115, 174, 143)
                    } else {
                        Color32::from_gray(110)
                    };
                    ui.label(
                        egui::RichText::new(if self.discovery.is_some() {
                            "On your network"
                        } else {
                            "Local access off"
                        })
                        .small()
                        .color(color),
                    );
                }
            },
        );
        // Register dragging only in the gaps; a full-header response steals clicks from its buttons.
        let mut gaps = vec![content];
        for control in &controls {
            let mut next = Vec::new();
            for gap in gaps {
                if gap.intersects(*control) {
                    if control.left() > gap.left() {
                        next.push(Rect::from_min_max(
                            gap.min,
                            egui::pos2(control.left(), gap.bottom()),
                        ));
                    }
                    if control.right() < gap.right() {
                        next.push(Rect::from_min_max(
                            egui::pos2(control.right(), gap.top()),
                            gap.max,
                        ));
                    }
                } else {
                    next.push(gap);
                }
            }
            gaps = next;
        }
        for (index, gap) in gaps.into_iter().enumerate() {
            let drag = ui.interact(
                gap,
                ui.id().with(("window-drag", index)),
                egui::Sense::click_and_drag(),
            );
            if drag.double_clicked() {
                let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            } else if drag.drag_started() {
                self.release();
                self.events.clear();
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        }
        if self.settings_open {
            controls.push(rect);
        }
        self.chrome.update(&ctx, rect, controls);
        ui.painter().hline(
            rect.x_range(),
            rect.bottom(),
            egui::Stroke::new(1.0, Color32::from_rgb(39, 40, 46)),
        );
    }

    fn settings_panel(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(false).default_width(420.0).show(ctx, |ui| {
            ui.label(egui::RichText::new(crate::discovery::computer_name()).strong());
            ui.label("Trusted LAN mode. Computers on this private network can see previews and control this computer while local access is enabled.");
            if let Some(discovery) = &self.discovery {
                let host = discovery.controller.state.lock().unwrap();
                ui.small(if host.sharing { "Screen sharing is active" } else { "Ready for incoming connections" });
                ui.label(&host.status);
                drop(host);
                if ui.button("Refresh nearby computers").clicked() { discovery.refresh_nearby(); }
                if ui.button("Disable local access").clicked() { self.discovery.take(); }
            } else if ui.button("Enable local access").clicked() { self.previews.get_or_insert_with(Previews::new); self.enable_discovery(); }
            ui.separator();
            egui::ComboBox::from_label("Network").selected_text(self.interfaces[self.interface].1.to_string()).show_ui(ui, |ui| {
                for (index, (name, ip)) in self.interfaces.iter().enumerate() {
                    ui.selectable_value(&mut self.interface, index, format!("{name} / {ip}"));
                }
            });
            if ui.button("Apply network").clicked() { self.discovery.take(); self.thumbnails.clear(); self.enable_discovery(); }
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Video").strong());
            ui.add(egui::Slider::new(&mut self.settings.fps, 5..=60).text("Target FPS"));
            ui.add(egui::Slider::new(&mut self.settings.width, 640..=3840).text("Maximum width"));
            ui.add(egui::Slider::new(&mut self.settings.quality, 20..=95).text("Quality"));
            ui.small("Applied on the next incoming connection. JPEG video; target FPS is not guaranteed.");
            if let Some(discovery) = &self.discovery { *discovery.controller.settings.lock().unwrap() = self.settings; }
            if cfg!(target_os = "macos") {
                ui.separator(); ui.label("Sharing needs Screen Recording and Accessibility. Previews only need Screen Recording.");
                if ui.button("Check Mac permissions").clicked() {
                    self.error = match crate::capture::request_permissions() { Ok(()) => "OS permissions are available.".into(), Err(e) => format!("{e:#}") };
                }
            }
            ui.separator();
            ui.small("Ctrl+Shift+F12 releases remote input. Copy/paste uses the remote clipboard.");
            ui.small(format!("Localnet {} / LAN only / TLS encrypted", env!("CARGO_PKG_VERSION")));
            if ui.button("Disconnect everything and stop local access").clicked() { self.shutdown(); }
        });
        self.settings_open = open;
    }

    fn gallery_view(&mut self, ui: &mut egui::Ui, devices: &[Device]) {
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Frame::new().inner_margin(28.0).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                ui.heading("Computers");
                ui.label(
                    egui::RichText::new("Your other screens, one click away.")
                        .color(Color32::from_gray(143)),
                );
                ui.add_space(18.0);
                if devices.is_empty() && self.sessions.is_empty() {
                    ui.add_space(70.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new(if self.discovery.is_some() {
                                "Looking for your computers"
                            } else {
                                "Local access is off"
                            })
                            .size(17.0),
                        );
                        ui.label(
                            egui::RichText::new(
                                "Open Localnet on another Mac or Windows PC on this network.",
                            )
                            .color(Color32::from_gray(145)),
                        );
                        if self.discovery.is_none() && ui.button("Enable local access").clicked() {
                            self.previews.get_or_insert_with(Previews::new);
                            self.enable_discovery();
                        }
                    });
                    return;
                }
                let columns = ((ui.available_width() + 22.0) / 300.0)
                    .floor()
                    .clamp(1.0, 4.0) as usize;
                let width = ((ui.available_width() - 22.0 * (columns - 1) as f32) / columns as f32)
                    .min(440.0);
                let mut clicked = None;
                for row in devices.chunks(columns) {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 22.0;
                        for device in row {
                            let (area, response) = ui.allocate_exact_size(
                                egui::vec2(width, width * 9.0 / 16.0 + 57.0),
                                egui::Sense::click(),
                            );
                            let preview = Rect::from_min_size(
                                area.min,
                                egui::vec2(width, width * 9.0 / 16.0),
                            );
                            let hovered = response.hovered() || response.has_focus();
                            let border = if hovered {
                                Color32::from_rgb(113, 117, 174)
                            } else {
                                Color32::from_rgb(52, 53, 61)
                            };
                            ui.painter()
                                .rect_filled(preview, 8.0, Color32::from_rgb(29, 30, 35));
                            let session = self
                                .sessions
                                .iter()
                                .find(|s| s.address.ip() == device.address.ip());
                            let texture = session.and_then(|s| s.texture.as_ref()).or_else(|| {
                                self.thumbnails.get(&device.address).map(|t| &t.texture)
                            });
                            if let Some(texture) = texture {
                                let pixels = texture.size_vec2();
                                let scale =
                                    (preview.width() / pixels.x).min(preview.height() / pixels.y);
                                let image_rect =
                                    Rect::from_center_size(preview.center(), pixels * scale)
                                        .shrink(1.0);
                                ui.painter().image(
                                    texture.id(),
                                    image_rect,
                                    Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            } else {
                                let label = if !device.compatible() {
                                    "Update Localnet to 0.3.0"
                                } else if self
                                    .previews
                                    .as_ref()
                                    .and_then(|p| p.thumbnails.get(&device.address))
                                    .is_some_and(|t| t.error.is_some())
                                {
                                    "Preview unavailable"
                                } else {
                                    "Getting a preview..."
                                };
                                ui.painter().text(
                                    preview.center(),
                                    egui::Align2::CENTER_CENTER,
                                    label,
                                    egui::FontId::proportional(13.0),
                                    Color32::from_gray(145),
                                );
                            }
                            ui.painter().rect_stroke(
                                preview,
                                8.0,
                                egui::Stroke::new(1.0, border),
                                egui::StrokeKind::Inside,
                            );
                            ui.painter().text(
                                egui::pos2(area.left(), preview.bottom() + 16.0),
                                egui::Align2::LEFT_CENTER,
                                &device.name,
                                egui::FontId::proportional(14.0),
                                Color32::from_gray(225),
                            );
                            let connected = session.is_some_and(|s| s.viewer.connected());
                            let status = if !device.compatible() {
                                "Update required"
                            } else if connected {
                                "Connected"
                            } else {
                                "On your network"
                            };
                            ui.painter().text(
                                egui::pos2(area.left(), preview.bottom() + 36.0),
                                egui::Align2::LEFT_CENTER,
                                status,
                                egui::FontId::proportional(12.0),
                                Color32::from_gray(132),
                            );
                            response.clone().widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    format!("Open {}", device.name),
                                )
                            });
                            if let Some(thumb) = self
                                .previews
                                .as_ref()
                                .and_then(|p| p.thumbnails.get(&device.address))
                            {
                                if let Some(error) = &thumb.error {
                                    response
                                        .clone()
                                        .on_hover_text(format!("Preview unavailable: {error}"));
                                } else if let Some(updated) = thumb.updated {
                                    response.clone().on_hover_text(format!(
                                        "Screenshot updated {}s ago. Click to control.",
                                        updated.elapsed().as_secs()
                                    ));
                                }
                            }
                            if response.clicked()
                                || (response.has_focus()
                                    && ctx.input(|i| i.key_pressed(egui::Key::Enter)))
                            {
                                clicked = Some(device.clone());
                            }
                        }
                    });
                    ui.add_space(14.0);
                }
                if let Some(device) = clicked {
                    self.connect_device(device);
                }
                let mut selected = None;
                for (index, session) in self
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| !devices.iter().any(|d| d.address.ip() == s.address.ip()))
                {
                    if ui
                        .button(format!(
                            "Open {} / {}",
                            session.name,
                            if session.viewer.connected() {
                                "Connected"
                            } else {
                                "Disconnected"
                            }
                        ))
                        .clicked()
                    {
                        selected = Some(index);
                    }
                }
                if let Some(index) = selected {
                    self.select_session(index);
                }
            });
        });
    }

    fn remote_view(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self.active else {
            self.gallery = true;
            return;
        };
        let session = &self.sessions[index];
        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::BLACK);
        if let Some(texture) = &session.texture {
            let pixels = texture.size_vec2();
            let scale = (area.width() / pixels.x).min(area.height() / pixels.y);
            let rect = Rect::from_center_size(area.center(), pixels * scale);
            ui.painter().image(
                texture.id(),
                rect,
                Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            if session.viewer.connected() && !self.settings_open {
                self.remote_rect = Some(rect);
            }
        } else {
            ui.painter().text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                session.viewer.state.lock().unwrap().status.clone(),
                egui::FontId::proportional(14.0),
                Color32::from_gray(180),
            );
        }
        if !session.viewer.connected() {
            let message = session.viewer.state.lock().unwrap().status.clone();
            ui.scope_builder(egui::UiBuilder::new().max_rect(area.shrink(24.0)), |ui| {
                egui::Frame::new()
                    .fill(Color32::from_rgb(35, 28, 30))
                    .inner_margin(12.0)
                    .corner_radius(6)
                    .show(ui, |ui| {
                        ui.label(message);
                    });
            });
        }
        ui.allocate_rect(area, egui::Sense::hover());
    }
}

fn remote_modifiers(value: egui::Modifiers) -> Modifiers {
    Modifiers {
        primary: value.command,
        control: value.ctrl && !value.command,
        alt: value.alt,
        shift: value.shift,
    }
}

impl eframe::App for Localnet {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if (!raw.focused || self.incoming_viewer()) && self.controlling {
            self.release();
        }
        self.events.extend(raw.events.clone());
        self.modifiers = raw.modifiers;
        self.focused = raw.focused;
        if self.controlling {
            // Leave local toolbar pointer events intact, but keep remote keys away from local widgets.
            raw.events.retain(|event| {
                !matches!(
                    event,
                    Event::Key { .. }
                        | Event::Text(_)
                        | Event::Copy
                        | Event::Cut
                        | Event::Paste(_)
                        | Event::Ime(_)
                        | Event::MouseWheel { .. }
                )
            });
            raw.modifiers = egui::Modifiers::NONE;
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().visible() == Some(false)) {
            if self.controlling {
                self.release();
            }
            self.events.clear();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let result = self
            .outgoing
            .as_ref()
            .and_then(|outgoing| outgoing.state.lock().unwrap().result.take());
        if let Some(result) = result {
            if self
                .outgoing
                .as_ref()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .refresh_discovery
                && let Some(discovery) = &self.discovery
            {
                discovery.refresh_nearby();
            }
            let name = self.outgoing.as_ref().unwrap().device.name.clone();
            self.outgoing.take();
            match result.and_then(|pair| {
                self.connect_pair(pair, name, &ctx)
                    .map_err(|e| e.to_string())
            }) {
                Ok(()) => self.error.clear(),
                Err(e) => self.error = e,
            }
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut devices = self
            .discovery
            .as_ref()
            .map(|discovery| {
                discovery
                    .state
                    .lock()
                    .unwrap()
                    .devices
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.address.cmp(&b.address)));
        let previews_enabled = self.gallery
            && self.outgoing.is_none()
            && ctx.input(|i| {
                i.viewport().visible() != Some(false) && i.viewport().minimized != Some(true)
            });
        if let Some(previews) = &mut self.previews {
            previews.tick(&devices, previews_enabled);
            if std::mem::take(&mut previews.refresh_discovery)
                && let Some(discovery) = &self.discovery
            {
                discovery.refresh_nearby();
            }
            self.thumbnails.retain(|address, tile| {
                devices
                    .iter()
                    .any(|d| d.address == *address && d.certificate == tile.certificate)
            });
            for (address, thumb) in &mut previews.thumbnails {
                if let Some(image) = thumb.image.take() {
                    if let Some(tile) = self.thumbnails.get_mut(address) {
                        tile.texture.set(image, TextureOptions::LINEAR);
                    } else {
                        self.thumbnails.insert(
                            *address,
                            TileTexture {
                                texture: ctx.load_texture(
                                    format!("preview-{address}"),
                                    image,
                                    TextureOptions::LINEAR,
                                ),
                                certificate: thumb.device.certificate.clone(),
                            },
                        );
                    }
                }
            }
        }
        for session in &mut self.sessions {
            let image = session.viewer.state.lock().unwrap().frame.take();
            if let Some(image) = image {
                if image
                    .size
                    .iter()
                    .any(|side| *side > ctx.input(|i| i.max_texture_side))
                {
                    self.error = "Frame exceeds GPU texture limit. Reduce host video width.".into();
                    session.viewer.stop();
                } else if let Some(texture) = &mut session.texture {
                    texture.set(image, TextureOptions::LINEAR);
                } else {
                    session.texture = Some(ctx.load_texture(
                        format!("remote-{}", session.address),
                        image,
                        TextureOptions::LINEAR,
                    ));
                }
            }
        }
        self.header(ui);
        self.remote_rect = None;
        if !self.error.is_empty() {
            egui::Frame::new()
                .fill(Color32::from_rgb(50, 29, 34))
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(Color32::from_rgb(239, 167, 174), &self.error);
                        if ui.small_button("Dismiss").clicked() {
                            self.error.clear();
                        }
                    });
                });
        }
        if let Some(outgoing) = &self.outgoing {
            let status = outgoing.state.lock().unwrap().status.clone();
            let name = outgoing.device.name.clone();
            egui::Frame::new().inner_margin(16.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("{name} / {status}"));
                    if ui.button("Cancel").clicked() {
                        self.outgoing.take();
                    }
                });
            });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::from_rgb(20, 21, 24)))
            .show_inside(ui, |ui| {
                if self.gallery {
                    self.gallery_view(ui, &devices);
                } else {
                    self.remote_view(ui);
                }
            });
        if self.settings_open {
            self.settings_panel(&ctx);
        }
        if self.discovery.is_some() || self.outgoing.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        self.route_input();
        if ctx.input(|i| i.viewport().close_requested()) {
            self.shutdown();
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primary_modifier_maps_without_double_control() {
        let mods = remote_modifiers(egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        });
        assert!(mods.primary && !mods.control);
        let mods = remote_modifiers(egui::Modifiers {
            mac_cmd: true,
            command: true,
            ..Default::default()
        });
        assert!(mods.primary && !mods.control);
        let mods = remote_modifiers(egui::Modifiers {
            ctrl: true,
            ..Default::default()
        });
        assert!(!mods.primary && mods.control);
    }

    fn app_with_sessions() -> Localnet {
        let mut app = Localnet::new(&egui::Context::default());
        for port in [45001, 45002] {
            app.sessions.push(RemoteSession {
                name: format!("Computer {port}"),
                address: SocketAddr::from(([127, 0, 0, 1], port)),
                viewer: Viewer::test_session(),
                texture: None,
            });
        }
        app.active = Some(0);
        app
    }

    #[test]
    fn switching_releases_old_session_and_drops_stale_events() {
        let mut app = app_with_sessions();
        app.controlling = true;
        app.remote_rect = Some(Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        ));
        app.events.push(Event::Text("old screen input".into()));
        app.select_session(1);
        assert_eq!(app.active, Some(1));
        assert!(!app.controlling && app.events.is_empty() && app.remote_rect.is_none());
        assert!(matches!(
            app.sessions[0].viewer.test_inputs().as_slice(),
            [Input::ReleaseAll]
        ));
        app.send(Input::Text("new screen input".into()));
        assert!(
            matches!(app.sessions[1].viewer.test_inputs().as_slice(), [Input::Text(text)] if text == "new screen input")
        );
        app.select_session(10);
        assert_eq!(app.active, Some(1));
    }

    #[test]
    fn gallery_never_routes_input_and_reopening_active_session_works() {
        let mut app = app_with_sessions();
        app.gallery = false;
        app.controlling = true;
        app.remote_rect = Some(Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        ));
        app.show_gallery();
        assert!(app.gallery && !app.controlling && app.remote_rect.is_none());
        assert!(matches!(
            app.sessions[0].viewer.test_inputs().as_slice(),
            [Input::ReleaseAll]
        ));
        app.events.push(Event::PointerButton {
            pos: egui::pos2(100.0, 100.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        });
        app.route_input();
        assert!(!app.controlling);
        app.select_session(0);
        assert!(!app.gallery && app.active == Some(0));
    }

    #[test]
    fn gallery_draws_at_compact_and_desktop_sizes_without_control_area() {
        for width in [480.0, 1080.0] {
            let ctx = egui::Context::default();
            let mut app = Localnet::new(&ctx);
            let devices = vec![Device {
                name: "Office Mac".into(),
                address: "192.168.1.20:45778".parse().unwrap(),
                certificate: vec![1],
                version: crate::protocol::VERSION,
            }];
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 720.0),
                )),
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                app.header(ui);
                app.gallery_view(ui, &devices);
            });
            assert!(!output.shapes.is_empty());
            assert!(app.remote_rect.is_none() && !app.controlling);
        }
    }

    #[test]
    fn disconnecting_selects_remaining_session_without_replaying_events() {
        let mut app = app_with_sessions();
        app.events.push(Event::Text("stale input".into()));
        app.disconnect_active();
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.active, Some(0));
        assert_eq!(app.sessions[0].address.port(), 45002);
        assert!(app.events.is_empty());
        app.disconnect_active();
        assert!(app.sessions.is_empty() && app.active.is_none());
        app.disconnect_active();
    }

    #[test]
    fn old_devices_preserve_existing_sessions_and_show_update_error() {
        let mut app = app_with_sessions();
        app.controlling = true;
        app.connect_device(Device {
            name: "Old Mac".into(),
            address: "127.0.0.1:45778".parse().unwrap(),
            certificate: vec![1],
            version: 1,
        });
        assert_eq!(app.active, Some(0));
        assert!(app.controlling);
        assert!(app.error.contains("Update Localnet"));
        assert_eq!(app.sessions.len(), 2);
        app.shutdown();
        assert!(app.sessions.is_empty() && app.active.is_none() && app.discovery.is_none());
    }

    #[test]
    fn clicking_connected_computer_selects_it_without_starting_another_connection() {
        let mut app = app_with_sessions();
        app.active = Some(1);
        app.connect_device(Device {
            name: "Existing PC".into(),
            address: "127.0.0.1:45778".parse().unwrap(),
            certificate: vec![1],
            version: crate::protocol::VERSION,
        });
        assert_eq!(app.active, Some(0));
        assert!(app.outgoing.is_none());
        assert_eq!(app.sessions.len(), 2);
    }

    #[test]
    fn disconnected_computer_is_replaced_without_leaving_duplicate_tab() {
        let mut app = app_with_sessions();
        app.sessions[0].viewer.stop();
        app.connect_device(Device {
            name: "Reconnect".into(),
            address: "127.0.0.1:1".parse().unwrap(),
            certificate: vec![1],
            version: crate::protocol::VERSION,
        });
        assert_eq!(app.sessions.len(), 1);
        assert!(app.outgoing.is_some());
        assert!(!app.controlling);
    }
}
