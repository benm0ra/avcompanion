//! Zoom remote control.
//!
//! Zoom has no interface for controlling the installed client, so this module
//! does what a person does: bring the meeting window forward, press Zoom's
//! own keyboard shortcuts, and (for one-touch sharing) operate Zoom's share
//! window through Windows UI Automation.
//!
//! Everything here is best-effort and fails safe. One-touch sharing never
//! guesses: if it cannot find exactly one matching window it stops with
//! Zoom's share window open so the user can finish by hand.
//!
//! The text Zoom shows differs between versions and languages. The strings
//! this module looks for can be overridden in `zoom.json` next to
//! `daemon.json`, and `zoom.diagnostics` writes what Zoom actually exposes.

use serde_json::Value;

/// Names this module looks for in Zoom. Override any of them in `zoom.json`.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, serde::Deserialize)]
#[serde(default)]
pub struct ZoomStrings {
    /// Executable name of the Zoom client.
    pub process: String,
    /// The share window is the Zoom window containing a control whose name contains one of these.
    pub sound_checkbox: Vec<String>,
    /// Exact name (case-insensitive) of the button that starts the share.
    pub share_button: Vec<String>,
}

impl Default for ZoomStrings {
    fn default() -> Self {
        Self {
            process: "Zoom.exe".into(),
            sound_checkbox: vec!["Share sound".into(), "Share computer sound".into()],
            share_button: vec!["Share".into()],
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
impl ZoomStrings {
    pub fn load() -> Self {
        let path = crate::daemon::discovery_dir().join("zoom.json");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

/// Run one `zoom.*` command. Blocking (up to a few seconds): call it from a
/// blocking thread, never from an async task or an audio callback.
pub fn command(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "zoom.share.open" => imp::shortcut(imp::Shortcut::ShareOpenOrStop),
        "zoom.share.toggle" => imp::shortcut(imp::Shortcut::ShareToggle),
        "zoom.share.pause" => imp::shortcut(imp::Shortcut::SharePause),
        "zoom.share.window" => {
            let wanted = args["match"].as_str().unwrap_or("").trim();
            if wanted.is_empty() {
                return Err("Say which app or window to share (part of its title).".into());
            }
            imp::share_window(wanted, args["sound"].as_bool().unwrap_or(false))
        }
        "zoom.diagnostics" => imp::diagnostics(),
        other => Err(format!("unknown Zoom command \"{other}\"")),
    }
}

#[cfg(windows)]
mod imp {
    use super::ZoomStrings;
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};

    use windows::core::{Interface, Result as WinResult, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{
        AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
        IUIAutomationLegacyIAccessiblePattern, IUIAutomationSelectionItemPattern,
        IUIAutomationTogglePattern, ToggleState_On, TreeScope_Descendants, UIA_ButtonControlTypeId,
        UIA_InvokePatternId, UIA_LegacyIAccessiblePatternId, UIA_SelectionItemPatternId,
        UIA_TogglePatternId, UIA_PATTERN_ID,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowRect,
        GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow,
        ShowWindow, SW_RESTORE,
    };

    const VK_S: VIRTUAL_KEY = VIRTUAL_KEY(0x53);
    const VK_T: VIRTUAL_KEY = VIRTUAL_KEY(0x54);

    /// Zoom's default Windows shortcuts. Users who rebind them in Zoom must rebind back.
    #[derive(Clone, Copy)]
    pub enum Shortcut {
        /// Alt+S: open the share window, or stop the current share.
        ShareOpenOrStop,
        /// Alt+Shift+S: start or stop a new share.
        ShareToggle,
        /// Alt+T: pause or resume the share.
        SharePause,
    }

    struct Win {
        hwnd: HWND,
        title: String,
        class: String,
        area: i64,
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = &mut *(lparam.0 as *mut Vec<HWND>);
        list.push(hwnd);
        BOOL(1)
    }

    fn process_name(pid: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(handle);
            ok.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            path.rsplit(['\\', '/']).next().map(str::to_string)
        }
    }

    /// Visible top-level windows that belong to the Zoom client, largest first.
    fn zoom_windows(strings: &ZoomStrings) -> Vec<Win> {
        let mut all: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize));
        }
        let mut out = Vec::new();
        for hwnd in all {
            unsafe {
                if !IsWindowVisible(hwnd).as_bool() {
                    continue;
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                let is_zoom =
                    process_name(pid).is_some_and(|n| n.eq_ignore_ascii_case(&strings.process));
                if !is_zoom {
                    continue;
                }
                let mut t = [0u16; 512];
                let tl = GetWindowTextW(hwnd, &mut t).max(0) as usize;
                let mut c = [0u16; 256];
                let cl = GetClassNameW(hwnd, &mut c).max(0) as usize;
                let mut r = RECT::default();
                let _ = GetWindowRect(hwnd, &mut r);
                out.push(Win {
                    hwnd,
                    title: String::from_utf16_lossy(&t[..tl]),
                    class: String::from_utf16_lossy(&c[..cl]),
                    area: (r.right - r.left).max(0) as i64 * (r.bottom - r.top).max(0) as i64,
                });
            }
        }
        out.sort_by(|a, b| b.area.cmp(&a.area));
        out
    }

    /// The meeting window: the largest Zoom window whose title mentions a meeting,
    /// otherwise the largest Zoom window.
    fn meeting_window(wins: &[Win]) -> Option<HWND> {
        wins.iter()
            .find(|w| w.title.to_lowercase().contains("meeting"))
            .or_else(|| wins.first())
            .map(|w| w.hwnd)
    }

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Press the keys in order and release them in reverse.
    fn chord(keys: &[VIRTUAL_KEY]) -> Result<(), String> {
        let mut inputs: Vec<INPUT> = keys.iter().map(|k| key(*k, false)).collect();
        inputs.extend(keys.iter().rev().map(|k| key(*k, true)));
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err("Windows blocked the key press. If Zoom is running as administrator, AVcompanion cannot control it.".into())
        }
    }

    /// Bring a window to the foreground. Windows only lets the foreground
    /// thread hand focus over, so attach to it for the duration of the call.
    fn focus(hwnd: HWND) -> Result<(), String> {
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let me = GetCurrentThreadId();
            let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
            let attached = fg_thread != 0
                && fg_thread != me
                && AttachThreadInput(me, fg_thread, true).as_bool();
            let _ = BringWindowToTop(hwnd);
            let ok = SetForegroundWindow(hwnd).as_bool();
            if attached {
                let _ = AttachThreadInput(me, fg_thread, false);
            }
            std::thread::sleep(Duration::from_millis(150));
            if ok || GetForegroundWindow() == hwnd {
                Ok(())
            } else {
                Err("Could not bring Zoom to the front. Click the Zoom meeting window once and try again.".into())
            }
        }
    }

    fn press(shortcut: Shortcut, strings: &ZoomStrings) -> Result<HWND, String> {
        let wins = zoom_windows(strings);
        let meeting =
            meeting_window(&wins).ok_or("Zoom does not seem to be open. Join a meeting first.")?;
        focus(meeting)?;
        // Zoom's share shortcuts only act while its meeting controls have focus;
        // Ctrl+Alt+Shift is Zoom's own "move focus to meeting controls".
        chord(&[VK_CONTROL, VK_MENU, VK_SHIFT])?;
        std::thread::sleep(Duration::from_millis(120));
        match shortcut {
            Shortcut::ShareOpenOrStop => chord(&[VK_MENU, VK_S])?,
            Shortcut::ShareToggle => chord(&[VK_MENU, VK_SHIFT, VK_S])?,
            Shortcut::SharePause => chord(&[VK_MENU, VK_T])?,
        }
        Ok(meeting)
    }

    pub fn shortcut(shortcut: Shortcut) -> Result<String, String> {
        press(shortcut, &ZoomStrings::load())?;
        Ok(match shortcut {
            Shortcut::ShareOpenOrStop => "Sent Zoom's share shortcut (Alt+S).",
            Shortcut::ShareToggle => "Sent Zoom's start/stop share shortcut (Alt+Shift+S).",
            Shortcut::SharePause => "Sent Zoom's pause/resume share shortcut (Alt+T).",
        }
        .into())
    }

    // ------------------------------------------------------ UI Automation

    fn automation() -> Result<IUIAutomation, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("Windows UI Automation is not available: {e}"))
        }
    }

    struct Node {
        el: IUIAutomationElement,
        name: String,
        control: i32,
    }

    fn descendants(uia: &IUIAutomation, hwnd: HWND) -> WinResult<Vec<Node>> {
        unsafe {
            let root = uia.ElementFromHandle(hwnd)?;
            let all = root.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
            let mut out = Vec::new();
            for i in 0..all.Length()? {
                let el = all.GetElement(i)?;
                let name = el.CurrentName().map(|b| b.to_string()).unwrap_or_default();
                let control = el.CurrentControlType().map(|c| c.0).unwrap_or(0);
                out.push(Node { el, name, control });
            }
            Ok(out)
        }
    }

    fn pattern<T: Interface>(el: &IUIAutomationElement, id: UIA_PATTERN_ID) -> Option<T> {
        unsafe { el.GetCurrentPatternAs::<T>(id).ok() }
    }

    fn contains_any(name: &str, needles: &[String]) -> bool {
        let n = name.to_lowercase();
        needles
            .iter()
            .any(|x| !x.is_empty() && n.contains(&x.to_lowercase()))
    }

    /// Select, invoke or default-action an element, whichever it supports.
    fn activate(el: &IUIAutomationElement) -> bool {
        unsafe {
            if let Some(p) =
                pattern::<IUIAutomationSelectionItemPattern>(el, UIA_SelectionItemPatternId)
            {
                if p.Select().is_ok() {
                    return true;
                }
            }
            if let Some(p) = pattern::<IUIAutomationInvokePattern>(el, UIA_InvokePatternId) {
                if p.Invoke().is_ok() {
                    return true;
                }
            }
            if let Some(p) =
                pattern::<IUIAutomationLegacyIAccessiblePattern>(el, UIA_LegacyIAccessiblePatternId)
            {
                if p.DoDefaultAction().is_ok() {
                    return true;
                }
            }
        }
        false
    }

    /// Wait for Zoom's share window: the Zoom window that contains the share-sound control.
    fn wait_for_share_window(
        uia: &IUIAutomation,
        strings: &ZoomStrings,
    ) -> Option<(HWND, Vec<Node>)> {
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            for w in zoom_windows(strings) {
                if let Ok(nodes) = descendants(uia, w.hwnd) {
                    if nodes
                        .iter()
                        .any(|n| contains_any(&n.name, &strings.sound_checkbox))
                    {
                        return Some((w.hwnd, nodes));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        None
    }

    const LEFT_OPEN: &str = "Zoom's share window is open; finish there by hand.";

    pub fn share_window(wanted: &str, sound: bool) -> Result<String, String> {
        let strings = ZoomStrings::load();
        let uia = automation()?;
        press(Shortcut::ShareOpenOrStop, &strings)?;

        let (_hwnd, nodes) = wait_for_share_window(&uia, &strings).ok_or(
            "Zoom's share window did not appear, or this Zoom version names things differently. If a share was already running, the shortcut stopped it. Use \"Save Zoom diagnostics\" in the app so the names can be adjusted.",
        )?;

        // Candidates: anything named like the wanted window, except the dialog's own controls.
        let needle = wanted.to_lowercase();
        let matches: Vec<&Node> = nodes
            .iter()
            .filter(|n| {
                n.name.to_lowercase().contains(&needle)
                    && !contains_any(&n.name, &strings.sound_checkbox)
                    && !strings
                        .share_button
                        .iter()
                        .any(|b| b.eq_ignore_ascii_case(n.name.trim()))
            })
            .collect();
        // The same tile is often exposed as several nested elements with one name.
        let mut names: Vec<&str> = matches.iter().map(|n| n.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        match names.len() {
            0 => {
                return Err(format!(
                    "Nothing in Zoom's share list matches \"{wanted}\". {LEFT_OPEN}"
                ))
            }
            1 => {}
            n => {
                return Err(format!(
                    "{n} windows match \"{wanted}\" ({}). Use a more specific name. {LEFT_OPEN}",
                    names.join("; ")
                ))
            }
        }
        let chosen = names[0].to_string();

        // Sound first: selecting a tile must not be able to start the share before this is set.
        let mut sound_note = "";
        match nodes
            .iter()
            .find(|n| contains_any(&n.name, &strings.sound_checkbox))
        {
            Some(n) => match pattern::<IUIAutomationTogglePattern>(&n.el, UIA_TogglePatternId) {
                Some(t) => unsafe {
                    let on = t
                        .CurrentToggleState()
                        .map(|s| s == ToggleState_On)
                        .unwrap_or(false);
                    if on != sound && t.Toggle().is_err() {
                        return Err(format!("Could not change \"{}\". {LEFT_OPEN}", n.name));
                    }
                },
                None if sound => {
                    return Err(format!(
                        "Zoom's \"{}\" box cannot be ticked from outside. {LEFT_OPEN}",
                        n.name
                    ))
                }
                None => sound_note = " (sound setting left as it was)",
            },
            None if sound => {
                return Err(format!(
                    "Could not find Zoom's share-sound box. {LEFT_OPEN}"
                ))
            }
            None => {}
        }

        if !matches.iter().any(|n| activate(&n.el)) {
            return Err(format!(
                "Found \"{chosen}\" but could not select it. {LEFT_OPEN}"
            ));
        }
        std::thread::sleep(Duration::from_millis(200));

        let button = nodes.iter().find(|n| {
            n.control == UIA_ButtonControlTypeId.0
                && strings
                    .share_button
                    .iter()
                    .any(|b| b.eq_ignore_ascii_case(n.name.trim()))
        });
        let started = button.is_some_and(|b| activate(&b.el));
        if !started {
            return Err(format!(
                "Selected \"{chosen}\" but could not press Share. {LEFT_OPEN}"
            ));
        }
        Ok(format!(
            "Sharing \"{chosen}\"{}{sound_note}.",
            if sound { " with sound" } else { "" }
        ))
    }

    /// Write what Zoom exposes to a text file, so the names this module looks
    /// for can be corrected for the installed Zoom version and language.
    pub fn diagnostics() -> Result<String, String> {
        let strings = ZoomStrings::load();
        let uia = automation()?;
        let wins = zoom_windows(&strings);
        let mut out = String::new();
        let _ = writeln!(out, "AVcompanion Zoom diagnostics");
        let _ = writeln!(out, "Zoom windows found: {}", wins.len());
        for w in &wins {
            let _ = writeln!(
                out,
                "\n== window class=\"{}\" title=\"{}\" area={}",
                w.class, w.title, w.area
            );
            match descendants(&uia, w.hwnd) {
                Ok(nodes) => {
                    for n in nodes.iter().take(600) {
                        if n.name.is_empty() {
                            continue;
                        }
                        let mut caps = String::new();
                        if pattern::<IUIAutomationInvokePattern>(&n.el, UIA_InvokePatternId)
                            .is_some()
                        {
                            caps.push_str(" invoke");
                        }
                        if pattern::<IUIAutomationTogglePattern>(&n.el, UIA_TogglePatternId)
                            .is_some()
                        {
                            caps.push_str(" toggle");
                        }
                        if pattern::<IUIAutomationSelectionItemPattern>(
                            &n.el,
                            UIA_SelectionItemPatternId,
                        )
                        .is_some()
                        {
                            caps.push_str(" select");
                        }
                        let _ = writeln!(out, "  type={} name=\"{}\"{}", n.control, n.name, caps);
                    }
                }
                Err(e) => {
                    let _ = writeln!(out, "  (could not read: {e})");
                }
            }
        }
        let dir = crate::daemon::discovery_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("zoom-diagnostics.txt");
        std::fs::write(&path, out).map_err(|e| e.to_string())?;
        Ok(format!("Saved {}", path.display()))
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
mod imp {
    const MSG: &str = "Zoom remote control is only available on Windows in this build.";

    #[derive(Clone, Copy)]
    pub enum Shortcut {
        ShareOpenOrStop,
        ShareToggle,
        SharePause,
    }
    pub fn shortcut(_s: Shortcut) -> Result<String, String> {
        Err(MSG.into())
    }
    pub fn share_window(_wanted: &str, _sound: bool) -> Result<String, String> {
        Err(MSG.into())
    }
    pub fn diagnostics() -> Result<String, String> {
        Err(MSG.into())
    }
}
