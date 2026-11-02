use eframe::egui;

#[derive(Default)]
pub struct Chrome {
    #[cfg(target_os = "windows")]
    native: Option<win::NativeFrame>,
    #[cfg(target_os = "macos")]
    traffic_lights: egui::Vec2,
}

impl Chrome {
    pub fn install(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
        #[cfg(target_os = "windows")]
        {
            Self {
                native: cc
                    .winit_window()
                    .and_then(|window| win::NativeFrame::install(window)),
            }
        }
        #[cfg(target_os = "macos")]
        {
            let traffic_lights = cc
                .winit_window()
                .map(|window| {
                    use eframe::egui::Vec2;
                    use raw_window_handle::HasWindowHandle;
                    window
                        .window_handle()
                        .ok()
                        .and_then(|handle| {
                            eframe::WindowChromeMetrics::from_window_handle(&handle.as_raw())
                                .map(|metrics| metrics.traffic_lights_size)
                        })
                        .unwrap_or(Vec2::new(80.0, 30.0))
                })
                .unwrap_or(egui::vec2(80.0, 30.0));
            Self { traffic_lights }
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Self::default()
        }
    }

    pub fn insets(&self, ctx: &egui::Context) -> (f32, f32) {
        #[cfg(target_os = "windows")]
        if let Some(native) = &self.native {
            return (16.0, native.button_width(ctx.pixels_per_point()) + 8.0);
        }
        #[cfg(target_os = "macos")]
        {
            return (
                (self.traffic_lights.x / ctx.zoom_factor()).max(78.0) + 10.0,
                16.0,
            );
        }
        #[allow(unreachable_code)]
        {
            let _ = ctx;
            (16.0, 16.0)
        }
    }

    pub fn update(&self, ctx: &egui::Context, header: egui::Rect, controls: Vec<egui::Rect>) {
        #[cfg(target_os = "windows")]
        if let Some(native) = &self.native {
            native.update(ctx.pixels_per_point(), header, controls);
        }
        #[cfg(not(target_os = "windows"))]
        let _ = (ctx, header, controls);
    }
}

#[cfg(target_os = "windows")]
mod win {
    use super::*;
    use std::{
        ffi::c_void,
        sync::{Arc, Mutex},
    };
    use windows_sys::Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        UI::{HiDpi::*, Shell::*, WindowsAndMessaging::*},
    };
    use winit::{
        platform::windows::{Color, WindowExtWindows},
        raw_window_handle::{HasWindowHandle, RawWindowHandle},
    };

    const SUBCLASS: usize = 0x4c4e;
    struct HitRegions {
        scale: f32,
        header: egui::Rect,
        controls: Vec<egui::Rect>,
    }
    impl Default for HitRegions {
        fn default() -> Self {
            Self {
                scale: 1.0,
                header: egui::Rect::NOTHING,
                controls: Vec::new(),
            }
        }
    }
    pub struct NativeFrame {
        hwnd: usize,
        regions: Arc<Mutex<HitRegions>>,
    }

    impl NativeFrame {
        pub fn install(window: &winit::window::Window) -> Option<Self> {
            let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else {
                return None;
            };
            let hwnd = handle.hwnd.get() as HWND;
            window.set_title_background_color(Some(Color::from_rgb(20, 21, 24)));
            window.set_title_text_color(Color::from_rgb(225, 225, 230));
            let regions = Arc::new(Mutex::new(HitRegions::default()));
            let state = Box::into_raw(Box::new(regions.clone()));
            // The subclass owns this Arc until WM_NCDESTROY; installation and dispatch are on the UI thread.
            unsafe {
                if SetWindowSubclass(hwnd, Some(procedure), SUBCLASS, state as usize) == 0 {
                    drop(Box::from_raw(state));
                    return None;
                }
                extend_frame(hwnd);
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
            Some(Self {
                hwnd: hwnd as usize,
                regions,
            })
        }

        pub fn button_width(&self, scale: f32) -> f32 {
            if unsafe { GetWindowLongW(self.hwnd as HWND, GWL_STYLE) } as u32 & WS_CAPTION == 0 {
                return 0.0;
            }
            let mut bounds = RECT::default();
            let result = unsafe {
                DwmGetWindowAttribute(
                    self.hwnd as HWND,
                    DWMWA_CAPTION_BUTTON_BOUNDS as u32,
                    &mut bounds as *mut _ as *mut c_void,
                    std::mem::size_of::<RECT>() as u32,
                )
            };
            if result >= 0 && bounds.right > bounds.left {
                (bounds.right - bounds.left) as f32 / scale
            } else {
                138.0 / scale.max(1.0)
            }
        }

        pub fn update(&self, scale: f32, header: egui::Rect, controls: Vec<egui::Rect>) {
            *self.regions.lock().unwrap() = HitRegions {
                scale,
                header,
                controls,
            };
        }
    }

    unsafe fn extend_frame(hwnd: HWND) {
        let top = (48.0 * unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0).ceil() as i32;
        let margins = windows_sys::Win32::UI::Controls::MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: top,
            cyBottomHeight: 0,
        };
        unsafe {
            DwmExtendFrameIntoClientArea(hwnd, &margins);
        }
    }

    unsafe extern "system" fn procedure(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _: usize,
        data: usize,
    ) -> LRESULT {
        // Never allow a Rust panic to cross the OS callback boundary.
        let result =
            std::panic::catch_unwind(|| unsafe { dispatch(hwnd, message, wparam, lparam, data) });
        match result {
            Ok(value) => value,
            Err(_) => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
        }
    }

    unsafe fn dispatch(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        data: usize,
    ) -> LRESULT {
        if message == WM_NCDESTROY {
            unsafe {
                RemoveWindowSubclass(hwnd, Some(procedure), SUBCLASS);
                drop(Box::from_raw(data as *mut Arc<Mutex<HitRegions>>));
            }
            return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        }
        if message == WM_NCCALCSIZE && wparam != 0 {
            if unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32 & WS_CAPTION == 0 {
                return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
            }
            let params = unsafe { &mut *(lparam as *mut NCCALCSIZE_PARAMS) };
            if unsafe { IsZoomed(hwnd) } != 0 {
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if unsafe {
                    GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info)
                } != 0
                {
                    params.rgrc[0] = info.rcWork;
                }
            }
            return 0;
        }
        if message == WM_DPICHANGED || message == WM_DWMCOMPOSITIONCHANGED {
            unsafe {
                extend_frame(hwnd);
            }
        }
        if message == WM_NCHITTEST || message == WM_NCMOUSELEAVE {
            let mut result = 0;
            if unsafe { DwmDefWindowProc(hwnd, message, wparam, lparam, &mut result) } != 0 {
                return result;
            }
        }
        if message == WM_NCHITTEST {
            let mut point = POINT {
                x: (lparam as u32 & 0xffff) as i16 as i32,
                y: ((lparam as u32 >> 16) & 0xffff) as i16 as i32,
            };
            unsafe {
                ScreenToClient(hwnd, &mut point);
            }
            let mut rect = RECT::default();
            unsafe {
                GetClientRect(hwnd, &mut rect);
            }
            if unsafe { IsZoomed(hwnd) } == 0
                && unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32 & WS_THICKFRAME != 0
            {
                let dpi = unsafe { GetDpiForWindow(hwnd) };
                let edge = unsafe {
                    GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi)
                        + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
                };
                let left = point.x < edge;
                let right = point.x >= rect.right - edge;
                let top = point.y < edge;
                let bottom = point.y >= rect.bottom - edge;
                let hit = match (left, right, top, bottom) {
                    (true, _, true, _) => Some(HTTOPLEFT),
                    (_, true, true, _) => Some(HTTOPRIGHT),
                    (true, _, _, true) => Some(HTBOTTOMLEFT),
                    (_, true, _, true) => Some(HTBOTTOMRIGHT),
                    (true, _, _, _) => Some(HTLEFT),
                    (_, true, _, _) => Some(HTRIGHT),
                    (_, _, true, _) => Some(HTTOP),
                    (_, _, _, true) => Some(HTBOTTOM),
                    _ => None,
                };
                if let Some(hit) = hit {
                    return hit as LRESULT;
                }
            }
            let regions = unsafe { &*(data as *const Arc<Mutex<HitRegions>>) };
            if let Ok(regions) = regions.lock() {
                let pos = egui::pos2(
                    point.x as f32 / regions.scale.max(1.0),
                    point.y as f32 / regions.scale.max(1.0),
                );
                if regions.header.contains(pos)
                    && !regions.controls.iter().any(|rect| rect.contains(pos))
                {
                    return HTCAPTION as LRESULT;
                }
            }
            return HTCLIENT as LRESULT;
        }
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }
}
