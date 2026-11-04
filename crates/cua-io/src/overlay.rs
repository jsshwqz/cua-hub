//! Interactive desktop control bar for supervision mode.
//!
//! The bar replaces the old passive click-through capsule. It is a small
//! always-on-top pill that the operator can actually interact with:
//!
//! 1. **Buttons**: ⏻ turns supervision off (pure background: the bar hides
//!    and the takeover hooks stand down); ✕ gracefully ends the process —
//!    the runner finishes its current step, writes the audit trail and exits.
//! 2. **Drag**: the blank area acts as a caption bar — drag anywhere outside
//!    the buttons to move; the position persists across restarts.
//! 3. **Capture immunity**: `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
//!    still keeps the bar out of every screenshot, so vision models never see
//!    it and never click "through" it by accident — while it stays clickable
//!    for humans; capture exclusion and interactivity do not conflict.
//! 4. **Action transparency**: while the agent is executing an input action
//!    the bar temporarily re-enables `WS_EX_TRANSPARENT`, so an injected
//!    click aimed at a target under the bar lands on the real target. The
//!    moment the action finishes the bar becomes interactive again.
//! 5. **Not bound to one task**: the bar is a process-level singleton — in
//!    serve mode it persists across tasks and shows a standby state between
//!    them; `Ctrl+Alt+O` shows/hides it at any time.
//!
//! Non-Windows platforms get a no-op implementation with the same API.

#[cfg(windows)]
pub use imp::WindowsBar;

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::spawn;
    use std::time::Duration;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
        CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, MoveToEx,
        RoundRect, SelectObject, SetBkMode, SetTextColor, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE,
        DT_VCENTER, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, FW_BOLD, FW_NORMAL,
        HBRUSH, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetSystemMetrics,
        GetWindowLongPtrW, GetWindowRect, KillTimer, PeekMessageW, PostQuitMessage,
        RegisterClassExW, SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity,
        SetWindowLongPtrW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
        GWL_EXSTYLE, HTCAPTION, HTCLIENT, LWA_ALPHA, LWA_COLORKEY, PM_REMOVE, SM_CXSCREEN,
        SM_CYSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE, WM_CLOSE, WM_DESTROY,
        WM_EXITSIZEMOVE, WM_LBUTTONDOWN, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSEXW,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
        WS_POPUP,
    };

    pub const BAR_WIDTH: i32 = 380;
    pub const BAR_HEIGHT: i32 = 46;

    /// The ⏻ (supervision on/off) button rect, in client coordinates.
    pub fn power_rect() -> RECT {
        RECT {
            left: BAR_WIDTH - 78,
            top: 7,
            right: BAR_WIDTH - 44,
            bottom: 39,
        }
    }

    /// The ✕ (graceful end) button rect, in client coordinates.
    pub fn close_rect() -> RECT {
        RECT {
            left: BAR_WIDTH - 40,
            top: 7,
            right: BAR_WIDTH - 8,
            bottom: 39,
        }
    }

    pub fn rect_contains(r: &RECT, x: i32, y: i32) -> bool {
        x >= r.left && x < r.right && y >= r.top && y < r.bottom
    }

    /// Which interactive zone a client point falls into. Pure, testable.
    #[derive(Clone, Copy, PartialEq, Debug)]
    pub enum Zone {
        Power,
        Close,
        /// Blank area: drag handle (OS caption semantics).
        Drag,
    }

    pub fn zone_at(x: i32, y: i32) -> Zone {
        if rect_contains(&power_rect(), x, y) {
            Zone::Power
        } else if rect_contains(&close_rect(), x, y) {
            Zone::Close
        } else {
            Zone::Drag
        }
    }

    /// Unpacks packed signed shorts from an lParam (negative on multi-monitor).
    pub fn point_from_lparam(lparam: LPARAM) -> POINT {
        POINT {
            x: (lparam.0 & 0xFFFF) as u16 as i16 as i32,
            y: ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
        }
    }

    /// Converts a screen point to this window's client coordinates. The bar is
    /// a borderless `WS_POPUP`, so the client origin equals the window origin.
    unsafe fn screen_to_client(hwnd: HWND, pt: &mut POINT) {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            pt.x -= rect.left;
            pt.y -= rect.top;
        }
    }

    #[derive(Clone, Debug)]
    struct State {
        step: usize,
        max_steps: usize,
        status: String,
        /// True between tasks: the bar shows its standby face.
        idle: bool,
        breath: u8,
        breath_dir: bool,
        takeover: Arc<crate::takeover::TakeoverState>,
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
                crate::takeover::PAUSED => RunMode::Paused,
                crate::takeover::STOPPED => RunMode::Stopped,
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
        /// A new supervised task attached to the bar.
        Begin { task: String, max_steps: usize },
        /// Task over: show the standby face (the bar outlives the task).
        Idle(String),
    }

    pub struct WindowsBar {
        tx: Sender<Command>,
        alive: Arc<AtomicBool>,
    }

    impl WindowsBar {
        pub fn start(
            task: &str,
            max_steps: usize,
            takeover: Arc<crate::takeover::TakeoverState>,
            visible: Arc<AtomicBool>,
            action: Arc<AtomicBool>,
        ) -> Option<Self> {
            let (tx, rx) = channel::<Command>();
            let alive = Arc::new(AtomicBool::new(true));
            let alive_clone = alive.clone();

            let initial_status = if task.is_empty() {
                "监督待机中".to_string()
            } else {
                task.chars().take(28).collect::<String>()
            };
            // An empty task hint means the bar was mounted process-wide
            // (serve --supervise) rather than for a specific run: start in
            // standby instead of showing "Step 1/0".
            let start_idle = task.is_empty();

            let (init_tx, init_rx) = channel::<bool>();

            // The bar thread lives for the process (the hub is a singleton);
            // detaching the handle is deliberate — there is no join path.
            spawn(move || {
                run_bar_loop(
                    rx,
                    init_tx,
                    alive_clone,
                    initial_status,
                    start_idle,
                    max_steps,
                    takeover,
                    visible,
                    action,
                );
            });

            match init_rx.recv_timeout(Duration::from_millis(800)) {
                Ok(true) => Some(Self { tx, alive }),
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

        pub fn begin_task(&self, task: &str, max_steps: usize) {
            if self.alive.load(Ordering::SeqCst) {
                let _ = self.tx.send(Command::Begin {
                    task: task.chars().take(28).collect(),
                    max_steps,
                });
            }
        }

        pub fn end_task(&self, summary: &str) {
            if self.alive.load(Ordering::SeqCst) {
                let _ = self
                    .tx
                    .send(Command::Idle(summary.chars().take(35).collect()));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_bar_loop(
        rx: Receiver<Command>,
        init_tx: Sender<bool>,
        alive: Arc<AtomicBool>,
        initial_status: String,
        start_idle: bool,
        max_steps: usize,
        takeover: Arc<crate::takeover::TakeoverState>,
        visible: Arc<AtomicBool>,
        action: Arc<AtomicBool>,
    ) {
        let state = Arc::new(Mutex::new(State {
            step: 0,
            max_steps,
            status: initial_status,
            idle: start_idle,
            breath: 0,
            breath_dir: true,
            takeover,
        }));

        unsafe {
            let class_name: Vec<u16> = "CUA_Supervise_Bar\0".encode_utf16().collect();
            let wnd_class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(bar_wnd_proc),
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
            let screen_h = GetSystemMetrics(SM_CYSCREEN);
            // Restore the persisted position (dragged last run) or default to
            // top-centre; either way keep the whole bar on screen.
            let (left, top) = crate::supervise::load_saved_position()
                .map(|(x, y)| {
                    crate::supervise::clamp_position(
                        x, y, BAR_WIDTH, BAR_HEIGHT, screen_w, screen_h,
                    )
                })
                .unwrap_or_else(|| (((screen_w - BAR_WIDTH) / 2).max(10), 14));

            // Interactive: no WS_EX_TRANSPARENT here — the bit is toggled
            // dynamically while the agent executes an input action. NOACTIVATE
            // keeps button clicks from stealing keyboard focus.
            let ex_style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;

            let hwnd = match CreateWindowExW(
                ex_style,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(
                    "CUA_Supervise_Bar\0"
                        .encode_utf16()
                        .collect::<Vec<_>>()
                        .as_ptr(),
                ),
                WS_POPUP,
                left,
                top,
                BAR_WIDTH,
                BAR_HEIGHT,
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
            // 2. Alpha 238 (~93% opacity for the bar body)
            let _ = SetLayeredWindowAttributes(
                hwnd,
                COLORREF(0x00000000),
                238,
                LWA_COLORKEY | LWA_ALPHA,
            );

            // Exclude from all desktop captures (DWM hardware-level isolation):
            // humans see and click the bar, models receive a clean screenshot.
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);

            let state_raw = Box::into_raw(Box::new(state.clone()));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_raw as isize);

            // Smooth breath animation timer (70ms)
            let _ = SetTimer(Some(hwnd), 1, 70, None);

            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = init_tx.send(true);

            let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            let mut last_shown = true;
            let mut last_transparent = false;
            'event_loop: loop {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_CLOSE || msg.message == WM_DESTROY {
                        break 'event_loop;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                // Commands from the runner thread.
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
                                s.idle = false;
                            }
                            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(
                                Some(hwnd),
                                None,
                                false,
                            );
                        }
                        Command::Begin { task, max_steps } => {
                            if let Ok(mut s) = state.lock() {
                                s.max_steps = max_steps;
                                s.status = if task.is_empty() {
                                    "监督待机中".to_string()
                                } else {
                                    task
                                };
                                s.idle = false;
                                s.step = 0;
                            }
                            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(
                                Some(hwnd),
                                None,
                                false,
                            );
                        }
                        Command::Idle(summary) => {
                            if let Ok(mut s) = state.lock() {
                                s.status = summary;
                                s.idle = true;
                            }
                            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(
                                Some(hwnd),
                                None,
                                false,
                            );
                        }
                    }
                }

                // Show/hide transitions (⏻ off, Ctrl+Alt+O, end-of-run hide).
                let shown = visible.load(Ordering::Relaxed);
                if shown != last_shown {
                    let _ = ShowWindow(hwnd, if shown { SW_SHOWNOACTIVATE } else { SW_HIDE });
                    last_shown = shown;
                }

                // Click-through while the agent executes an input action, so
                // injected clicks aimed under the bar land on the real target.
                let transparent = action.load(Ordering::Relaxed);
                if transparent != last_transparent {
                    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as i32;
                    let ex = if transparent {
                        ex | WS_EX_TRANSPARENT.0 as i32
                    } else {
                        ex & !(WS_EX_TRANSPARENT.0 as i32)
                    };
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
                    last_transparent = transparent;
                }

                std::thread::sleep(Duration::from_millis(16));
            }

            let _ = KillTimer(Some(hwnd), 1);
            let _ = DestroyWindow(hwnd);
            alive.store(false, Ordering::SeqCst);
        }
    }

    unsafe extern "system" fn bar_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Arc<Mutex<State>>;

        match msg {
            WM_NCHITTEST => {
                // Buttons are clickable (HTCLIENT); the rest of the bar is the
                // drag handle (HTCAPTION → OS drag + WM_EXITSIZEMOVE on drop).
                let mut pt = point_from_lparam(lparam);
                screen_to_client(hwnd, &mut pt);
                match zone_at(pt.x, pt.y) {
                    Zone::Drag => LRESULT(HTCAPTION as isize),
                    _ => LRESULT(HTCLIENT as isize),
                }
            }
            WM_LBUTTONDOWN => {
                let mut pt = point_from_lparam(lparam);
                screen_to_client(hwnd, &mut pt);
                match zone_at(pt.x, pt.y) {
                    Zone::Power => crate::supervise::turn_off(),
                    Zone::Close => crate::supervise::request_quit(),
                    Zone::Drag => {}
                }
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                // OS drag finished: persist the new position for next time.
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    crate::supervise::persist_position(rect.left, rect.top);
                }
                LRESULT(0)
            }
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
                    let (step, max_steps, status, breath, mode, idle) = if let Ok(s) = arc.lock() {
                        let mode = RunMode::from_status(s.takeover.status());
                        (
                            s.step,
                            s.max_steps,
                            s.status.clone(),
                            s.breath,
                            mode,
                            s.idle,
                        )
                    } else {
                        (0, 0, String::new(), 0, RunMode::Running, false)
                    };

                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(hwnd, &mut ps);
                    if !hdc.0.is_null() {
                        draw_bar(hdc, step, max_steps, &status, breath, mode, idle);
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
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn draw_bar(
        hdc: HDC,
        step: usize,
        max_steps: usize,
        status: &str,
        breath: u8,
        mode: RunMode,
        idle: bool,
    ) {
        // Double-buffering in memory DC to guarantee flicker-free rendering
        let mem_dc = CreateCompatibleDC(Some(hdc));
        let mem_bmp = CreateCompatibleBitmap(hdc, BAR_WIDTH, BAR_HEIGHT);
        let old_bmp = SelectObject(mem_dc, HGDIOBJ(mem_bmp.0));

        // 1. Fill entire window with black (0x000000), which SetLayeredWindowAttributes
        // treats as completely transparent / hollow.
        let black_brush = CreateSolidBrush(COLORREF(0x00000000));
        let full_rect = RECT {
            left: 0,
            top: 0,
            right: BAR_WIDTH,
            bottom: BAR_HEIGHT,
        };
        FillRect(mem_dc, &full_rect, black_brush);
        let _ = DeleteObject(HGDIOBJ(black_brush.0));

        // 2. Dark pill body with a mode-colored border.
        let bg_brush = CreateSolidBrush(COLORREF(0x001E1816)); // BGR: R=22, G=24, B=30
        let border_pen = CreatePen(PS_SOLID, 1, mode.accent());
        let old_brush = SelectObject(mem_dc, HGDIOBJ(bg_brush.0));
        let old_pen = SelectObject(mem_dc, HGDIOBJ(border_pen.0));

        let _ = RoundRect(mem_dc, 2, 2, BAR_WIDTH - 2, BAR_HEIGHT - 2, 24, 24);

        SelectObject(mem_dc, old_brush);
        SelectObject(mem_dc, old_pen);
        let _ = DeleteObject(HGDIOBJ(bg_brush.0));
        let _ = DeleteObject(HGDIOBJ(border_pen.0));

        // 3. Status dot in the accent color; running breathes, others hold.
        let accent = mode.accent().0; // 0x00BBGGRR
        let base_g = (accent >> 8) & 0xFF;
        let base_r = accent & 0xFF;
        let pulse = if mode == RunMode::Running && !idle {
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

        // 4. Texts: title and status line.
        SetBkMode(mem_dc, TRANSPARENT);

        let font_title = create_gdi_font(12, FW_BOLD);
        SelectObject(mem_dc, HGDIOBJ(font_title.0));
        SetTextColor(mem_dc, COLORREF(0x00FFFFFF)); // White

        let title_text: Vec<u16> = match (idle, mode) {
            (true, _) => "CUA-HUB 监督待机".to_string(),
            (false, RunMode::Running) => format!("CUA-HUB 监督中 · Step {}/{}", step, max_steps),
            (false, RunMode::Paused) => format!("CUA-HUB 人工接管 · Step {}/{}", step, max_steps),
            (false, RunMode::Stopped) => "CUA-HUB 已停止".to_string(),
        }
        .encode_utf16()
        .collect();
        let mut title_rect = RECT {
            left: 38,
            top: 7,
            right: power_rect().left - 8,
            bottom: 23,
        };
        DrawTextW(
            mem_dc,
            &mut title_text.clone(),
            &mut title_rect,
            DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER,
        );

        let font_sub = create_gdi_font(10, FW_NORMAL);
        SelectObject(mem_dc, HGDIOBJ(font_sub.0));
        SetTextColor(mem_dc, mode.accent());

        let status_str = match mode {
            RunMode::Running => {
                if idle {
                    "Ctrl+Alt+O 显隐 · 拖动空白移动 · ⏻ 关闭 · ✕ 结束"
                } else if status.is_empty() {
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
            right: power_rect().left - 8,
            bottom: 39,
        };
        DrawTextW(
            mem_dc,
            &mut sub_text,
            &mut sub_rect,
            DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER,
        );

        // 5. Buttons: subtle raised squares with white vector icons.
        draw_button_chrome(mem_dc, power_rect());
        draw_power_icon(mem_dc, power_rect());
        draw_button_chrome(mem_dc, close_rect());
        draw_close_icon(mem_dc, close_rect());

        // Blit back to target DC
        let _ = BitBlt(
            hdc,
            0,
            0,
            BAR_WIDTH,
            BAR_HEIGHT,
            Some(mem_dc),
            0,
            0,
            SRCCOPY,
        );

        let _ = DeleteObject(HGDIOBJ(font_title.0));
        let _ = DeleteObject(HGDIOBJ(font_sub.0));

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(mem_bmp.0));
        let _ = DeleteDC(mem_dc);
    }

    /// Raised button chrome: lighter body + grey border so both icons read
    /// against the dark pill.
    unsafe fn draw_button_chrome(hdc: HDC, rect: RECT) {
        let body = CreateSolidBrush(COLORREF(0x00302C28)); // BGR: R=40, G=44, B=48
        let pen = CreatePen(PS_SOLID, 1, COLORREF(0x005A6A78)); // grey-blue border
        let old_brush = SelectObject(hdc, HGDIOBJ(body.0));
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, 10, 10);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(HGDIOBJ(body.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }

    /// Power icon: circle outline with a vertical stem through the top.
    unsafe fn draw_power_icon(hdc: HDC, rect: RECT) {
        let white = COLORREF(0x00FFFFFF);
        let pen = CreatePen(PS_SOLID, 2, white);
        let null_brush = get_stock_null_brush();
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let old_brush = SelectObject(hdc, HGDIOBJ(null_brush.0));

        // Circle body, inset to leave room for the stem.
        let _ = windows::Win32::Graphics::Gdi::Ellipse(
            hdc,
            rect.left + 6,
            rect.top + 9,
            rect.right - 6,
            rect.bottom - 4,
        );
        // Vertical stem.
        let cx = (rect.left + rect.right) / 2;
        let _ = MoveToEx(hdc, cx, rect.top + 4, None);
        let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, cx, (rect.top + rect.bottom) / 2 + 2);

        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }

    /// Close icon: a bold ✕.
    unsafe fn draw_close_icon(hdc: HDC, rect: RECT) {
        let white = COLORREF(0x00FFFFFF);
        let pen = CreatePen(PS_SOLID, 2, white);
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));

        let _ = MoveToEx(hdc, rect.left + 11, rect.top + 10, None);
        let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, rect.right - 11, rect.bottom - 10);
        let _ = MoveToEx(hdc, rect.right - 11, rect.top + 10, None);
        let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, rect.left + 11, rect.bottom - 10);

        SelectObject(hdc, old_pen);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }

    // NULL_BRUSH stock object for outline-only icon shapes.
    unsafe fn get_stock_null_brush() -> windows::Win32::Graphics::Gdi::HBRUSH {
        windows::Win32::Graphics::Gdi::HBRUSH(
            windows::Win32::Graphics::Gdi::GetStockObject(
                windows::Win32::Graphics::Gdi::NULL_BRUSH,
            )
            .0,
        )
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

#[cfg(all(test, windows))]
mod tests {
    use super::imp::{
        close_rect, point_from_lparam, power_rect, rect_contains, zone_at, Zone, BAR_HEIGHT,
        BAR_WIDTH,
    };

    #[test]
    fn buttons_live_inside_the_bar() {
        for r in [power_rect(), close_rect()] {
            assert!(r.left >= 0 && r.right <= BAR_WIDTH, "button x within bar");
            assert!(r.top >= 0 && r.bottom <= BAR_HEIGHT, "button y within bar");
            assert!(
                r.left < r.right && r.top < r.bottom,
                "button rect non-empty"
            );
        }
        // The two buttons must not overlap.
        let p = power_rect();
        let c = close_rect();
        assert!(
            p.right <= c.left,
            "power and close buttons must not overlap"
        );
    }

    #[test]
    fn zone_hit_testing_routes_clicks() {
        let p = power_rect();
        let c = close_rect();
        assert_eq!(zone_at(p.left + 2, p.top + 2), Zone::Power);
        assert_eq!(zone_at(c.left + 2, c.top + 2), Zone::Close);
        // Everything else is the drag handle.
        assert_eq!(zone_at(20, 20), Zone::Drag);
        assert_eq!(zone_at(200, 23), Zone::Drag);
        // Boundary: right-exclusive.
        assert_eq!(zone_at(p.right, p.top + 2), Zone::Drag);
    }

    #[test]
    fn rect_contains_matches_zone_routing() {
        let c = close_rect();
        assert!(rect_contains(&c, c.left, c.top));
        assert!(!rect_contains(&c, c.right, c.top));
        assert!(!rect_contains(&c, c.left - 1, c.top));
    }

    #[test]
    fn packed_lparam_decodes_signed_screen_coords() {
        use windows::Win32::Foundation::LPARAM;
        // -1 encoded as unsigned 16-bit in both halves (multi-monitor edge).
        let lp = LPARAM(0xFFFF_FFFFu32 as isize);
        let pt = point_from_lparam(lp);
        assert_eq!((pt.x, pt.y), (-1, -1));
        let lp = LPARAM(((200u32) << 16 | 100u32) as isize);
        let pt = point_from_lparam(lp);
        assert_eq!((pt.x, pt.y), (100, 200));
    }
}
