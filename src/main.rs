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
use windows::Win32::Graphics::Gdi::{
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, GetDC,
    GetDIBits, GetObjectW, HBRUSH, ReleaseDC,
};
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
    overlay_ico: Option<String>,
    #[serde(default = "default_overlay_scale")]
    overlay_scale: f32,
    #[serde(default = "default_overlay_base_scale")]
    overlay_base_scale: f32,
    #[serde(default)]
    overlay_margin: u32,
    #[serde(default)]
    rules: Vec<RawRule>,
}

fn default_poll_ms() -> u32 {
    2000
}

fn default_overlay_scale() -> f32 {
    0.5
}

fn default_overlay_base_scale() -> f32 {
    1.0
}

#[derive(Deserialize)]
struct RawRule {
    exe: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    title_regex: Option<String>,
    ico: String,
    #[serde(default)]
    overlay_ico: Option<String>,
    #[serde(default = "default_true")]
    overlay: bool,
}

fn default_true() -> bool {
    true
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
    badge_big: Option<HICON>,
    badge_small: Option<HICON>,
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

fn resolve_ico(p: &str, base: &Option<PathBuf>) -> String {
    let path = Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        base.as_ref()
            .map(|d| d.join(path).to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string())
    }
}

fn badge_size(icon_size: i32, scale: f32) -> i32 {
    (icon_size as f32 * scale).round().max(8.0) as i32
}

/// Build a top-down BITMAPINFO for a w x h bitmap with `bpp` bits per pixel.
fn top_down_bmi(w: i32, h: i32, bpp: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: bpp as u16,
            biCompression: 0, // BI_RGB
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        ..Default::default()
    }
}

/// Read an icon's color bitmap into `out` as top-down 32bpp BGRA pixels.
/// Returns the bitmap's (width, height), or None if the icon has no 32bpp
/// color bitmap. Reading the bitmap directly is pixel-exact, unlike the
/// GDI icon-draw calls which are unreliable on this machine.
unsafe fn read_icon_bgra(icon: HICON, out: &mut Vec<u8>) -> Option<(i32, i32)> {
    let mut ii = ICONINFO::default();
    let read = match GetIconInfo(icon, &mut ii) {
        Ok(()) if !ii.hbmColor.0.is_null() => {
            let mut bm = BITMAP::default();
            let got = GetObjectW(
                ii.hbmColor.into(),
                mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut BITMAP as *mut core::ffi::c_void),
            );
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            if got == 0 || w <= 0 || h <= 0 || bm.bmBitsPixel != 32 {
                None
            } else {
                out.resize((w * h * 4) as usize, 0);
                let mut bmi = top_down_bmi(w, h, 32);
                let dc = GetDC(None);
                let rows = GetDIBits(
                    dc,
                    ii.hbmColor,
                    0,
                    h as u32,
                    Some(out.as_mut_ptr() as *mut core::ffi::c_void),
                    &mut bmi,
                    DIB_RGB_COLORS,
                );
                ReleaseDC(None, dc);
                (rows == h).then_some((w, h))
            }
        }
        _ => None,
    };
    // GetIconInfo returns new bitmaps the caller must delete
    let _ = DeleteObject(ii.hbmColor.into());
    let _ = DeleteObject(ii.hbmMask.into());
    read
}

/// Nearest-neighbor resize of a top-down BGRA buffer to dst_sz x dst_sz.
fn nearest_resize(src: &[u8], sw: i32, sh: i32, dst_sz: i32) -> Vec<u8> {
    let mut out = vec![0u8; (dst_sz * dst_sz * 4) as usize];
    for dy in 0..dst_sz {
        for dx in 0..dst_sz {
            let sx = (dx as u32 * sw as u32 / dst_sz as u32).min((sw - 1) as u32) as i32;
            let sy = (dy as u32 * sh as u32 / dst_sz as u32).min((sh - 1) as u32) as i32;
            let si = ((sy * sw + sx) * 4) as usize;
            let di = ((dy * dst_sz + dx) * 4) as usize;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    out
}

/// Area-averaging (box) downscale of a top-down BGRA buffer to dst_sz x
/// dst_sz. Averaging is done in premultiplied alpha so semi-transparent
/// edges stay clean. Only used for shrinking (dst_sz < source size).
fn box_downscale(src: &[u8], sw: i32, sh: i32, dst_sz: i32) -> Vec<u8> {
    let mut out = vec![0u8; (dst_sz * dst_sz * 4) as usize];
    let fx = sw as f64 / dst_sz as f64;
    let fy = sh as f64 / dst_sz as f64;
    for dy in 0..dst_sz {
        let y0 = (dy as f64 * fy).floor() as i32;
        let y1 = (((dy + 1) as f64) * fy).ceil().min(sh as f64).max(y0 as f64 + 1.0) as i32;
        for dx in 0..dst_sz {
            let x0 = (dx as f64 * fx).floor() as i32;
            let x1 = (((dx + 1) as f64) * fx).ceil().min(sw as f64).max(x0 as f64 + 1.0) as i32;
            let (mut pb, mut pg, mut pr, mut pa) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let si = ((sy * sw + sx) * 4) as usize;
                    let a = src[si + 3] as f64 / 255.0;
                    pb += src[si] as f64 * a;
                    pg += src[si + 1] as f64 * a;
                    pr += src[si + 2] as f64 * a;
                    pa += a;
                }
            }
            let n = (x1 - x0) as f64 * (y1 - y0) as f64;
            let di = ((dy * dst_sz + dx) * 4) as usize;
            let a = pa / n;
            out[di + 3] = (a * 255.0).round().min(255.0) as u8;
            if a > 0.0 {
                // pb/n is the mean premultiplied color, already in 0..255 units;
                // dividing by the mean alpha unpremultiplies it (stays <= 255)
                out[di] = (pb / n / a).round().min(255.0) as u8;
                out[di + 1] = (pg / n / a).round().min(255.0) as u8;
                out[di + 2] = (pr / n / a).round().min(255.0) as u8;
            }
        }
    }
    out
}

/// Straight-alpha "over" composite of `src` (sw x sh, top-down BGRA) onto
/// `dst` (dw x dh) at offset (ox, oy). Out-of-range pixels are clipped.
fn over_blend(dst: &mut [u8], dw: i32, dh: i32, src: &[u8], sw: i32, sh: i32, ox: i32, oy: i32) {
    for sy in 0..sh {
        let dy = oy + sy;
        if dy < 0 || dy >= dh {
            continue;
        }
        for sx in 0..sw {
            let dx = ox + sx;
            if dx < 0 || dx >= dw {
                continue;
            }
            let si = ((sy * sw + sx) * 4) as usize;
            let sa = src[si + 3] as u32;
            if sa == 0 {
                continue;
            }
            let di = ((dy * dw + dx) * 4) as usize;
            let sb = src[si] as u32;
            let sg = src[si + 1] as u32;
            let sr = src[si + 2] as u32;
            let db = dst[di] as u32;
            let dg = dst[di + 1] as u32;
            let dr = dst[di + 2] as u32;
            let da = dst[di + 3] as u32;
            let out_a = sa + da * (255 - sa) / 255;
            if out_a == 0 {
                dst[di..di + 4].fill(0);
                continue;
            }
            dst[di] = ((sb * sa + db * da * (255 - sa) / 255) / out_a) as u8;
            dst[di + 1] = ((sg * sa + dg * da * (255 - sa) / 255) / out_a) as u8;
            dst[di + 2] = ((sr * sa + dr * da * (255 - sa) / 255) / out_a) as u8;
            dst[di + 3] = out_a as u8;
        }
    }
}

/// Compose `badge` onto the bottom-right corner of `base` and return a new
/// HICON. The base icon is placed in the top-left at `size * base_scale`
/// (like xpra's ICON_SHRINKAGE=75) and the badge at `badge_sz` in the
/// bottom-right on top (like xpra's ICON_OVERLAY=50). Both icons are
/// already loaded at their final sizes (base at `size`, badge at
/// `badge_sz`); their pixels are read with GetDIBits and alpha-composited
/// in software, because GDI icon-draw calls
/// (DrawIcon/DrawIconEx/CreateIconFromResourceEx) are unreliable on this
/// machine. Returns None on failure.
unsafe fn compose_icon(base: HICON, badge: HICON, size: i32, base_scale: f32, badge_sz: i32, margin: i32) -> Option<HICON> {
    let mut base_px: Vec<u8> = Vec::new();
    let (bw, bh) = match read_icon_bgra(base, &mut base_px) {
        Some(d) => d,
        None => {
            log_line(&format!("compose({size}): base icon has no 32bpp color bitmap"));
            return None;
        }
    };
    if bw != size || bh != size {
        log_line(&format!(
            "compose({size}): base icon is {bw}x{bh}, expected {size}x{size}"
        ));
        return None;
    }
    let mut badge_px: Vec<u8> = Vec::new();
    let (vw, vh) = match read_icon_bgra(badge, &mut badge_px) {
        Some(d) => d,
        None => {
            log_line(&format!("compose({size}): badge icon has no 32bpp color bitmap"));
            return None;
        }
    };
    if vw != badge_sz || vh != badge_sz {
        log_line(&format!(
            "compose({size}): badge icon is {vw}x{vh}, resampling to {badge_sz}x{badge_sz}"
        ));
        badge_px = nearest_resize(&badge_px, vw, vh, badge_sz);
    }

    // transparent canvas: base in the top-left at base_scale, then the
    // badge in the bottom-right on top (mirrors xpra's window_icon_image)
    let mut canvas = vec![0u8; (size * size * 4) as usize];
    let base_sz = (size as f32 * base_scale).floor().max(1.0) as i32;
    if base_sz < size {
        let shrunk = box_downscale(&base_px, size, size, base_sz);
        over_blend(&mut canvas, size, size, &shrunk, base_sz, base_sz, 0, 0);
    } else {
        over_blend(&mut canvas, size, size, &base_px, size, size, 0, 0);
    }
    let off = (size - badge_sz - margin).max(0);
    over_blend(&mut canvas, size, size, &badge_px, badge_sz, badge_sz, off, off);

    // 32bpp top-down DIB holding the composed pixels
    let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
    let dib =
        match CreateDIBSection(None, &top_down_bmi(size, size, 32), DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(d) if !d.0.is_null() && !bits.is_null() => d,
            _ => {
                log_line(&format!(
                    "compose({size}): CreateDIBSection failed (gle={})",
                    GetLastError().0
                ));
                return None;
            }
        };
    std::ptr::copy_nonoverlapping(canvas.as_ptr(), bits as *mut u8, canvas.len());

    // 1bpp top-down AND mask, all zeros (alpha comes from the 32bpp DIB)
    let mut mbits: *mut core::ffi::c_void = core::ptr::null_mut();
    let mask =
        match CreateDIBSection(None, &top_down_bmi(size, size, 1), DIB_RGB_COLORS, &mut mbits, None, 0) {
            Ok(d) if !d.0.is_null() && !mbits.is_null() => d,
            _ => {
                let _ = DeleteObject(dib.into());
                log_line(&format!(
                    "compose({size}): CreateDIBSection(mask) failed (gle={})",
                    GetLastError().0
                ));
                return None;
            }
        };
    // zero the whole allocation (stride is DWORD-aligned, not necessarily
    // (size+7)/8 bytes per row)
    let mut mbm = BITMAP::default();
    let _ = GetObjectW(
        mask.into(),
        mem::size_of::<BITMAP>() as i32,
        Some(&mut mbm as *mut BITMAP as *mut core::ffi::c_void),
    );
    let mask_bytes = (mbm.bmWidthBytes as usize) * (mbm.bmHeight as usize);
    std::slice::from_raw_parts_mut(mbits as *mut u8, mask_bytes).fill(0);

    let mut ii = ICONINFO::default();
    ii.fIcon = TRUE;
    ii.hbmMask = mask;
    ii.hbmColor = dib;
    let icon = match CreateIconIndirect(&ii as *const ICONINFO) {
        Ok(i) if !i.0.is_null() => i,
        _ => {
            let _ = DeleteObject(mask.into());
            let _ = DeleteObject(dib.into());
            log_line(&format!(
                "compose({size}): CreateIconIndirect failed (gle={})",
                GetLastError().0
            ));
            return None;
        }
    };
    // GDI copied the bitmaps into the new icon; release both DIBs
    let _ = DeleteObject(mask.into());
    let _ = DeleteObject(dib.into());
    Some(icon)
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
    let scale = cfg.overlay_scale.clamp(0.1, 1.0);
    let base_scale = cfg.overlay_base_scale.clamp(0.1, 1.0);
    let margin = cfg.overlay_margin as i32;
    let big_badge_sz = badge_size(big_sz, scale);
    let small_badge_sz = badge_size(small_sz, scale);

    // global badge: drawn at the bottom-right of every rule's icon
    // (like xpra does for icons it receives from the remote side)
    let global_badge = match &cfg.overlay_ico {
        Some(p) => match unsafe { load_icon_pair(&resolve_ico(p, &base), big_badge_sz, small_badge_sz) } {
            Ok(b) => Some(b),
            Err(e) => {
                log_line(&format!("overlay_ico: {e}"));
                None
            }
        },
        None => None,
    };

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
        // badge for this rule: per-rule overlay_ico override > global; overlay = false disables it
        let badge = if r.overlay {
            match &r.overlay_ico {
                Some(p) => match unsafe {
                    load_icon_pair(&resolve_ico(p, &base), big_badge_sz, small_badge_sz)
                } {
                    Ok(b) => Some(b),
                    Err(e) => {
                        log_line(&format!("rule[{i}]: overlay_ico: {e}"));
                        None
                    }
                },
                None => global_badge.clone(),
            }
        } else {
            None
        };
        let ico_path = resolve_ico(&r.ico, &base);
        let (big, small) = match unsafe { load_icon_pair(&ico_path, big_sz, small_sz) } {
            Ok((b, s)) => match &badge {
                Some((bb, bs)) => match (
                    unsafe { compose_icon(b, *bb, big_sz, base_scale, big_badge_sz, margin) },
                    unsafe { compose_icon(s, *bs, small_sz, base_scale, small_badge_sz, margin) },
                ) {
                    (Some(x), Some(y)) => (x, y),
                    _ => {
                        log_line(&format!("rule[{i}]: icon compose failed, using icon without badge"));
                        (b, s)
                    }
                },
                None => (b, s),
            },
            Err(e) => {
                log_line(&format!("rule[{i}]: {e}"));
                continue;
            }
        };
        rules.push(RtRule {
            exe: r.exe.to_ascii_lowercase(),
            title: r.title.clone(),
            re,
            big,
            small,
            ico: r.ico.clone(),
        });
    }
    let mut s = state();
    let old: Vec<(HICON, HICON)> = s.rules.iter().map(|r| (r.big, r.small)).collect();
    for (b, sm) in old {
        s.retired.push(b);
        s.retired.push(sm);
    }
    if let (Some(b), Some(sm)) = (s.badge_big, s.badge_small) {
        s.retired.push(b);
        s.retired.push(sm);
    }
    s.badge_big = global_badge.as_ref().map(|(b, _)| *b);
    s.badge_small = global_badge.as_ref().map(|(_, sm)| *sm);
    s.rules = rules;
    s.poll_ms = cfg.poll_interval_ms.max(500);
    log_line(&format!(
        "config loaded from {}: {} rule(s), poll every {} ms, overlay {}",
        path.display(),
        s.rules.len(),
        s.poll_ms,
        if s.badge_big.is_some() { "on" } else { "off" }
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
