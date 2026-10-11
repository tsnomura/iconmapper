#![windows_subsystem = "windows"]

use std::collections::HashMap;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde::Deserialize;
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const TIMER_ID: usize = 1;
const TRAY_MSG: u32 = WM_APP + 1;
const CMD_RELOAD: u32 = 1;
const CMD_EXIT: u32 = 2;
const TIMEOUT_MS: u32 = 200;

#[derive(Deserialize)]
struct Config {
    #[serde(default = "default_poll_ms")]
    poll_interval_ms: u32,
    #[serde(default)]
    rules: Vec<RawRule>,
}

fn default_poll_ms() -> u32 {
    2000
}

#[derive(Deserialize)]
struct RawRule {
    exe: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    title_regex: Option<String>,
    ico: String,
}

struct RtRule {
    exe: String,
    title: Option<String>,
    re: Option<Regex>,
    big: HICON,
    small: HICON,
    ico: String,
}

#[derive(Default)]
struct AppState {
    rules: Vec<RtRule>,
    retired: Vec<HICON>,
    hooks: Vec<HWINEVENTHOOK>,
    hwnd: HWND,
    tray_added: bool,
    tray_icon: HICON,
    pid_cache: HashMap<u32, String>,
    // hwnd -> (last observed small, last observed big, consecutive stable-mismatch count)
    icon_track: HashMap<usize, (usize, usize, u32)>,
    poll_ms: u32,
}

// single-threaded app; raw pointer fields (HICON/HWND) are only used on the main thread
unsafe impl Send for AppState {}

static STATE: LazyLock<Mutex<AppState>> = LazyLock::new(|| Mutex::new(AppState::default()));

fn state() -> std::sync::MutexGuard<'static, AppState> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("iconmapper: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    log_line("iconmapper starting");
    let hmodule =
        unsafe { GetModuleHandleW(PCWSTR(std::ptr::null())) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let hinstance = HINSTANCE(hmodule.0);

    let class_name = w!("IconMapperWnd");
    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: HICON(std::ptr::null_mut()),
        hCursor: HCURSOR(std::ptr::null_mut()),
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR(std::ptr::null()),
        lpszClassName: class_name,
    };
    if unsafe { RegisterClassW(&wc) } == 0 {
        return Err(format!("RegisterClassW failed: {}", unsafe { GetLastError().0 }));
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            &HSTRING::from("iconmapper"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW: {e}"))?;
    state().hwnd = hwnd;

    reload_config();
    unsafe { add_tray(hwnd) };
    setup_hooks();
    scan_all();

    let poll = state().poll_ms;
    unsafe {
        SetTimer(Some(hwnd), TIMER_ID, poll, None);
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        TRAY_MSG => {
            let event = lparam.0 as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP || event == WM_LBUTTONDBLCLK || event == WM_CONTEXTMENU
            {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 as usize == TIMER_ID {
                scan_all();
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd = (wparam.0 & 0xFFFF) as u32;
            match cmd {
                CMD_RELOAD => {
                    reload_config();
                    rearm_timer();
                }
                CMD_EXIT => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            cleanup();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn cleanup() {
    let mut s = state();
    let hwnd = s.hwnd;
    if s.tray_added {
        let nid = make_nid(hwnd, s.tray_icon);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        s.tray_added = false;
    }
    let _ = KillTimer(Some(hwnd), TIMER_ID);
    for h in s.hooks.drain(..) {
        if !h.0.is_null() {
            let _ = UnhookWinEvent(h);
        }
    }
    log_line("iconmapper exiting");
}

unsafe fn rearm_timer() {
    let s = state();
    let hwnd = s.hwnd;
    let poll = s.poll_ms;
    let _ = KillTimer(Some(hwnd), TIMER_ID);
    SetTimer(Some(hwnd), TIMER_ID, poll, None);
}

unsafe extern "system" fn event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    idobject: i32,
    _idchild: i32,
    _thread: u32,
    _time: u32,
) {
    if idobject == 0 {
        apply_to(hwnd);
    }
}

fn setup_hooks() {
    unsafe {
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        for ev in [EVENT_OBJECT_SHOW, EVENT_OBJECT_NAMECHANGE] {
            let h = SetWinEventHook(ev, ev, None, Some(event_proc), 0, 0, flags);
            if h.0.is_null() {
                log_line(&format!("SetWinEventHook({ev}) failed: {}", GetLastError().0));
            } else {
                state().hooks.push(h);
            }
        }
    }
}

unsafe extern "system" fn enum_cb(hwnd: HWND, _lparam: LPARAM) -> windows::core::BOOL {
    apply_to(hwnd);
    TRUE
}

fn scan_all() {
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(0));
    }
    let dead: Vec<usize> = state()
        .icon_track
        .keys()
        .copied()
        .filter(|k| !unsafe { IsWindow(Some(HWND(*k as *mut std::ffi::c_void))).as_bool() })
        .collect();
    if !dead.is_empty() {
        let mut s = state();
        for k in dead {
            s.icon_track.remove(&k);
        }
    }
}

unsafe fn apply_to(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    if GetAncestor(hwnd, GA_ROOT) != hwnd {
        return;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == GetCurrentProcessId() {
        return;
    }
    let Some(exe) = get_exe_name(pid) else {
        return;
    };
    let title = get_title(hwnd);
    let (big, small, ico) = {
        let s = state();
        match s.rules.iter().find(|r| rule_matches(r, &exe, &title)) {
            Some(r) => (r.big, r.small, r.ico.clone()),
            None => return,
        }
    };
    let cur_small =
        unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                TIMEOUT_MS,
                None,
            )
        }
        .0 as usize;
    let cur_big =
        unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                TIMEOUT_MS,
                None,
            )
        }
        .0 as usize;
    let observed = (cur_small, cur_big);
    let want = (small.0 as usize, big.0 as usize);
    let key = hwnd.0 as usize;
    let mut changed = false;
    if observed != want {
        let mut s = state();
        let t = s.icon_track.entry(key).or_insert((0, 0, 0));
        let stable = t.0 == cur_small && t.1 == cur_big;
        if stable {
            t.2 += 1;
        }
        t.0 = cur_small;
        t.1 = cur_big;
        // some apps (e.g. Win11 notepad) never report the icon we set;
        // stop hammering them, retry only every 10th poll
        if !stable || t.2 % 10 == 0 {
            if cur_small != want.0 {
                unsafe {
                    SendMessageTimeoutW(
                        hwnd,
                        WM_SETICON,
                        WPARAM(ICON_SMALL as usize),
                        LPARAM(small.0 as isize),
                        SMTO_ABORTIFHUNG,
                        TIMEOUT_MS,
                        None,
                    )
                };
            }
            if cur_big != want.1 {
                unsafe {
                    SendMessageTimeoutW(
                        hwnd,
                        WM_SETICON,
                        WPARAM(ICON_BIG as usize),
                        LPARAM(big.0 as isize),
                        SMTO_ABORTIFHUNG,
                        TIMEOUT_MS,
                        None,
                    )
                };
            }
            changed = true;
        }
    } else {
        state().icon_track.remove(&key);
    }
    if changed {
        log_line(&format!(
            "set icon hwnd={:?} pid={} exe={} title=\"{}\" ico={}",
            hwnd.0 as usize, pid, exe, title, ico
        ));
    }
}

fn rule_matches(r: &RtRule, exe: &str, title: &str) -> bool {
    if r.exe != exe {
        return false;
    }
    if let Some(re) = &r.re {
        return re.is_match(title);
    }
    match &r.title {
        Some(t) => title.to_lowercase().contains(&t.to_lowercase()),
        None => true,
    }
}

unsafe fn get_exe_name(pid: u32) -> Option<String> {
    {
        let s = state();
        if let Some(e) = s.pid_cache.get(&pid) {
            return Some(e.clone());
        }
    }
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = vec![0u16; 2048];
    let mut size = buf.len() as u32;
    QueryFullProcessImageNameW(
        h,
        PROCESS_NAME_WIN32,
        PWSTR::from_raw(buf.as_mut_ptr()),
        &mut size,
    )
    .ok()?;
    let _ = CloseHandle(h);
    let path = String::from_utf16_lossy(&buf[..size as usize]);
    let name = Path::new(&path).file_name()?.to_str()?.to_ascii_lowercase();
    {
        let mut s = state();
        if s.pid_cache.len() > 4096 {
            s.pid_cache.clear();
        }
        s.pid_cache.insert(pid, name.clone());
    }
    Some(name)
}

unsafe fn get_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn load_icon_pair(ico: &str, big: i32, small: i32) -> Result<(HICON, HICON), String> {
    let h = HSTRING::from(ico);
    let b = LoadImageW(None, &h, IMAGE_ICON, big, big, LR_LOADFROMFILE)
        .map(|x| HICON(x.0))
        .map_err(|e| format!("load {ico} ({big}x{big}): {e}"))?;
    let s = LoadImageW(None, &h, IMAGE_ICON, small, small, LR_LOADFROMFILE)
        .map(|x| HICON(x.0))
        .map_err(|e| format!("load {ico} ({small}x{small}): {e}"))?;
    Ok((b, s))
}

fn config_path() -> PathBuf {
    let mut candidates = Vec::new();
    if let Some(d) = exe_dir() {
        candidates.push(d.join("iconmapper.toml"));
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        candidates.push(
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("iconmapper")
                .join("iconmapper.toml"),
        );
    }
    candidates.push(PathBuf::from("iconmapper.toml"));
    for c in &candidates {
        if c.is_file() {
            return c.clone();
        }
    }
    candidates.remove(0)
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn reload_config() {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            log_line(&format!("config load failed ({}): {e}", path.display()));
            return;
        }
    };
    let cfg: Config = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            log_line(&format!("config parse failed ({}): {e}", path.display()));
            return;
        }
    };
    let big_sz = unsafe { GetSystemMetrics(SM_CXICON) };
    let small_sz = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let base = path.parent().map(|d| d.to_path_buf());
    let mut rules = Vec::new();
    for (i, r) in cfg.rules.iter().enumerate() {
        if r.exe.trim().is_empty() || r.ico.trim().is_empty() {
            log_line(&format!("rule[{i}]: missing exe or ico, skipped"));
            continue;
        }
        let re = match &r.title_regex {
            Some(p) => match Regex::new(p) {
                Ok(x) => Some(x),
                Err(e) => {
                    log_line(&format!("rule[{i}]: bad title_regex {p:?}: {e}"));
                    None
                }
            },
            None => None,
        };
        let ico_path = {
            let p = std::path::Path::new(&r.ico);
            if p.is_absolute() {
                r.ico.clone()
            } else {
                base.as_ref()
                    .map(|d| d.join(p).to_string_lossy().into_owned())
                    .unwrap_or_else(|| r.ico.clone())
            }
        };
        match unsafe { load_icon_pair(&ico_path, big_sz, small_sz) } {
            Ok((big, small)) => rules.push(RtRule {
                exe: r.exe.to_ascii_lowercase(),
                title: r.title.clone(),
                re,
                big,
                small,
                ico: r.ico.clone(),
            }),
            Err(e) => log_line(&format!("rule[{i}]: {e}")),
        }
    }
    let mut s = state();
    let old: Vec<(HICON, HICON)> = s.rules.iter().map(|r| (r.big, r.small)).collect();
    for (b, sm) in old {
        s.retired.push(b);
        s.retired.push(sm);
    }
    s.rules = rules;
    s.poll_ms = cfg.poll_interval_ms.max(500);
    log_line(&format!(
        "config loaded from {}: {} rule(s), poll every {} ms",
        path.display(),
        s.rules.len(),
        s.poll_ms
    ));
}

unsafe fn add_tray(hwnd: HWND) {
    let icon = LoadIconW(None, IDI_APPLICATION).unwrap_or_else(|_| HICON(std::ptr::null_mut()));
    state().tray_icon = icon;
    let nid = make_nid(hwnd, icon);
    if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        state().tray_added = true;
        log_line("tray icon added");
    } else {
        log_line(&format!("tray icon add failed: {}", GetLastError().0));
    }
}

unsafe fn make_nid(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = TRAY_MSG;
    nid.hIcon = icon;
    for (i, c) in "iconmapper".encode_utf16().enumerate().take(127) {
        nid.szTip[i] = c;
    }
    nid
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(e) => {
            log_line(&format!("CreatePopupMenu failed: {e}"));
            return;
        }
    };
    let _ = AppendMenuW(menu, MF_STRING, CMD_RELOAD as usize, w!("Reload config (iconmapper.toml)"));
    let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT as usize, w!("Quit iconmapper"));
    let mut pt = POINT { x: 0, y: 0 };
    if GetCursorPos(&mut pt).is_ok() {
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
    }
    let _ = SetForegroundWindow(hwnd);
    let _ = DestroyMenu(menu);
}

fn now_str() -> String {
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

fn log_path() -> PathBuf {
    config_path()
        .parent()
        .map(|d| d.join("iconmapper.log"))
        .unwrap_or_else(|| PathBuf::from("iconmapper.log"))
}

fn log_line(msg: &str) {
    let line = format!("[{}] {msg}\n", now_str());
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) else {
        return;
    };
    use std::io::Write;
    let _ = f.write_all(line.as_bytes());
}
