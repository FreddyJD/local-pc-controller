use crate::protocol::Input;
#[cfg(any(target_os = "windows", test))]
use crate::protocol::Modifiers;
#[cfg(target_os = "windows")]
use eframe::egui;

#[derive(Default)]
pub struct Keyboard {
    #[cfg(target_os = "windows")]
    hook: Option<win::Hook>,
}

impl Keyboard {
    pub fn install(_cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(target_os = "windows")]
        {
            Self {
                hook: _cc
                    .winit_window()
                    .and_then(|window| win::Hook::new(window, _cc.egui_ctx.clone())),
            }
        }
        #[cfg(not(target_os = "windows"))]
        Self::default()
    }

    pub fn arm(&mut self, _enabled: bool) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.hook.as_mut().is_some_and(|hook| hook.arm(_enabled))
        }
        #[cfg(not(target_os = "windows"))]
        true
    }

    pub fn drain(&mut self) -> (Vec<Input>, bool, bool) {
        #[cfg(target_os = "windows")]
        if let Some(hook) = &self.hook {
            return hook.drain();
        }
        (Vec::new(), false, false)
    }

    pub fn meta(&self) -> bool {
        #[cfg(target_os = "windows")]
        if let Some(hook) = &self.hook {
            return hook.meta();
        }
        false
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
struct ChordState {
    win: [bool; 2],
    owned: std::collections::BTreeSet<u32>,
}

#[cfg(any(target_os = "windows", test))]
impl ChordState {
    fn event(&mut self, vk: u32, pressed: bool, mut modifiers: Modifiers) -> Option<Vec<Input>> {
        if vk == 0x5b || vk == 0x5c {
            self.win[(vk - 0x5b) as usize] = pressed;
            modifiers.meta = self.win.iter().any(|held| *held);
            return Some(vec![Input::Modifiers(modifiers)]);
        }
        let owned = self.owned.contains(&vk);
        if !owned && !self.win.iter().any(|held| *held) {
            return None;
        }
        // Do not forward the lock shortcut or secure-attention sequences. Unsupported chords pass through.
        if vk == 0x4c {
            return None;
        }
        let name = match vk {
            0x41..=0x5a => (vk as u8 as char).to_string(),
            0x09 => "Tab".into(),
            0x20 => "Space".into(),
            0x0d => "Enter".into(),
            0x1b => "Escape".into(),
            0x25 => "Left".into(),
            0x26 => "Up".into(),
            0x27 => "Right".into(),
            0x28 => "Down".into(),
            _ => return None,
        };
        if pressed {
            self.owned.insert(vk);
        } else {
            self.owned.remove(&vk);
        }
        modifiers.meta = self.win.iter().any(|held| *held);
        Some(vec![
            Input::Modifiers(modifiers),
            Input::Key { name, pressed },
        ])
    }
}

#[cfg(any(target_os = "windows", test))]
fn scoped(armed: bool, foreground: usize, window: usize, injected: bool) -> bool {
    armed && window != 0 && foreground == window && !injected
}

#[cfg(target_os = "windows")]
pub fn focus_lost(hwnd: usize) {
    win::focus_lost(hwnd);
}

#[cfg(target_os = "windows")]
mod win {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};
    use windows_sys::Win32::{
        Foundation::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    struct State {
        hwnd: usize,
        ctx: egui::Context,
        armed: bool,
        chord: ChordState,
        events: VecDeque<Input>,
        canceled: bool,
    }
    thread_local! { static STATE:RefCell<Option<State>>=const{RefCell::new(None)}; }
    pub struct Hook {
        hwnd: usize,
        handle: HHOOK,
    }

    pub fn focus_lost(hwnd: usize) {
        STATE.with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut()
                && let Some(state) = slot.as_mut()
                && state.hwnd == hwnd
                && state.armed
            {
                state.armed = false;
                state.events.clear();
                state.chord = ChordState::default();
                state.canceled = true;
                state.ctx.request_repaint();
            }
        });
    }

    impl Hook {
        pub fn new(window: &winit::window::Window, ctx: egui::Context) -> Option<Self> {
            let RawWindowHandle::Win32(raw) = window.window_handle().ok()?.as_raw() else {
                return None;
            };
            let hwnd = raw.hwnd.get() as usize;
            STATE.with(|slot| {
                *slot.borrow_mut() = Some(State {
                    hwnd,
                    ctx,
                    armed: false,
                    chord: ChordState::default(),
                    events: VecDeque::new(),
                    canceled: false,
                })
            });
            Some(Self {
                hwnd,
                handle: std::ptr::null_mut(),
            })
        }

        pub fn arm(&mut self, enabled: bool) -> bool {
            if !enabled {
                if !self.handle.is_null() {
                    unsafe {
                        UnhookWindowsHookEx(self.handle);
                    }
                    self.handle = std::ptr::null_mut();
                }
                STATE.with(|slot| {
                    if let Some(state) = slot.borrow_mut().as_mut() {
                        state.armed = false;
                        state.events.clear();
                        state.chord = ChordState::default();
                        state.canceled = false;
                    }
                });
                return true;
            }
            if self.handle.is_null() {
                self.handle = unsafe {
                    SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(callback),
                        GetModuleHandleW(std::ptr::null()),
                        0,
                    )
                };
                if self.handle.is_null() {
                    return false;
                }
            }
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    state.armed = true;
                }
            });
            true
        }

        pub fn drain(&self) -> (Vec<Input>, bool, bool) {
            STATE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(state) = slot.as_mut() else {
                    return (Vec::new(), false, false);
                };
                if state.armed && unsafe { GetForegroundWindow() } as usize != self.hwnd {
                    state.canceled = true;
                    state.armed = false;
                    state.events.clear();
                    state.chord = ChordState::default();
                }
                (
                    state.events.drain(..).collect(),
                    state.chord.win.iter().any(|x| *x),
                    std::mem::take(&mut state.canceled),
                )
            })
        }
        pub fn meta(&self) -> bool {
            STATE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|state| state.armed && state.chord.win.iter().any(|x| *x))
            })
        }
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            self.arm(false);
            STATE.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code < 0 {
            return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
        }
        let captured = std::panic::catch_unwind(|| {
            let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            if key.flags & LLKHF_INJECTED != 0 {
                return false;
            }
            let pressed = match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => true,
                WM_KEYUP | WM_SYSKEYUP => false,
                _ => return false,
            };
            STATE.with(|slot| {
                let Ok(mut slot) = slot.try_borrow_mut() else {
                    return false;
                };
                let Some(state) = slot.as_mut() else {
                    return false;
                };
                if !scoped(
                    state.armed,
                    unsafe { GetForegroundWindow() } as usize,
                    state.hwnd,
                    false,
                ) {
                    if !state.armed {
                        return false;
                    }
                    state.armed = false;
                    state.events.clear();
                    state.chord = ChordState::default();
                    state.canceled = true;
                    state.ctx.request_repaint();
                    return false;
                }
                let down = |vk| unsafe { GetAsyncKeyState(vk as i32) } < 0;
                if key.vkCode == u32::from(VK_L) && state.chord.win.iter().any(|held| *held) {
                    state.armed = false;
                    state.events.clear();
                    state.chord = ChordState::default();
                    state.canceled = true;
                    state.ctx.request_repaint();
                    return false;
                }
                let mut mods = Modifiers {
                    primary: down(VK_CONTROL),
                    control: false,
                    alt: down(VK_MENU),
                    shift: down(VK_SHIFT),
                    meta: false,
                };
                // GetAsyncKeyState describes the state before this low-level transition.
                match key.vkCode {
                    0xa0 | 0xa1 | 0x10 => mods.shift = pressed,
                    0xa2 | 0xa3 | 0x11 => mods.primary = pressed,
                    0xa4 | 0xa5 | 0x12 => mods.alt = pressed,
                    _ => {}
                }
                let events = state.chord.event(key.vkCode, pressed, mods);
                if let Some(events) = events {
                    if state.events.len() + events.len() > 128 {
                        state.armed = false;
                        state.canceled = true;
                        state.events.clear();
                        state.chord = ChordState::default();
                        state.ctx.request_repaint();
                        return false;
                    }
                    state.events.extend(events);
                    state.ctx.request_repaint();
                    return true;
                }
                if state.chord.win.iter().any(|held| *held)
                    && matches!(key.vkCode,0xa0..=0xa5|0x10..=0x12)
                {
                    if state.events.len() >= 128 {
                        state.armed = false;
                        state.canceled = true;
                        state.events.clear();
                        state.ctx.request_repaint();
                        return false;
                    }
                    mods.meta = true;
                    state.events.push_back(Input::Modifiers(mods));
                    state.ctx.request_repaint();
                }
                false
            })
        })
        .unwrap_or(false);
        if captured {
            1
        } else {
            unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_meta_chords_preserve_hold_release_order_and_both_windows_keys() {
        let mut state = ChordState::default();
        let mods = Modifiers::default();
        assert!(state.event(0x41, true, mods).is_none());
        assert!(
            matches!(state.event(0x5b,true,mods).unwrap().as_slice(),[Input::Modifiers(m)] if m.meta)
        );
        assert!(
            matches!(state.event(0x09,true,mods).unwrap().as_slice(),[Input::Modifiers(m),Input::Key{name,pressed:true}] if m.meta&&name=="Tab")
        );
        state.event(0x5c, true, mods);
        state.event(0x5b, false, mods);
        assert!(state.win[1]);
        state.event(0x5c, false, mods);
        assert!(
            matches!(state.event(0x09,false,mods).unwrap().as_slice(),[Input::Modifiers(m),Input::Key{pressed:false,..}] if !m.meta)
        );
        assert!(state.owned.is_empty());
    }
    #[test]
    fn windows_space_and_letters_are_forwarded_but_lock_and_delete_are_not() {
        let mut state = ChordState::default();
        let mods = Modifiers::default();
        state.event(0x5b, true, mods);
        assert!(state.event(0x20, true, mods).is_some());
        assert!(state.event(0x41, true, mods).is_some());
        assert!(state.event(0x4c, true, mods).is_none());
        assert!(state.event(0x2e, true, mods).is_none());
    }

    #[test]
    fn modifier_only_taps_and_repeated_tabs_are_not_lost_between_frames() {
        let mut state = ChordState::default();
        let mods = Modifiers::default();
        let mut events = state.event(0x5b, true, mods).unwrap();
        events.extend(state.event(0x5b, false, mods).unwrap());
        assert!(
            matches!(events.as_slice(),[Input::Modifiers(down),Input::Modifiers(up)] if down.meta&&!up.meta)
        );
        state.event(0x5b, true, mods);
        for _ in 0..3 {
            let events = state
                .event(
                    0x09,
                    true,
                    Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(matches!(&events[0],Input::Modifiers(m) if m.meta&&m.shift));
        }
        assert_eq!(state.owned.len(), 1);
        state.event(0x09, false, mods);
        state.event(0x5b, false, mods);
        assert!(state.owned.is_empty() && !state.win.iter().any(|held| *held));
    }

    #[test]
    fn hook_scope_never_captures_other_apps_disabled_control_or_injected_input() {
        assert!(scoped(true, 10, 10, false));
        assert!(!scoped(false, 10, 10, false));
        assert!(!scoped(true, 20, 10, false));
        assert!(!scoped(true, 10, 10, true));
        assert!(!scoped(true, 0, 0, false));
    }
}
