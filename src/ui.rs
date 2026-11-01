use crate::{
    discovery::{Discovery, Outgoing},
    host::{Host, VideoSettings},
    input::named_key,
    protocol::{Button, Input, Modifiers, Pairing, is_local},
    viewer::Viewer,
};
use eframe::egui::{self, Color32, Event, Rect, TextureHandle, TextureOptions};
use std::{
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

pub struct Localnet {
    interfaces: Vec<(String, IpAddr)>,
    interface: usize,
    settings: VideoSettings,
    host: Option<Host>,
    sessions: Vec<RemoteSession>,
    active: Option<usize>,
    computer_name: String,
    pairing: String,
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
}

impl Localnet {
    pub fn new(ctx: &egui::Context) -> Self {
        ctx.set_visuals(egui::Visuals::dark());
        ctx.global_style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(10.0, 10.0);
            s.spacing.button_padding = egui::vec2(14.0, 8.0);
            s.visuals.selection.bg_fill = Color32::from_rgb(33, 115, 100);
        });
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
            interfaces,
            interface: 0,
            settings: VideoSettings {
                fps: 60,
                width: 1920,
                quality: 70,
            },
            host: None,
            sessions: Vec::new(),
            active: None,
            computer_name: String::new(),
            pairing: String::new(),
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
        }
    }

    pub fn enable_discovery(&mut self) {
        match Discovery::start(self.interfaces[self.interface].1) {
            Ok(discovery) => {
                self.discovery = Some(discovery);
                self.error.clear();
            }
            Err(e) => {
                self.error = format!("Discovery unavailable: {e}. Manual pairing still works.")
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
        if index >= self.sessions.len() || self.active == Some(index) {
            return;
        }
        self.release();
        // Events were captured against the old screen; never replay them into the new session.
        self.events.clear();
        self.remote_rect = None;
        self.active = Some(index);
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
    }

    fn stop_sharing(&mut self) {
        self.host.take();
    }

    fn shutdown(&mut self) {
        self.release();
        self.events.clear();
        self.active = None;
        self.remote_rect = None;
        self.sessions.clear();
        self.stop_sharing();
        self.outgoing.take();
        self.discovery.take();
    }

    fn connect(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.sessions.len() < MAX_SESSIONS,
            "Disconnect a session before adding another (maximum four)"
        );
        let pair = Pairing::decode(&self.pairing)?;
        anyhow::ensure!(
            !self
                .sessions
                .iter()
                .any(|session| session.address == pair.address),
            "This computer already has a session. Select it or disconnect it before reconnecting."
        );
        let name = self.computer_name.trim();
        let name = if name.is_empty() {
            pair.address.to_string()
        } else {
            name.to_owned()
        };
        self.connect_pair(pair, name, ctx)?;
        self.pairing.clear();
        self.computer_name.clear();
        Ok(())
    }

    fn connect_pair(
        &mut self,
        pair: Pairing,
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
        if !self.focused
            || self.host.as_ref().is_some_and(Host::has_viewer)
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
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if (!raw.focused || self.host.as_ref().is_some_and(Host::has_viewer)) && self.controlling {
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
        if self.host.as_ref().is_some_and(Host::approval_finished) {
            self.stop_sharing();
        }
        if let Some(discovery) = &self.discovery {
            discovery
                .accepting
                .store(self.host.is_none(), std::sync::atomic::Ordering::Relaxed);
        }
        let result = self
            .outgoing
            .as_ref()
            .and_then(|outgoing| outgoing.state.lock().unwrap().result.take());
        if let Some(result) = result {
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
        ui.horizontal(|ui| {
            ui.heading("LOCALNET");
            ui.label(
                egui::RichText::new("PRIVATE LAN / REMOTE DESKTOP")
                    .small()
                    .color(Color32::from_rgb(107, 175, 156)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Stop all").clicked() {
                    self.shutdown();
                }
                if self.controlling && ui.button("Release input").clicked() {
                    self.release();
                }
            });
        });
        ui.separator();
        egui::Panel::left("computers").default_size(300.0).size_range(260.0..=380.0)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("This computer");
                    ui.label(if cfg!(target_os = "macos") { "macOS / Local sharing" } else { "Windows / Local sharing" });
                    ui.heading("Nearby computers");
                    ui.label("Discovery is not authorization. Every connection needs a fresh code and approval on the other computer.");
                    if let Some(discovery) = &self.discovery {
                        let (status, mut devices) = { let state = discovery.state.lock().unwrap();
                            (state.status.clone(), state.devices.values().cloned().collect::<Vec<_>>()) };
                        ui.label(status);
                        devices.sort_by(|a,b| a.name.cmp(&b.name).then(a.address.cmp(&b.address)));
                        if devices.is_empty() { ui.label("Open Localnet on your other computers. They will appear here automatically."); }
                        for device in devices {
                            ui.label(format!("{} ({})", device.name, device.address.ip()));
                            if ui.add_enabled(self.outgoing.is_none() && self.sessions.len() < MAX_SESSIONS,
                                egui::Button::new(format!("Connect to {}", device.name))).clicked() {
                                self.outgoing = Some(Outgoing::start(device));
                                self.release(); self.events.clear();
                            }
                        }
                        if ui.button("Disable discovery and incoming requests").clicked() { self.discovery.take(); }
                        ctx.request_repaint_after(Duration::from_millis(100));
                    } else if ui.button("Enable automatic discovery").clicked() { self.enable_discovery(); }
                    if let Some(outgoing) = &mut self.outgoing {
                        let mut state = outgoing.state.lock().unwrap();
                        ui.label(format!("Connecting to {}", outgoing.device.name));
                        ui.label(&state.status);
                        if state.waiting_code {
                            ui.add(egui::TextEdit::singleline(&mut outgoing.code).char_limit(6).hint_text("Six-digit code").password(true));
                            if ui.add_enabled(outgoing.code.len() == 6 && outgoing.code.bytes().all(|b| b.is_ascii_digit()),
                                egui::Button::new("Verify code")).clicked() {
                                state.entered_code = Some(std::mem::take(&mut outgoing.code)); state.waiting_code = false;
                            }
                        }
                        drop(state);
                        if ui.button("Cancel connection request").clicked() { self.outgoing.take(); }
                    }
                    ui.separator();
                    if let Some(host) = &self.host {
                        ui.colored_label(Color32::from_rgb(107, 175, 156), "Sharing is enabled");
                        ui.label(host.status.lock().unwrap().clone());
                        if host.has_viewer() {
                            ui.label("Remote input forwarding is paused while someone is connected to this computer, to prevent control loops.");
                        }
                        if ui.button("Copy private pairing code").clicked() { ctx.copy_text(host.pairing.clone()); }
                        ui.label("Anyone with this code can view and control this computer. Stopping sharing revokes the code.");
                        if ui.button("Stop sharing this computer").clicked() { self.stop_sharing(); }
                        ctx.request_repaint_after(Duration::from_millis(250));
                    } else {
                        ui.label("Share your primary display to let a paired Mac or Windows computer control it.");
                        egui::ComboBox::from_label("LAN interface")
                            .selected_text(format!("{} ({})", self.interfaces[self.interface].0, self.interfaces[self.interface].1))
                            .show_ui(ui, |ui| {
                                for (index, (name, ip)) in self.interfaces.iter().enumerate() {
                                    ui.selectable_value(&mut self.interface, index, format!("{name} ({ip})"));
                                }
                            });
                        ui.collapsing("Video settings", |ui| {
                            ui.add(egui::Slider::new(&mut self.settings.fps, 5..=60).text("Target FPS (cap, not guaranteed)"));
                            ui.add(egui::Slider::new(&mut self.settings.width, 640..=3840).text("Video width"));
                            ui.add(egui::Slider::new(&mut self.settings.quality, 20..=95).text("JPEG quality"));
                        });
                        if cfg!(target_os = "macos") {
                            ui.label("Sharing requires Screen Recording and Accessibility permission. Viewing another computer does not.");
                        } else {
                            ui.label("Allow Localnet through Windows Firewall on Private networks when sharing.");
                        }
                        if ui.button("Share this computer").clicked() {
                            match Host::start(self.interfaces[self.interface].1, self.settings) {
                                Ok(host) => { self.host = Some(host); self.error.clear(); }
                                Err(e) => self.error = format!("{e:#}"),
                            }
                        }
                    }
                    ui.add_space(16.0);
                    ui.separator();
                    ui.collapsing("Manual pairing (advanced)", |ui| {
                    ui.label("On the other Mac or Windows PC, click Share this computer, then transfer its pairing code privately.");
                    ui.add(egui::TextEdit::singleline(&mut self.computer_name).char_limit(60)
                        .hint_text("Computer name (optional)").desired_width(f32::INFINITY));
                    ui.add(egui::TextEdit::multiline(&mut self.pairing).char_limit(crate::protocol::MAX_JSON)
                        .desired_rows(3).desired_width(f32::INFINITY).hint_text("Private pairing code: ln1:...").password(true));
                    if ui.add_enabled(self.sessions.len() < MAX_SESSIONS, egui::Button::new("Connect")).clicked() {
                        match self.connect(&ctx) { Ok(()) => self.error.clear(), Err(e) => self.error = format!("{e:#}") }
                    }
                    });
                    ui.label(format!("{} / {MAX_SESSIONS} remote sessions. Sharing and viewing work independently.", self.sessions.len()));
                    if !self.error.is_empty() { ui.colored_label(Color32::from_rgb(245, 140, 130), &self.error); }
                    ui.add_space(16.0);
                    ui.label("LAN only / TLS encrypted / No cloud");
                    ui.label("Windows: native DXGI capture with screenshot fallback. Native TurboJPEG encoding; not hardware H.264 or zero latency.");
                });
            });
        self.remote_rect = None;
        egui::CentralPanel::default().show_inside(ui, |ui| {
            if self.sessions.is_empty() {
                ui.heading("One app. Either direction.");
                ui.label("Control Windows from your Mac, or your Mac from Windows.");
                ui.add_space(16.0);
                ui.label("Click a nearby computer, enter the code displayed there, then approve access there. Sharing starts automatically after approval.");
                ui.label("You can also share this computer while viewing others. Sharing is always opt-in.");
                return;
            }
            let mut selected = None;
            ui.horizontal_wrapped(|ui| {
                for (index, session) in self.sessions.iter().enumerate() {
                    let suffix = if session.viewer.connected() { "" } else { " (disconnected)" };
                    if ui.selectable_label(self.active == Some(index), format!("{}{suffix}", session.name)).clicked() {
                        selected = Some(index);
                    }
                }
            });
            if let Some(index) = selected { self.select_session(index); }
            ui.horizontal(|ui| {
                if ui.button("Disconnect selected computer").clicked() { self.disconnect_active(); }
            });
            let Some(index) = self.active else { return; };
            let session = &mut self.sessions[index];
            let (status, frame, rtt, video_time, frames, remote_mac) = {
                let mut state = session.viewer.state.lock().unwrap();
                (state.status.clone(), state.frame.take(), state.input_rtt_ms, state.frame_roundtrip_ms,
                    state.frames, state.display.as_ref().map(|d| d.mac))
            };
            ui.heading(&session.name);
            ui.label(format!("{} / {}", remote_mac.map_or("Connecting", |mac| if mac { "macOS" } else { "Windows" }), session.address));
            ui.label(status);
            ui.label(format!("Input ACK: {rtt:.1} ms | Receive + decode: {video_time:.1} ms | Frames: {frames}"));
            ui.label(if self.controlling { "Controlling this computer. Ctrl+Shift+F12 releases input." }
                else { "Click the screen to control. Ctrl/Command follows the remote OS; clipboard shortcuts use its clipboard." });
            if let Some(image) = frame {
                if image.size.iter().any(|side| *side > ctx.input(|i| i.max_texture_side)) {
                    self.error = "Frame exceeds GPU texture limit. Reduce host video width.".into();
                    session.viewer.stop();
                } else if let Some(texture) = &mut session.texture { texture.set(image, TextureOptions::LINEAR); }
                else { session.texture = Some(ctx.load_texture(format!("remote-{}", session.address), image, TextureOptions::LINEAR)); }
            }
            if let Some(texture) = &session.texture {
                let available = ui.available_size().max(egui::vec2(1.0, 1.0));
                let pixels = texture.size_vec2();
                let scale = (available.x / pixels.x).min(available.y / pixels.y);
                let (_, area) = ui.allocate_space(available);
                let rect = Rect::from_center_size(area.center(), pixels * scale);
                ui.painter().rect_filled(area, 0.0, Color32::BLACK);
                ui.painter().image(texture.id(), rect, Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
                if session.viewer.connected() { self.remote_rect = Some(rect); }
            }
        });
        let pending = self
            .discovery
            .as_ref()
            .and_then(|d| d.state.lock().unwrap().pending.clone());
        if let Some(pending) = pending {
            if self.controlling {
                self.release();
            }
            self.events.clear();
            let mut pending = pending.lock().unwrap();
            egui::Window::new("Connection request").collapsible(false).resizable(false).show(&ctx, |ui| {
                ui.label(format!("{} ({}) requests screen viewing and keyboard/mouse control.", pending.name, pending.peer));
                ui.label("The computer name is not verified. Only allow a request you initiated.");
                ui.heading(format!("Pairing code: {}", pending.code));
                ui.label("Enter this code on the computer you are connecting from. Never give it to an unexpected caller.");
                if pending.verified { ui.label("Code verified. You can approve this connection."); }
                if ui.add_enabled(pending.ready() && self.host.is_none(), egui::Button::new("Allow viewing and control")).clicked() {
                    match Host::start_approved(self.discovery.as_ref().unwrap().ip, self.settings) {
                        Ok(host) => {
                            let decision = Pairing::decode(&host.pairing).map_err(|e| e.to_string());
                            if pending.ready() { pending.decision = Some(decision); self.host = Some(host); }
                        }
                        Err(e) => { self.error = format!("{e:#}"); pending.decision = Some(Err(self.error.clone())); }
                    }
                }
                if ui.button("Deny request").clicked() { pending.cancel(); }
            });
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
    fn failed_pairing_preserves_existing_sessions() {
        let mut app = app_with_sessions();
        app.controlling = true;
        app.pairing = "not a code".into();
        assert!(app.connect(&egui::Context::default()).is_err());
        assert_eq!(app.active, Some(0));
        assert!(app.controlling);
        let pair = Pairing {
            version: crate::protocol::VERSION,
            address: app.sessions[0].address,
            certificate: vec![1],
            token: "a".repeat(64),
        };
        app.pairing = pair.encode().unwrap();
        assert!(app.connect(&egui::Context::default()).is_err());
        assert_eq!(app.sessions.len(), 2);
        app.shutdown();
        assert!(app.sessions.is_empty() && app.active.is_none() && app.host.is_none());
    }

    #[test]
    #[ignore = "Requires an unlocked interactive desktop; loopback capture only, no input injection"]
    fn sharing_and_viewing_have_independent_lifetimes() {
        let mut app = app_with_sessions();
        app.host = Some(
            Host::start(
                "127.0.0.1".parse().unwrap(),
                VideoSettings {
                    fps: 10,
                    width: 640,
                    quality: 50,
                },
            )
            .unwrap(),
        );
        app.disconnect_active();
        assert!(app.host.is_some());
        app.stop_sharing();
        assert!(app.sessions[0].viewer.connected());
        let ctx = egui::Context::default();
        app.host = Some(
            Host::start(
                "127.0.0.1".parse().unwrap(),
                VideoSettings {
                    fps: 10,
                    width: 640,
                    quality: 50,
                },
            )
            .unwrap(),
        );
        app.pairing = app.host.as_ref().unwrap().pairing.clone();
        app.connect(&ctx).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while app.sessions[1].viewer.state.lock().unwrap().frames < 2
            && std::time::Instant::now() < until
        {
            assert!(
                app.sessions[1].viewer.connected(),
                "{}",
                app.sessions[1].viewer.state.lock().unwrap().status
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(app.sessions[1].viewer.state.lock().unwrap().frames >= 2);
        // Do not forward input received from another controller into a second computer.
        app.controlling = true;
        app.events.push(Event::Text("must not forward".into()));
        app.route_input();
        assert!(!app.controlling);
        app.disconnect_active();
        assert!(app.host.is_some() && app.sessions[0].viewer.connected());
        app.shutdown();
    }
}
