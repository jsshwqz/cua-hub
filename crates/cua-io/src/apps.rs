//! Application discovery and launch.
//!
//! Window lists answer "what is open", but an agent that can only act on
//! already-open windows cannot start a workflow: the user says "check this in
//! FreeCAD" and the app is not running. These helpers give the same view an
//! operating system shell does — running apps with their windows, plus the
//! launchable entries the Start Menu knows about — and a launch that waits for
//! the window to actually appear instead of returning as soon as the process
//! was spawned.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cua_core::{Error, Result};

use crate::capture::{list_windows, WindowInfo};

/// Where an app entry came from. `StartMenu` entries are not running and exist
/// only so `launch_app` can start them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSource {
    Running,
    StartMenu,
}

/// One window belonging to an app.
#[derive(Debug, Clone)]
pub struct AppWindow {
    pub id: u32,
    pub title: String,
    pub minimized: bool,
    pub focused: bool,
}

/// An application that can be targeted or launched.
#[derive(Debug, Clone)]
pub struct AppInfo {
    /// Launchable identifier: an `.exe` path for running apps, a `.lnk` path for
    /// Start Menu entries. This is what `launch_app` accepts.
    pub id: String,
    pub display_name: String,
    pub pid: Option<u32>,
    pub running: bool,
    pub source: AppSource,
    pub windows: Vec<AppWindow>,
}

/// Outcome of a launch request.
#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    /// What was actually launched or matched.
    pub id: String,
    pub display_name: String,
    /// True when the app was already open, so nothing was started.
    pub already_running: bool,
    pub pid: Option<u32>,
    /// How the returned windows were attributed to the launch: `new_process`,
    /// `matching_window`, `single_new_window`, `already_running` or `none`.
    pub matched_by: String,
    /// Windows belonging to the app once it was ready. May be empty if the app
    /// did not show a window within the timeout (splash-only, tray-only, or a
    /// launch that failed silently).
    pub windows: Vec<AppWindow>,
    /// True when the window appeared within the timeout.
    pub ready: bool,
}

fn app_window(w: &WindowInfo) -> AppWindow {
    AppWindow {
        id: w.id,
        title: w.title.clone(),
        minimized: w.minimized,
        focused: w.focused,
    }
}

/// Running apps grouped by process, each with the windows it owns.
///
/// `include_all` additionally lists Start Menu shortcuts for apps that are not
/// running, which is what makes "launch X" possible for an X that is closed.
pub fn list_apps(include_all: bool) -> Result<Vec<AppInfo>> {
    let windows = list_windows(None, true)?;
    // BTreeMap keeps the output ordered by display name, so a caller comparing
    // two listings is not chasing noise.
    let mut by_name: BTreeMap<String, AppInfo> = BTreeMap::new();

    #[cfg(windows)]
    let mut by_pid: BTreeMap<u32, Vec<WindowInfo>> = BTreeMap::new();
    #[cfg(windows)]
    for w in &windows {
        by_pid.entry(w.pid).or_default().push(w.clone());
    }

    #[cfg(windows)]
    for (pid, wins) in &by_pid {
        let path = imp::process_path(*pid);
        let display_name = path
            .as_ref()
            .map(|p| file_stem(p))
            .unwrap_or_else(|| wins[0].app.clone());
        let id = path.clone().unwrap_or_else(|| wins[0].app.clone());
        let key = display_name.to_lowercase();
        let entry = by_name.entry(key).or_insert_with(|| AppInfo {
            id,
            display_name,
            pid: Some(*pid),
            running: true,
            source: AppSource::Running,
            windows: Vec::new(),
        });
        entry.windows.extend(wins.iter().map(app_window));
    }

    #[cfg(not(windows))]
    {
        let mut by_app: BTreeMap<String, Vec<WindowInfo>> = BTreeMap::new();
        for w in &windows {
            by_app.entry(w.app.clone()).or_default().push(w.clone());
        }
        for (name, wins) in by_app {
            by_name.insert(
                name.to_lowercase(),
                AppInfo {
                    id: name.clone(),
                    display_name: name,
                    pid: Some(wins[0].pid),
                    running: true,
                    source: AppSource::Running,
                    windows: wins.iter().map(app_window).collect(),
                },
            );
        }
    }

    if include_all {
        for (stem, path) in start_menu_entries() {
            let key = stem.to_lowercase();
            if by_name.contains_key(&key) {
                continue;
            }
            by_name.insert(
                key,
                AppInfo {
                    id: path.to_string_lossy().to_string(),
                    display_name: stem,
                    pid: None,
                    running: false,
                    source: AppSource::StartMenu,
                    windows: Vec::new(),
                },
            );
        }
    }

    Ok(by_name.into_values().collect())
}

/// Start an app and wait for its window.
///
/// `app` may be an id from [`list_apps`], an explicit `.exe` or `.lnk` path, or
/// a display name. An app that is already running is returned as-is rather than
/// started a second time: two windows of the same CAD package is a mess an
/// agent should not create on purpose.
pub fn launch_app(app: &str, timeout_ms: u64) -> Result<LaunchOutcome> {
    let needle = app.trim();
    if needle.is_empty() {
        return Err(Error::InvalidAction("`app` is empty".into()));
    }

    let apps = list_apps(true)?;
    let resolved = resolve_app(&apps, needle)?;

    if resolved.running {
        return Ok(LaunchOutcome {
            id: resolved.id.clone(),
            display_name: resolved.display_name.clone(),
            already_running: true,
            pid: resolved.pid,
            matched_by: "already_running".to_string(),
            windows: resolved.windows.clone(),
            ready: !resolved.windows.is_empty(),
        });
    }

    #[cfg(not(windows))]
    {
        let _ = timeout_ms;
        Err(Error::InvalidAction(
            "launch_app is only implemented on Windows".into(),
        ))
    }

    #[cfg(windows)]
    {
        // A launch is recognised by what appears, not by what we asked for.
        // New processes are unambiguous; a new *window* of an already-running
        // process (Explorer, a browser) has to be matched by name, and only a
        // single unexplained substantial window is accepted as a last resort.
        let before: Vec<WindowInfo> = list_windows(None, true)?;
        let before_ids: std::collections::HashSet<u32> = before.iter().map(|w| w.id).collect();
        let before_pids: std::collections::HashSet<u32> = before.iter().map(|w| w.pid).collect();
        let stem = file_stem(&resolved.id).to_lowercase();

        imp::shell_execute(&resolved.id)?;

        let started = std::time::Instant::now();
        let deadline = std::time::Duration::from_millis(timeout_ms);
        loop {
            let fresh: Vec<WindowInfo> = list_windows(None, true)
                .unwrap_or_default()
                .into_iter()
                .filter(|w| !before_ids.contains(&w.id))
                .collect();

            let (mine, matched_by) = {
                let by_pid: Vec<WindowInfo> = fresh
                    .iter()
                    .filter(|w| !before_pids.contains(&w.pid))
                    .cloned()
                    .collect();
                if !by_pid.is_empty() {
                    (by_pid, "new_process")
                } else {
                    let by_name: Vec<WindowInfo> = fresh
                        .iter()
                        .filter(|w| {
                            w.app.to_lowercase() == stem || w.title.to_lowercase().contains(&stem)
                        })
                        .cloned()
                        .collect();
                    if !by_name.is_empty() {
                        (by_name, "matching_window")
                    } else {
                        let substantial: Vec<WindowInfo> = fresh
                            .iter()
                            .filter(|w| !w.title.is_empty() && w.width >= 200 && w.height >= 200)
                            .cloned()
                            .collect();
                        // Only accept the fallback when it cannot be an accident.
                        if substantial.len() == 1 {
                            (substantial, "single_new_window")
                        } else {
                            (Vec::new(), "none")
                        }
                    }
                }
            };

            if !mine.is_empty() {
                return Ok(LaunchOutcome {
                    id: resolved.id.clone(),
                    display_name: resolved.display_name.clone(),
                    already_running: false,
                    pid: Some(mine[0].pid),
                    matched_by: matched_by.to_string(),
                    windows: mine.iter().map(app_window).collect(),
                    ready: true,
                });
            }
            if started.elapsed() > deadline {
                return Ok(LaunchOutcome {
                    id: resolved.id.clone(),
                    display_name: resolved.display_name.clone(),
                    already_running: false,
                    pid: resolved.pid,
                    matched_by: "none".to_string(),
                    windows: Vec::new(),
                    ready: false,
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
}

/// Exact name match first, then an `.exe`/`.lnk` path on disk, then a unique
/// case-insensitive substring. Ambiguity is an error carrying the candidates:
/// launching the wrong program is much worse than asking.
fn resolve_app(apps: &[AppInfo], needle: &str) -> Result<AppInfo> {
    let lower = needle.to_lowercase();
    if let Some(exact) = apps.iter().find(|a| a.display_name.to_lowercase() == lower) {
        return Ok(exact.clone());
    }
    if let Some(exact) = apps.iter().find(|a| a.id.to_lowercase() == lower) {
        return Ok(exact.clone());
    }
    let path = Path::new(needle);
    if path.is_absolute() && path.exists() {
        let display = file_stem(needle);
        return Ok(AppInfo {
            id: needle.to_string(),
            display_name: display,
            pid: None,
            running: false,
            source: AppSource::StartMenu,
            windows: Vec::new(),
        });
    }
    let matches: Vec<&AppInfo> = apps
        .iter()
        .filter(|a| {
            a.display_name.to_lowercase().contains(&lower) || a.id.to_lowercase().contains(&lower)
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => {
            let names: Vec<String> = apps
                .iter()
                .take(12)
                .map(|a| a.display_name.clone())
                .collect();
            Err(Error::InvalidAction(format!(
                "no app matches `{needle}`; known apps: {}",
                names.join(", ")
            )))
        }
        _ => {
            let names: Vec<String> = matches.iter().map(|a| a.display_name.clone()).collect();
            Err(Error::InvalidAction(format!(
                "`{needle}` matches {} apps ({}); pass an exact name or a full path",
                names.len(),
                names.join(", ")
            )))
        }
    }
}

fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Start Menu shortcuts, keyed by display name. Folders that only contain
/// uninstallers are skipped: launching one of those is never what a task means.
fn start_menu_entries() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for root in start_menu_roots() {
        scan_dir(&root, 0, &mut out);
    }
    out.sort_by_key(|entry| entry.0.to_lowercase());
    out.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
    out
}

fn start_menu_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        roots.push(Path::new(&appdata).join("Microsoft/Windows/Start Menu/Programs"));
    }
    if let Ok(program_data) = std::env::var("ProgramData") {
        roots.push(Path::new(&program_data).join("Microsoft/Windows/Start Menu/Programs"));
    }
    roots
}

const MAX_SCAN_DEPTH: usize = 4;
const MAX_ENTRIES: usize = 512;

fn scan_dir(dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    if depth > MAX_SCAN_DEPTH || out.len() >= MAX_ENTRIES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if name.to_lowercase().contains("uninstall") {
                continue;
            }
            scan_dir(&path, depth + 1, out);
            continue;
        }
        if !name.to_lowercase().ends_with(".lnk") {
            continue;
        }
        out.push((file_stem(&name), path));
    }
}

#[cfg(windows)]
mod imp {
    use cua_core::{Error, Result};

    /// Full image path of a process, used as the app id and to derive a display
    /// name. Limited-information access is enough and works for elevated
    /// processes we do not own.
    pub fn process_path(pid: u32) -> Option<String> {
        use windows::core::PWSTR;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };

        unsafe {
            let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = [0u16; 1024];
            let mut length = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_FORMAT(0),
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
            .is_ok();
            let _ = CloseHandle(handle);
            if ok && length > 0 {
                Some(String::from_utf16_lossy(&buffer[..length as usize]))
            } else {
                None
            }
        }
    }

    /// Launch a file the way Explorer does, so `.lnk` shortcuts, `.exe` images
    /// and file associations all behave as the user would expect.
    pub fn shell_execute(path: &str) -> Result<()> {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let file: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let operation: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        // ShellExecuteW returns a fake HINSTANCE; anything above 32 means it
        // worked, and the low values are documented error codes.
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(operation.as_ptr()),
                PCWSTR(file.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if (result.0 as isize) <= 32 {
            return Err(Error::Capture(format!(
                "ShellExecuteW refused to launch `{path}` (code {})",
                result.0 as isize
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, id: &str, running: bool) -> AppInfo {
        AppInfo {
            id: id.to_string(),
            display_name: name.to_string(),
            pid: running.then_some(4242),
            running,
            source: if running {
                AppSource::Running
            } else {
                AppSource::StartMenu
            },
            windows: Vec::new(),
        }
    }

    #[test]
    fn resolves_exact_name_before_substring() {
        let apps = vec![
            app("Notepad", "C:/Windows/notepad.exe", true),
            app("Notepad++", "C:/Program Files/npp.exe", false),
        ];
        let found = resolve_app(&apps, "notepad").expect("resolved");
        assert_eq!(found.display_name, "Notepad");
    }

    #[test]
    fn resolves_by_id_path() {
        let apps = vec![app("Notepad", "C:/Windows/notepad.exe", true)];
        let found = resolve_app(&apps, "c:/windows/notepad.exe").expect("resolved");
        assert!(found.running);
    }

    #[test]
    fn ambiguous_name_is_refused_with_candidates() {
        let apps = vec![
            app("FreeCAD 1.0", "C:/a/freecad.exe", false),
            app("FreeCAD Weekly", "C:/b/freecad.exe", false),
        ];
        let err = resolve_app(&apps, "freecad").expect_err("must be ambiguous");
        let message = err.to_string();
        assert!(message.contains("FreeCAD 1.0"), "{message}");
        assert!(message.contains("FreeCAD Weekly"), "{message}");
    }

    #[test]
    fn unknown_name_lists_what_exists() {
        let apps = vec![app("Notepad", "C:/Windows/notepad.exe", true)];
        let err = resolve_app(&apps, "definitely-not-installed").expect_err("must fail");
        assert!(err.to_string().contains("Notepad"));
    }

    #[test]
    fn file_stem_strips_extension_and_path() {
        assert_eq!(file_stem("C:/Windows/notepad.exe"), "notepad");
        assert_eq!(file_stem("FreeCAD 1.0.lnk"), "FreeCAD 1.0");
        assert_eq!(file_stem("plain"), "plain");
    }
}
