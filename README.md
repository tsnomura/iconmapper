# iconmapper

A resident utility for Windows that matches windows by **process name + title** and applies a chosen `.ico` to their taskbar (and titlebar) icon.

Built to tell windows from an xpra session apart in the taskbar, but it works on any local window that has no AppUserModelID (AUMID).

## How it works

- Detects windows via WinEvents (window shown / title changed) plus a 2 s poll
- Applies the matched icon with `WM_SETICON` (big + small)
- For windows **without** an AUMID (xpra / GTK etc.) the taskbar button icon updates **live**
- Windows that match **no rule are left untouched** (there is no fallback icon)
- If an app resets its icon, it is re-applied (throttled: at most once per 20 s)

### Known limitations

| Target | Taskbar button | Titlebar |
|---|---|---|
| Windows without AUMID (e.g. xpra, GTK apps) | **updated live** | updated |
| Packaged / AUMID apps (e.g. Win11 notepad) | not updated (icon is resolved from the AUMID) | not updated if the titlebar is custom-drawn |
| Elevated (admin) windows | not updated (UIPI blocks the message) | same |

## Building

Requirements: Rust toolchain (MSVC target on Windows).

```
cargo build --release
```

The binary is `target/release/iconmapper.exe` (GUI subsystem — no console window).

## Running

Run `iconmapper.exe` (double-click, or your autostart mechanism of choice).

- Tray icon: right-click → **Reload config (iconmapper.toml)** / **Quit iconmapper**
- Config: `iconmapper.toml` — looked up next to the exe, then in `%USERPROFILE%\.local\share\iconmapper\`, then in the CWD. Start from [`iconmapper.example.toml`](iconmapper.example.toml).
- Log: `iconmapper.log` in the config file's directory (`set icon ...` lines show what was applied)
- Suggested install layout: exe in `%USERPROFILE%\.local\bin\`, config + icons in `%USERPROFILE%\.local\share\iconmapper\`

## Configuration

```toml
poll_interval_ms = 2000   # optional, default 2000

# Badge compositing — see "Overlay" below:
overlay_ico = "icons\\badge.ico"   # badge drawn at the bottom-right of every rule's icon
# overlay_scale = 0.5  # badge size as a fraction of the icon size (0.1–1.0, default 0.5)
# overlay_base_scale = 0.75  # app icon shrunk to this fraction, top-left (0.1–1.0, default 1.0)
# overlay_margin = 0   # badge inset from the bottom-right corner, in px (default 0)

[[rules]]
exe = "someclient.exe"        # process name: exact match, case-insensitive (required)
title = "Spreadsheet"         # title substring, case-insensitive
ico = "icons\\green.ico"      # absolute path, or relative to the config file's dir

[[rules]]
exe = "someclient.exe"
title_regex = "report.*\\.txt"  # regex; takes priority over `title` when both are set
ico = "icons\\blue.ico"
# overlay_ico = "icons\\other-badge.ico"  # per-rule badge override
# overlay = false                         # skip the badge for this rule
```

- Rules are evaluated top to bottom; the **first match wins** → put specific rules before generic ones
- `title` is a substring match (`title = "term"` matches any title containing "term")
- Relative `ico` paths are resolved against the **config file's directory**

### Overlay (badge compositing)

`overlay_ico` composites a badge icon at the **bottom-right corner** of every rule's icon when the config is (re)loaded — the same look xpra gives icons received from the remote side (app icon + small badge in the corner).

- A global `overlay_ico` applies to all rules; a per-rule `overlay_ico` overrides it, and `overlay = false` excludes a rule
- `overlay_scale` (default `0.5`, clamped to 0.1–1.0) sizes the badge relative to the icon; `overlay_margin` (default `0`) insets it from the corner
- `overlay_base_scale` (default `1.0`, clamped to 0.1–1.0) shrinks the app icon and places it in the **top-left** corner, matching xpra's own compositing (`ICON_SHRINKAGE=75`, `ICON_OVERLAY=50`) — set it to `0.75` for an exact xpra look
- Compositing happens in memory at config load — no pre-baked `.ico` files needed
- If a badge fails to load or compose, the rule falls back to its plain icon (a line is logged)

## Icons

Only `.ico` is supported (PNG cannot be used directly). Convert with Pillow:

```
python -c "from PIL import Image; im = Image.open(r'C:\path\to\icon.png').convert('RGBA'); im.save(r'icons\name.ico', sizes=[(16,16),(24,24),(32,32),(48,48),(64,64),(128,128)])"
```

- Put the `.ico` in `icons\` (next to the config) and reference it as `"icons\name.ico"`
- `make-test-ico.py` generates single-color test icons:
  `python make-test-ico.py icons\name.ico 220 60 60` (R G B)
- The committed `icons\*` are self-generated solid-color squares

> Keep third-party icons (vendor logos etc.) local only — they are gitignored in this checkout.

## Adding a new window type

1. Find the window's process name and title (xpra windows append ` on <host>` to the title)
2. Prepare an `.ico` in `icons\`
3. Add a rule at the right position in `iconmapper.toml`
4. Reload via the tray menu (or restart iconmapper)
5. Check `iconmapper.log` and the taskbar

## Autostart (optional)

Either drop a shortcut in the Startup folder or register a logon task. If using Task Scheduler, set `MultipleInstances = IgnoreNew` so a second start is ignored.

## Files

| File | Purpose |
|---|---|
| `src/main.rs` | the whole program |
| `iconmapper.example.toml` | config template |
| `icons\*.ico` | self-generated solid-color test icons |
| `make-test-ico.py` | solid-color .ico generator |
