//! Desktop running indicator overlay for CUA tasks.
//!
//! When an automated agent executes actions across the desktop, human operators
//! need immediate visual situational awareness without being obstructed:
//!
//! 1. **Zero-obstruction click-through**: `WS_EX_TRANSPARENT` allows mouse clicks
//!    and hovers to pass completely through to underlying application windows.
//! 2. **Non-stealing focus**: `WS_EX_TOOLWINDOW` hides the overlay from Alt-Tab
//!    and the taskbar, preventing it from stealing keyboard or cursor focus.
//! 3. **Model screenshot immunity**: `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
//!    instructs the Windows DWM to omit the overlay from all screen captures.
//!    Human operators see the status capsule, while vision models receive a
//!    completely clean screenshot with zero visual artifact contamination.
//! 4. **Graceful degradation**: On Linux or environments without a graphical
//!    desktop, the guard degrades to a no-op implementation with identical API.

use std::sync::Arc;

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::{spawn, JoinHandle};
    use std::time::Duration;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
        CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, RoundRect,
        SelectObject, SetBkMode, SetTextColor, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
        FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, FW_BOLD, FW_NORMAL, HBRUSH, HDC,
        HFONT, HGDIOBJ, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetSystemMetrics,
        KillTimer, PeekMessageW, PostQuitMessage, RegisterClassExW, SetLayeredWindowAttributes,
        SetTimer, SetWindowDisplayAffinity, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
        LWA_ALPHA, LWA_COLORKEY, PM_REMOVE, SM_CXSCREEN, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE,
        WM_CLOSE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    const PILL_WIDTH: i32 = 380;
    const PILL_HEIGHT: i32 = 46;
    const PILL_TOP: i32 = 14;

    #[derive(Clone, Debug)]
    struct State {
        step: usize,
        max_steps: usize,
        status: String,
        breath: u8,
        breath_dir: bool,
        /// Shared human-takeover state machine, polled each paint tick so the
        /// capsule recolors (green=running / amber=paused / red=stopped) the
        /// moment the operator seizes control. `None` when takeover is off.
        takeover: Option<Arc<super::super::takeover::TakeoverState>>,
    }

    /// Visual mode derived from the takeover state each paint tick.
    #[derive(Clone, Copy, PartialEq)]
    enum RunMode {
        /// Agent in control — green, breathing dot.
        Running,
        /// Human has seized the keyboard/mouse — amber, agent is parked.
        Paused,
        /// Task stopped outright — red, runner is winding down.
        Stopped,
    }

    impl RunMode {
        fn from_status(status: u8) -> Self {
            match status {
                super::super::takeover::PAUSED => RunMode::Paused,
                super::super::takeover::STOPPED => RunMode::Stopped,
                _ => RunMode::Running,
            }
        }

        /// Accent color (BGR) for the border, dot and status text.
        fn accent(&self) -> COLORREF {
            match self {
                // emerald  RGB(0,220,150)   -> BGR 0x0096DC00
                RunMode::Running => COLORREF(0x0096DC00),
                // amber    RGB(255,176,0)   -> BGR 0x0000B0FF
                RunMode::Paused => COLORREF(0x0000B0FF),
                // red      RGB(255,80,80)   -> BGR 0x005050FF
                RunMode::Stopped => COLORREF(0x005050FF),
            }
        }

        /// Status-line text shown when the agent is not the one in control.
        fn status_text(&self) -> &'static str {
            match self {
                // Empty means "let the runner's status show through".
                RunMode::Running => "",
                RunMode::Paused => "人工接管中 · agent 已暂停",
                RunMode::Stopped => "已停止 · 正在恢复现场",
            }
        }
    }

    enum Command {
        Update {
            step: usize,
            max_steps: usize,
            status: String,
        },
        Close,
    }

    pub struct WindowsOverlay {
        tx: Sender<Command>,
        alive: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl WindowsOverlay {
        pub fn start(
            task: &str,
            max_steps: usize,
            takeover: Option<Arc<super::super::takeover::TakeoverState>>,
        ) -> Option<Self> {
            let (tx, rx) = channel::<Command>();
            let alive = Arc::new(AtomicBool::new(true));
            let alive_clone = alive.clone();

            let initial_status = if task.is_empty() {
                "CUA 正在接管操控系统...".to_string()
            } else {
                task.chars().take(28).collect::<String>()
            };

            let (init_tx, init_rx) = channel::<bool>();

            let handle = spawn(move || {
                run_ui_loop(
                    rx,
                    init_tx,
                    alive_clone,
                    initial_status,
                    max_steps,
                    takeover,
                );
            });

            match init_rx.recv_timeout(Duration::from_millis(600)) {
                Ok(true) => Some(Self {
                    tx,
                    alive,
                    thread: Some(handle),
                }),
                _ => None,
            }
        }

        pub fn update(&self, step: usize, max_steps: usize, status: &str) {
            if self.alive.load(Ordering::SeqCst) {
                let _ = self.tx.send(Command::Update {
                    step,
                    max_steps,
                    status: status.chars().take(35).collect(),
                });
            }
        }

        pub fn close(&mut self) {
            if self.alive.load(Ordering::SeqCst) {
                let _ = self.tx.send(Command::Close);
                self.alive.store(false, Ordering::SeqCst);
            }
            if let Some(handle) = self.thread.take() {
                let _ = handle.join();
            }
        }
    }

    impl Drop for WindowsOverlay {
        fn drop(&mut self) {
            self.close();
        }
    }

    fn run_ui_loop(
        rx: Receiver<Command>,
        init_tx: Sender<bool>,
        alive: Arc<AtomicBool>,
        initial_status: String,
        max_steps: usize,
        takeover: Option<Arc<super::super::takeover::TakeoverState>>,
    ) {
        let state = Arc::new(Mutex::new(State {
            step: 1,
            max_steps,
            status: initial_status,
            breath: 0,
            breath_dir: true,
            takeover,
        }));

        unsafe {
            let class_name: Vec<u16> = "CUA_Overlay_Indicator\0".encode_utf16().collect();
            let wnd_class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: windows::Win32::Foundation::HINSTANCE(core::ptr::null_mut()),
                hIcon: windows::Win32::UI::WindowsAndMessaging::HICON(core::ptr::null_mut()),
                hCursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR(core::ptr::null_mut()),
                hbrBackground: HBRUSH(core::ptr::null_mut()),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                hIconSm: windows::Win32::UI::WindowsAndMessaging::HICON(core::ptr::null_mut()),
            };

            RegisterClassExW(&wnd_class);

            let screen_w = GetSystemMetrics(SM_CXSCREEN);
            let left = ((screen_w - PILL_WIDTH) / 2).max(10);

            // WS_EX_LAYERED + WS_EX_TRANSPARENT (Click-through) + WS_EX_TOOLWINDOW (No taskbar/Alt-Tab) + WS_EX_TOPMOST
            let ex_style = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;

            let hwnd = match CreateWindowExW(
                ex_style,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(
                    "CUA_Overlay_Pill\0"
                        .encode_utf16()
                        .collect::<Vec<_>>()
                        .as_ptr(),
                ),
                WS_POPUP,
                left,
                PILL_TOP,
                PILL_WIDTH,
                PILL_HEIGHT,
                None,
                None,
                None,
                None,
            ) {
                Ok(h) if !h.0.is_null() => h,
                _ => {
                    let _ = init_tx.send(false);
                    alive.store(false, Ordering::SeqCst);
                    return;
                }
            };

            // 1. Transparent colorkey 0x000000 (black pixels are completely see-through)
            // 2. Alpha 238 (~93% opacity for the capsule body)
            let _ = SetLayeredWindowAttributes(
                hwnd,
                COLORREF(0x00000000),
                238,
                LWA_COLORKEY | LWA_ALPHA,
            );

            // Exclude from all desktop captures (DWM hardware-level isolation):
            // Humans see the indicator on screen, but VLM models receive a 100% clean screenshot.
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);

            // Store state pointer in window userdata
            let state_raw = Box::into_raw(Box::new(state.clone()));
            windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                state_raw as isize,
            );

            // Smooth breath animation timer (70ms)
            let _ = SetTimer(Some(hwnd), 1, 70, None);

            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = init_tx.send(true);

            let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            'event_loop: loop {
                // Process pending window messages
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_CLOSE || msg.message == WM_DESTROY {
                        break 'event_loop;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                // Check for commands from the runner thread
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        Command::Update {
                            step,
                            max_steps,
                            status,
                        } => {
                            if let Ok(mut s) = state.lock() {
                                s.step = step;
                                s.max_steps = max_steps;
                                s.status = status;
                            }
                            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(
                                Some(hwnd),
                                None,
                                false,
                            );
                        }
                        Command::Close => {
                            break 'event_loop;
                        }
                    }
                }

                // Sleep briefly to yield CPU when idle (16ms ≈ 60Hz)
                std::thread::sleep(Duration::from_millis(16));
            }

            let _ = KillTimer(Some(hwnd), 1);
            let _ = DestroyWindow(hwnd);
        }
        alive.store(false, Ordering::SeqCst);
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *mut Arc<Mutex<State>>;

        match msg {
            WM_TIMER => {
                if !ptr.is_null() {
                    let arc = &*ptr;
                    if let Ok(mut s) = arc.lock() {
                        if s.breath_dir {
                            s.breath = s.breath.saturating_add(6);
                            if s.breath >= 180 {
                                s.breath_dir = false;
                            }
                        } else {
                            s.breath = s.breath.saturating_sub(6);
                            if s.breath <= 10 {
                                s.breath_dir = true;
                            }
                        }
                    }
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                if !ptr.is_null() {
                    let arc = &*ptr;
                    let (step, max_steps, status, breath, mode) = if let Ok(s) = arc.lock() {
                        let mode = match s.takeover.as_ref() {
                            Some(t) => RunMode::from_status(t.status()),
                            None => RunMode::Running,
                        };
                        (s.step, s.max_steps, s.status.clone(), s.breath, mode)
                    } else {
                        (0, 0, String::new(), 0, RunMode::Running)
                    };

                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(hwnd, &mut ps);
                    if !hdc.0.is_null() {
                        draw_overlay_pill(hdc, step, max_steps, &status, breath, mode);
                        let _ = EndPaint(hwnd, &ps);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if !ptr.is_null() {
                    let _ = Box::from_raw(ptr);
                    windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                        hwnd,
                        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                        0,
                    );
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn draw_overlay_pill(
        hdc: HDC,
        step: usize,
        max_steps: usize,
        status: &str,
        breath: u8,
        mode: RunMode,
    ) {
        // Double-buffering in memory DC to guarantee flicker-free rendering
        let mem_dc = CreateCompatibleDC(Some(hdc));
        let mem_bmp = CreateCompatibleBitmap(hdc, PILL_WIDTH, PILL_HEIGHT);
        let old_bmp = SelectObject(mem_dc, HGDIOBJ(mem_bmp.0));

        // 1. Fill entire window with black (0x000000), which SetLayeredWindowAttributes
        // treats as completely transparent / hollow.
        let black_brush = CreateSolidBrush(COLORREF(0x00000000));
        let full_rect = RECT {
            left: 0,
            top: 0,
            right: PILL_WIDTH,
            bottom: PILL_HEIGHT,
        };
        FillRect(mem_dc, &full_rect, black_brush);
        let _ = DeleteObject(HGDIOBJ(black_brush.0));

        // 2. Draw modern dark pill capsule: background RGB(22, 24, 30), border
        //    colored by run mode (green=running / amber=paused / red=stopped).
        let bg_brush = CreateSolidBrush(COLORREF(0x001E1816)); // BGR format: R=22, G=24, B=30
        let border_pen = CreatePen(PS_SOLID, 1, mode.accent());
        let old_brush = SelectObject(mem_dc, HGDIOBJ(bg_brush.0));
        let old_pen = SelectObject(mem_dc, HGDIOBJ(border_pen.0));

        let _ = RoundRect(mem_dc, 2, 2, PILL_WIDTH - 2, PILL_HEIGHT - 2, 24, 24);

        SelectObject(mem_dc, old_brush);
        SelectObject(mem_dc, old_pen);
        let _ = DeleteObject(HGDIOBJ(bg_brush.0));
        let _ = DeleteObject(HGDIOBJ(border_pen.0));

        // 3. Pulsing indicator dot in the accent color. Running breathes; a
        //    parked (paused) or stopped task holds a steady, non-breathing dot.
        let accent = mode.accent().0; // 0x00BBGGRR
        let base_g = (accent >> 8) & 0xFF;
        let base_r = accent & 0xFF;
        let pulse = if mode == RunMode::Running {
            breath as u32
        } else {
            0
        };
        let g_val = (base_g + pulse).min(255);
        let r_val = (base_r + pulse / 4).min(255);
        let dot_color = COLORREF((g_val << 8) | r_val);
        let dot_brush = CreateSolidBrush(dot_color);
        let null_pen = CreatePen(PS_SOLID, 0, dot_color);
        let old_b = SelectObject(mem_dc, HGDIOBJ(dot_brush.0));
        let old_p = SelectObject(mem_dc, HGDIOBJ(null_pen.0));

        let _ = windows::Win32::Graphics::Gdi::Ellipse(mem_dc, 16, 17, 28, 29);

        SelectObject(mem_dc, old_b);
        SelectObject(mem_dc, old_p);
        let _ = DeleteObject(HGDIOBJ(dot_brush.0));
        let _ = DeleteObject(HGDIOBJ(null_pen.0));

        // 4. Texts: Title, Step, and Status
        SetBkMode(mem_dc, TRANSPARENT);

        // Top line: Bold title
        let font_title = create_gdi_font(12, FW_BOLD);
        let old_f = SelectObject(mem_dc, HGDIOBJ(font_title.0));
        SetTextColor(mem_dc, COLORREF(0x00FFFFFF)); // White

        let title_text: Vec<u16> = match mode {
            RunMode::Running => format!("CUA-HUB 正在操控 · Step {}/{}", step, max_steps),
            RunMode::Paused => format!("CUA-HUB 人工接管 · Step {}/{}", step, max_steps),
            RunMode::Stopped => format!("CUA-HUB 已停止 · Step {}/{}", step, max_steps),
        }
        .encode_utf16()
        .collect();
        let mut title_rect = RECT {
            left: 38,
            top: 7,
            right: PILL_WIDTH - 80,
            bottom: 23,
        };
        DrawTextW(
            mem_dc,
            &mut title_text.clone(),
            &mut title_rect,
            DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER,
        );

        // Bottom line: Action status
        let font_sub = create_gdi_font(10, FW_NORMAL);
        SelectObject(mem_dc, HGDIOBJ(font_sub.0));
        SetTextColor(mem_dc, mode.accent());

        // A paused/stopped task shows the takeover label instead of whatever
        // action the runner last reported — the agent is no longer acting.
        let status_str = match mode {
            RunMode::Running => {
                if status.is_empty() {
                    "正在规划下一步..."
                } else {
                    status
                }
            }
            _ => mode.status_text(),
        };
        let mut sub_text: Vec<u16> = status_str.encode_utf16().collect();
        let mut sub_rect = RECT {
            left: 38,
            top: 24,
            right: PILL_WIDTH - 80,
            bottom: 39,
        };
        DrawTextW(
            mem_dc,
            &mut sub_text,
            &mut sub_rect,
            DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER,
        );

        // Right badge: PASSTHROUGH tag
        let font_badge = create_gdi_font(9, FW_NORMAL);
        SelectObject(mem_dc, HGDIOBJ(font_badge.0));
        SetTextColor(mem_dc, COLORREF(0x00A0A0A0)); // Light grey
        let mut badge_text: Vec<u16> = "点击穿透".encode_utf16().collect();
        let mut badge_rect = RECT {
            left: PILL_WIDTH - 76,
            top: 15,
            right: PILL_WIDTH - 12,
            bottom: 31,
        };
        DrawTextW(
            mem_dc,
            &mut badge_text,
            &mut badge_rect,
            DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER,
        );

        // Blit back to target DC
        let _ = BitBlt(
            hdc,
            0,
            0,
            PILL_WIDTH,
            PILL_HEIGHT,
            Some(mem_dc),
            0,
            0,
            SRCCOPY,
        );

        // Cleanup GDI objects
        SelectObject(mem_dc, old_f);
        let _ = DeleteObject(HGDIOBJ(font_title.0));
        let _ = DeleteObject(HGDIOBJ(font_sub.0));
        let _ = DeleteObject(HGDIOBJ(font_badge.0));

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(mem_bmp.0));
        let _ = DeleteDC(mem_dc);
    }

    unsafe fn create_gdi_font(
        pt_size: i32,
        weight: windows::Win32::Graphics::Gdi::FONT_WEIGHT,
    ) -> HFONT {
        let font_name: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
        CreateFontW(
            -pt_size,
            0,
            0,
            0,
            weight.0 as i32,
            0,
            0,
            0,
            windows::Win32::Graphics::Gdi::FONT_CHARSET(1), // DEFAULT_CHARSET
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(2), // CLEARTYPE_QUALITY
            0,
            PCWSTR(font_name.as_ptr()),
        )
    }
}

/// RAII guard managing a desktop overlay status indicator during task execution.
///
/// When active, a non-intrusive capsule indicator floats at the top of the
/// primary display, pulsing to signal that CUA automation is in progress.
/// Mouse clicks pass straight through (`WS_EX_TRANSPARENT`), the window never
/// steals keyboard focus (`WS_EX_TOOLWINDOW`), and `WDA_EXCLUDEFROMCAPTURE`
/// isolates it from vision models' screen observations.
///
/// Dropping this guard cleanly destroys the overlay window and terminates
/// the UI thread.
pub struct OverlayGuard {
    enabled: bool,
    #[cfg(windows)]
    inner: Option<imp::WindowsOverlay>,
}

impl OverlayGuard {
    /// Starts the overlay indicator if enabled.
    ///
    /// Respects the `CUA_HUB_OVERLAY` environment variable (`0`/`false`/`off` disables).
    /// Returns a no-op guard when disabled or on unsupported platforms.
    ///
    /// `takeover` links the capsule to the human-takeover state machine so it
    /// recolors as the operator seizes and releases control.
    pub fn start(
        enabled: bool,
        task: &str,
        max_steps: usize,
        takeover: Option<Arc<crate::takeover::TakeoverState>>,
    ) -> Self {
        let effective_enabled = enabled && overlay_env_enabled();
        if !effective_enabled {
            return Self {
                enabled: false,
                #[cfg(windows)]
                inner: None,
            };
        }

        #[cfg(windows)]
        {
            let inner = imp::WindowsOverlay::start(task, max_steps, takeover);
            Self {
                enabled: inner.is_some(),
                inner,
            }
        }
        #[cfg(not(windows))]
        {
            let _ = takeover;
            Self { enabled: false }
        }
    }

    /// Updates the displayed step number and active status description.
    pub fn update(&self, step: usize, max_steps: usize, status: &str) {
        if !self.enabled {
            return;
        }
        #[cfg(windows)]
        if let Some(inner) = &self.inner {
            inner.update(step, max_steps, status);
        }
    }

    /// Whether the overlay window is currently active on screen.
    pub fn is_active(&self) -> bool {
        self.enabled
    }
}

/// Parse `CUA_HUB_OVERLAY` environment variable. Defaults to `true`.
pub fn overlay_env_enabled() -> bool {
    parse_overlay_env(std::env::var("CUA_HUB_OVERLAY").ok().as_deref())
}

/// Pure helper to parse the environment variable value string.
pub fn parse_overlay_env(raw: Option<&str>) -> bool {
    match raw {
        Some(v) => {
            let s = v.trim().to_ascii_lowercase();
            !matches!(s.as_str(), "0" | "false" | "off" | "no")
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_overlay_constructs_safely_as_noop() {
        let guard = OverlayGuard::start(false, "sample", 10, None);
        assert!(!guard.is_active());
        // update must not panic
        guard.update(1, 10, "doing something");
    }

    #[test]
    fn env_variable_parsing_matches_standard_conventions() {
        assert!(!parse_overlay_env(Some("0")));
        assert!(!parse_overlay_env(Some("false")));
        assert!(!parse_overlay_env(Some("off")));
        assert!(!parse_overlay_env(Some("no")));
        assert!(!parse_overlay_env(Some(" FALSE ")));
        assert!(parse_overlay_env(Some("1")));
        assert!(parse_overlay_env(Some("true")));
        assert!(parse_overlay_env(Some("on")));
        assert!(parse_overlay_env(None), "default is enabled");
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "real-machine: opens a real Win32 overlay window on the desktop"]
    fn real_machine_overlay_lifecycle() {
        let guard = OverlayGuard::start(true, "测试覆盖层指示器", 5, None);
        assert!(
            guard.is_active(),
            "overlay window should be active on Windows desktop"
        );
        guard.update(2, 5, "正在执行物理验证...");
        std::thread::sleep(std::time::Duration::from_millis(600));
        drop(guard);
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "real-machine: installs low-level hooks + global hotkeys; press Ctrl+Alt+H to pause, move the mouse to resume"]
    fn real_machine_takeover_lifecycle() {
        let takeover = crate::takeover::TakeoverGuard::start(true);
        assert!(
            takeover.is_enabled(),
            "takeover hooks should install on desktop"
        );
        let st = takeover.state();
        assert_eq!(st.status(), crate::takeover::RUNNING);

        // The gate passes while running.
        assert!(takeover.wait_gate());

        // Simulate the takeover thread observing a human: pause, then confirm
        // the gate blocks, then resume via the idle path.
        st.note_human_input(crate::takeover::now_ms());
        assert_eq!(st.status(), crate::takeover::PAUSED);
        // wait_gate polls; auto-resume kicks in after the idle threshold, so
        // this returns true within a few seconds rather than blocking forever.
        assert!(takeover.wait_gate(), "agent should auto-resume after idle");
        assert_eq!(st.status(), crate::takeover::RUNNING);

        // Manual stop is one-way: the gate then reports false.
        st.stop();
        assert!(!takeover.wait_gate());
        assert_eq!(st.status(), crate::takeover::STOPPED);
    }
}
