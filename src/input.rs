use crate::protocol::{Button, DisplayInfo, Input, Modifiers};
use anyhow::{Result, ensure};
use enigo::{Axis, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};

pub struct InputDevice {
    enigo: Enigo,
    keys: Vec<Key>,
    buttons: Vec<Button>,
    modifiers: Modifiers,
}

impl InputDevice {
    pub fn new() -> Result<Self> {
        Ok(Self {
            enigo: Enigo::new(&Settings {
                open_prompt_to_get_permissions: false,
                ..Default::default()
            })?,
            keys: Vec::new(),
            buttons: Vec::new(),
            modifiers: Modifiers::default(),
        })
    }

    fn key(&mut self, key: Key, pressed: bool) -> Result<()> {
        if pressed {
            if !self.keys.contains(&key) {
                self.keys.push(key);
            }
            self.enigo.key(key, Direction::Press)?;
        } else {
            self.enigo.key(key, Direction::Release)?;
            self.keys.retain(|k| *k != key);
        }
        Ok(())
    }

    fn set_modifiers(&mut self, value: Modifiers) -> Result<()> {
        let previous = modifier_keys(self.modifiers);
        let desired = modifier_keys(value);
        for key in previous.iter().filter(|k| !desired.contains(k)) {
            self.key(*key, false)?;
        }
        for key in desired.iter().filter(|k| !previous.contains(k)) {
            self.key(*key, true)?;
        }
        self.modifiers = value;
        Ok(())
    }

    pub fn apply(&mut self, event: Input, display: &DisplayInfo) -> Result<()> {
        match event {
            Input::Move { x, y } => {
                let (x, y) = display.point(x, y)?;
                self.enigo.move_mouse(x, y, Coordinate::Abs)?;
            }
            Input::Button { button, pressed } => {
                if pressed && !self.buttons.contains(&button) {
                    self.buttons.push(button);
                }
                self.enigo.button(
                    native_button(button),
                    if pressed {
                        Direction::Press
                    } else {
                        Direction::Release
                    },
                )?;
                if !pressed {
                    self.buttons.retain(|b| *b != button);
                }
            }
            Input::Scroll { x, y } => {
                ensure!(
                    x.abs_diff(0) <= 100 && y.abs_diff(0) <= 100,
                    "Scroll exceeds limit"
                );
                if x != 0 {
                    self.enigo.scroll(x, Axis::Horizontal)?;
                }
                if y != 0 {
                    self.enigo.scroll(y, Axis::Vertical)?;
                }
            }
            Input::Modifiers(value) => self.set_modifiers(value)?,
            Input::Key { name, pressed } => {
                let key = named_key(&name).ok_or_else(|| anyhow::anyhow!("Unsupported key"))?;
                self.key(key, pressed)?;
            }
            Input::Text(text) => {
                ensure!(
                    text.len() <= 4096 && !text.chars().any(char::is_control),
                    "Invalid text input"
                );
                // Text already contains layout/shift composition. Do not modify it a second time.
                let restore = self.modifiers;
                self.set_modifiers(Modifiers::default())?;
                self.enigo.text(&text)?;
                self.set_modifiers(restore)?;
            }
            Input::Shortcut { letter, shift } => {
                ensure!(letter.is_ascii_lowercase(), "Invalid shortcut key");
                let restore = self.modifiers;
                self.set_modifiers(Modifiers {
                    primary: true,
                    shift,
                    ..Default::default()
                })?;
                let code = shortcut_code(letter);
                let press = self.enigo.raw(code, Direction::Press);
                let release = self.enigo.raw(code, Direction::Release);
                press?;
                release?;
                self.set_modifiers(restore)?;
            }
            Input::ReleaseAll => self.release_all(),
        }
        Ok(())
    }

    pub fn release_all(&mut self) {
        for button in self.buttons.drain(..) {
            let _ = self.enigo.button(native_button(button), Direction::Release);
        }
        for key in self.keys.drain(..) {
            let _ = self.enigo.key(key, Direction::Release);
        }
        self.modifiers = Modifiers::default();
    }
}

impl Drop for InputDevice {
    fn drop(&mut self) {
        self.release_all();
    }
}

fn native_button(button: Button) -> enigo::Button {
    match button {
        Button::Left => enigo::Button::Left,
        Button::Right => enigo::Button::Right,
        Button::Middle => enigo::Button::Middle,
    }
}

fn modifier_keys(value: Modifiers) -> Vec<Key> {
    let mut keys = Vec::new();
    if value.primary {
        keys.push(if cfg!(target_os = "macos") {
            Key::Meta
        } else {
            Key::Control
        });
    }
    if value.control && !keys.contains(&Key::Control) {
        keys.push(Key::Control);
    }
    if value.alt {
        keys.push(Key::Alt);
    }
    if value.shift {
        keys.push(Key::Shift);
    }
    keys
}

// Shortcut letters use US-layout physical positions. Ordinary typing uses committed Unicode text.
fn shortcut_code(letter: char) -> u16 {
    #[cfg(target_os = "macos")]
    const CODES: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    #[cfg(not(target_os = "macos"))]
    const CODES: [u16; 26] = [
        0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
    ];
    CODES[(letter as u8 - b'a') as usize]
}

pub fn named_key(name: &str) -> Option<Key> {
    Some(match name {
        "Enter" => Key::Return,
        "Escape" => Key::Escape,
        "Tab" => Key::Tab,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Left" => Key::LeftArrow,
        "Right" => Key::RightArrow,
        "Up" => Key::UpArrow,
        "Down" => Key::DownArrow,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowlisted_keys_and_modifiers() {
        assert_eq!(named_key("Enter"), Some(Key::Return));
        assert_eq!(
            named_key(eframe::egui::Key::ArrowLeft.name()),
            Some(Key::LeftArrow)
        );
        assert!(named_key("Other(1234)").is_none());
        let keys = modifier_keys(Modifiers {
            primary: true,
            control: true,
            ..Default::default()
        });
        assert_eq!(keys.iter().filter(|k| **k == Key::Control).count(), 1);
        assert!(shortcut_code('a') != shortcut_code('z'));
    }
}
