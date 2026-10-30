//! chat-daddy — unified chat transcript viewer for Claude, Cursor, Codex, and custom sources.
//! Config: ~/.chat-daddy/config.json (auto-generated on first run)

#![windows_subsystem = "windows"]

mod font;
mod join;

use arboard::Clipboard;
use chrono::{Local, TimeZone};
use font::FontAtlas;
use image::{save_buffer, ColorType, GenericImageView};
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

const WIN_W: usize = 1000;
const WIN_H: usize = 700;
const PAD: i32 = 12;

const FONT_SIZES: [f32; 4] = [11.0, 14.0, 18.0, 24.0];
const DEFAULT_SIZE_IDX: usize = 1; // 14px

/// Query the actual NSView size on macOS, bypassing minifb's broken get_size().
/// minifb only updates width/height in viewDidEndLiveResize, which doesn't fire
/// for system-managed resizes (tile, maximize, Stage Manager).
#[cfg(target_os = "macos")]
fn native_view_size(window: &Window) -> Option<(usize, usize)> {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_ref() else { return None };
    let ns_view = appkit.ns_view.as_ptr();

    // Call [ns_view bounds] via objc_msgSend.
    // bounds returns NSRect { origin: NSPoint { x, y }, size: NSSize { width, height } }
    // On ARM64 macOS, structs <= 16 bytes are returned in registers; NSRect is 32 bytes
    // so we use objc_msgSend_stret on x86_64 and regular objc_msgSend on ARM64.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct NSRect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    extern "C" {
        fn sel_registerName(name: *const u8) -> *const std::ffi::c_void;
    }

    unsafe {
        let sel = sel_registerName(b"bounds\0".as_ptr());

        #[cfg(target_arch = "aarch64")]
        {
            // ARM64: large structs returned in registers via regular objc_msgSend
            extern "C" {
                fn objc_msgSend(obj: *mut std::ffi::c_void, sel: *const std::ffi::c_void) -> NSRect;
            }
            let rect = objc_msgSend(ns_view as *mut _, sel);
            Some((rect.width.max(1.0) as usize, rect.height.max(1.0) as usize))
        }

        #[cfg(target_arch = "x86_64")]
        {
            // x86_64: structs > 16 bytes use objc_msgSend_stret
            extern "C" {
                fn objc_msgSend_stret(
                    out: *mut NSRect,
                    obj: *mut std::ffi::c_void,
                    sel: *const std::ffi::c_void,
                );
            }
            let mut rect: NSRect = std::mem::zeroed();
            objc_msgSend_stret(&mut rect, ns_view as *mut _, sel);
            Some((rect.width.max(1.0) as usize, rect.height.max(1.0) as usize))
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn native_view_size(_window: &Window) -> Option<(usize, usize)> {
    None // other platforms: minifb's get_size() works correctly
}

/// Set the macOS dock icon from the embedded icon.png.
/// minifb doesn't do this — unbundled binaries get the default terminal icon.
///
/// objc_msgSend on ARM64 is NOT variadic — must transmute to exact function
/// pointer types per call, otherwise the compiler uses the wrong ABI.
#[cfg(target_os = "macos")]
fn set_macos_dock_icon() {
    let png_bytes = include_bytes!("../assets/icon.png");

    #[allow(clashing_extern_declarations)]
    extern "C" {
        fn sel_registerName(name: *const u8) -> *const std::ffi::c_void;
        fn objc_getClass(name: *const u8) -> *mut std::ffi::c_void;
        fn objc_msgSend(); // raw symbol — we transmute to correct signatures
    }

    type MsgSend0 = unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void) -> *mut std::ffi::c_void;
    type MsgSend1 = unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    type MsgSendDataWithBytes = unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void, *const std::ffi::c_void, usize) -> *mut std::ffi::c_void;

    unsafe {
        let send0: MsgSend0 = std::mem::transmute(objc_msgSend as *const ());
        let send1: MsgSend1 = std::mem::transmute(objc_msgSend as *const ());
        let send_data: MsgSendDataWithBytes = std::mem::transmute(objc_msgSend as *const ());

        // NSData *data = [NSData dataWithBytes:png_bytes length:len]
        let cls_nsdata = objc_getClass(b"NSData\0".as_ptr());
        let sel_data = sel_registerName(b"dataWithBytes:length:\0".as_ptr());
        let ns_data = send_data(
            cls_nsdata,
            sel_data,
            png_bytes.as_ptr() as *const std::ffi::c_void,
            png_bytes.len(),
        );
        if ns_data.is_null() { return; }

        // NSImage *image = [[NSImage alloc] initWithData:data]
        let cls_nsimage = objc_getClass(b"NSImage\0".as_ptr());
        let sel_alloc = sel_registerName(b"alloc\0".as_ptr());
        let sel_init = sel_registerName(b"initWithData:\0".as_ptr());
        let ns_image = send0(cls_nsimage, sel_alloc);
        let ns_image = send1(ns_image, sel_init, ns_data);
        if ns_image.is_null() { return; }

        // [[NSApplication sharedApplication] setApplicationIconImage:image]
        let cls_nsapp = objc_getClass(b"NSApplication\0".as_ptr());
        let sel_shared = sel_registerName(b"sharedApplication\0".as_ptr());
        let sel_set_icon = sel_registerName(b"setApplicationIconImage:\0".as_ptr());
        let app = send0(cls_nsapp, sel_shared);
        send1(app, sel_set_icon, ns_image);
    }
}

#[cfg(not(target_os = "macos"))]
fn set_macos_dock_icon() {
    // Windows icon is handled by build.rs / winresource
}

// --- default colors (used as fallbacks, overridable via config) ---
// These constants are used by helper functions (parse_inline_markdown, etc.)
// and get shadowed by theme values inside main()'s render loop.

const COL_BG: (u8, u8, u8) = (0x0d, 0x11, 0x17);
const COL_DIM: (u8, u8, u8) = (0x58, 0x58, 0x5a);
const COL_USER: (u8, u8, u8) = (0x58, 0xc4, 0xdc);
const COL_ASST: (u8, u8, u8) = (0xe6, 0xb3, 0x4d);
const COL_TEXT: (u8, u8, u8) = (0xc9, 0xd1, 0xd9);
const COL_SEL: (u8, u8, u8) = (0x21, 0x3d, 0x5a);
const COL_SEP: (u8, u8, u8) = (0x1a, 0x1e, 0x26);
const COL_HEADER_BG: (u8, u8, u8) = (0x14, 0x1a, 0x22);
const COL_ACCENT: (u8, u8, u8) = (0x58, 0xc4, 0xdc);
const COL_SEARCH_BG: (u8, u8, u8) = (0x1a, 0x24, 0x30);
const COL_TIMESTAMP: (u8, u8, u8) = (0x44, 0x6e, 0x7a);
const COL_SELECT_BG: (u8, u8, u8) = (0x26, 0x4f, 0x78);
const COL_CODE: (u8, u8, u8) = (0x9d, 0xc8, 0x9d);
const COL_CODE_BG: (u8, u8, u8) = (0x12, 0x1a, 0x14);
const COL_BOLD: (u8, u8, u8) = (0xf0, 0xf4, 0xf8);
const COL_TOGGLE: (u8, u8, u8) = (0x44, 0x88, 0xaa);
const COL_HEADING: (u8, u8, u8) = (0x6a, 0xd0, 0xe8);
const COL_MSG_TIME: (u8, u8, u8) = (0x3a, 0x5a, 0x66);

#[derive(Clone)]
struct Theme {
    bg: (u8, u8, u8),
    dim: (u8, u8, u8),
    user: (u8, u8, u8),
    asst: (u8, u8, u8),
    text: (u8, u8, u8),
    sel: (u8, u8, u8),
    sep: (u8, u8, u8),
    header_bg: (u8, u8, u8),
    accent: (u8, u8, u8),
    search_bg: (u8, u8, u8),
    timestamp: (u8, u8, u8),
    select_bg: (u8, u8, u8),
    code: (u8, u8, u8),
    code_bg: (u8, u8, u8),
    bold: (u8, u8, u8),
    toggle: (u8, u8, u8),
    heading: (u8, u8, u8),
    msg_time: (u8, u8, u8),
    remote_host: (u8, u8, u8),
    platform: (u8, u8, u8),
    datetime: (u8, u8, u8),
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            bg: COL_BG,
            dim: COL_DIM,
            user: COL_USER,
            asst: COL_ASST,
            text: COL_TEXT,
            sel: COL_SEL,
            sep: COL_SEP,
            header_bg: COL_HEADER_BG,
            accent: COL_ACCENT,
            search_bg: COL_SEARCH_BG,
            timestamp: COL_TIMESTAMP,
            select_bg: COL_SELECT_BG,
            code: COL_CODE,
            code_bg: COL_CODE_BG,
            bold: COL_BOLD,
            toggle: COL_TOGGLE,
            heading: COL_HEADING,
            msg_time: COL_MSG_TIME,
            remote_host: COL_ACCENT,
            platform: COL_DIM,
            datetime: COL_TIMESTAMP,
        }
    }
}

/// Parse "#RRGGBB" or "RRGGBB" hex string to (r, g, b)
fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn color_to_hex(c: (u8, u8, u8)) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

fn load_theme_from_value(val: &Value) -> Theme {
    let mut t = Theme::default();
    if let Some(colors) = val.get("colors").and_then(|c| c.as_object()) {
        macro_rules! col {
            ($key:expr, $field:ident) => {
                if let Some(s) = colors.get($key).and_then(|v| v.as_str()) {
                    if let Some(c) = parse_hex_color(s) {
                        t.$field = c;
                    }
                }
            };
        }
        col!("bg", bg);
        col!("dim", dim);
        col!("user", user);
        col!("assistant", asst);
        col!("text", text);
        col!("selection", sel);
        col!("separator", sep);
        col!("header_bg", header_bg);
        col!("accent", accent);
        col!("search_bg", search_bg);
        col!("timestamp", timestamp);
        col!("select_bg", select_bg);
        col!("code", code);
        col!("code_bg", code_bg);
        col!("bold", bold);
        col!("toggle", toggle);
        col!("heading", heading);
        col!("msg_time", msg_time);
        col!("remote_host", remote_host);
        col!("platform", platform);
        col!("datetime", datetime);
    }
    t
}

fn theme_to_json(t: &Theme) -> Value {
    serde_json::json!({
        "bg": color_to_hex(t.bg),
        "dim": color_to_hex(t.dim),
        "user": color_to_hex(t.user),
        "assistant": color_to_hex(t.asst),
        "text": color_to_hex(t.text),
        "selection": color_to_hex(t.sel),
        "separator": color_to_hex(t.sep),
        "header_bg": color_to_hex(t.header_bg),
        "accent": color_to_hex(t.accent),
        "search_bg": color_to_hex(t.search_bg),
        "timestamp": color_to_hex(t.timestamp),
        "select_bg": color_to_hex(t.select_bg),
        "code": color_to_hex(t.code),
        "code_bg": color_to_hex(t.code_bg),
        "bold": color_to_hex(t.bold),
        "toggle": color_to_hex(t.toggle),
        "heading": color_to_hex(t.heading),
        "remote_host": color_to_hex(t.remote_host),
        "platform": color_to_hex(t.platform),
        "datetime": color_to_hex(t.datetime),
        "msg_time": color_to_hex(t.msg_time),
    })
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

fn home_dir() -> PathBuf {
    let home = env::var("USERPROFILE")
        .or_else(|_| env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
}

fn chat_daddy_dir() -> PathBuf {
    let dir = home_dir().join(".chat-daddy");
    if !dir.exists() {
        let _ = fs::create_dir_all(&dir);
    }
    dir
}

fn config_path() -> PathBuf {
    chat_daddy_dir().join("config.json")
}

fn favorites_path() -> PathBuf {
    chat_daddy_dir().join("favorites.json")
}

fn names_path() -> PathBuf {
    chat_daddy_dir().join("names.json")
}

// --- source config ---

#[derive(Clone, Debug, PartialEq)]
enum SourceFormat {
    Claude,  // event-based: type/message.role/message.content, queue-operation
    Cursor,  // simple: role/message.content per line, <user_query> wrappers
    Codex,   // session_meta + response_item events with payload wrapper
    Generic, // try Claude-style, fall back to Cursor-style
}

#[derive(Clone)]
struct SourceConfig {
    name: String,       // display name: "claude", "cursor", "codex"
    format: SourceFormat,
    root: PathBuf,      // base directory to scan
    /// How to find JSONL files within root:
    /// "projects" — root/projects/<dir>/UUID.jsonl (Claude-style)
    /// "agent-transcripts" — root/projects/<dir>/agent-transcripts/UUID/UUID.jsonl (Cursor)
    /// "sessions" — root/sessions/**/*.jsonl (Codex date-tree)
    /// "flat" — root/**/*.jsonl (generic recursive)
    layout: String,
}

fn default_sources() -> Vec<SourceConfig> {
    let home = home_dir();
    let mut sources = Vec::new();

    // Claude
    let claude_root = home.join(".claude");
    if claude_root.join("projects").exists() {
        sources.push(SourceConfig {
            name: "claude".into(),
            format: SourceFormat::Claude,
            root: claude_root,
            layout: "projects".into(),
        });
    }

    // Cursor
    let cursor_root = home.join(".cursor-server");
    let cursor_root2 = home.join(".cursor");
    let cursor_base = if cursor_root.join("projects").exists() {
        Some(cursor_root)
    } else if cursor_root2.join("projects").exists() {
        Some(cursor_root2)
    } else {
        None
    };
    if let Some(cr) = cursor_base {
        sources.push(SourceConfig {
            name: "cursor".into(),
            format: SourceFormat::Cursor,
            root: cr,
            layout: "agent-transcripts".into(),
        });
    }

    // Codex
    let codex_root = home.join(".codex");
    if codex_root.join("sessions").exists() {
        sources.push(SourceConfig {
            name: "codex".into(),
            format: SourceFormat::Codex,
            root: codex_root,
            layout: "sessions".into(),
        });
    }

    sources
}

struct AppConfig {
    sources: Vec<SourceConfig>,
    font: String,       // font family name, e.g. "Fira Code"
    font_weight: u16,   // 300 = light, 400 = regular, 500 = medium
    theme: Theme,
    llm_endpoint: String, // local LLM for auto-naming, e.g. "http://localhost:1235"
    join_code: Option<String>, // LAN sync group; None = sync off
}

fn parse_sources(val: &Value) -> Vec<SourceConfig> {
    let Some(arr) = val.get("sources").and_then(|s| s.as_array()) else {
        return vec![];
    };
    let mut sources = Vec::new();
    for item in arr {
        let name = item
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("custom")
            .to_string();
        let format_str = item
            .get("format")
            .and_then(|f| f.as_str())
            .unwrap_or("generic");
        let format = match format_str {
            "claude" => SourceFormat::Claude,
            "cursor" => SourceFormat::Cursor,
            "codex" => SourceFormat::Codex,
            _ => SourceFormat::Generic,
        };
        let root_str = item
            .get("root")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let root = if root_str.starts_with("~/") || root_str.starts_with("~\\") {
            home_dir().join(&root_str[2..])
        } else {
            PathBuf::from(root_str)
        };
        let layout = item
            .get("layout")
            .and_then(|l| l.as_str())
            .unwrap_or("flat")
            .to_string();
        sources.push(SourceConfig {
            name,
            format,
            root,
            layout,
        });
    }
    sources
}

fn load_config() -> AppConfig {
    let path = config_path();

    // migrate from old ~/.claude/ location if new path doesn't exist
    if !path.exists() {
        let old = home_dir().join(".claude").join("chat-daddy.json");
        if old.exists() {
            let _ = fs::copy(&old, &path);
        }
        let old_favs = home_dir().join(".claude").join("chat-daddy-favorites.json");
        let new_favs = favorites_path();
        if old_favs.exists() && !new_favs.exists() {
            let _ = fs::copy(&old_favs, &new_favs);
        }
    }

    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(val) = serde_json::from_str::<Value>(&data) {
            let sources = parse_sources(&val);
            let font = val
                .get("font")
                .and_then(|f| f.as_str())
                .unwrap_or("Fira Code")
                .to_string();
            let font_weight = val
                .get("font_weight")
                .and_then(|w| w.as_u64())
                .unwrap_or(300) as u16;
            let theme = load_theme_from_value(&val);
            let llm_endpoint = val
                .get("llm_endpoint")
                .and_then(|e| e.as_str())
                .unwrap_or("http://localhost:1235")
                .to_string();
            let join_code = val
                .get("join_code")
                .and_then(|c| c.as_str())
                .and_then(join::normalize);

            if !sources.is_empty() {
                return AppConfig {
                    sources,
                    font,
                    font_weight,
                    theme,
                    llm_endpoint,
                    join_code,
                };
            }
        }
    }

    // auto-detect defaults and write config
    let sources = default_sources();
    let cfg = AppConfig {
        sources,
        font: "Fira Code".into(),
        font_weight: 300,
        theme: Theme::default(),
        llm_endpoint: "http://localhost:1235".into(),
        join_code: None,
    };
    save_config(&cfg);
    cfg
}

fn sources_to_json(sources: &[SourceConfig]) -> Vec<Value> {
    sources
        .iter()
        .map(|s| {
            let format_str = match s.format {
                SourceFormat::Claude => "claude",
                SourceFormat::Cursor => "cursor",
                SourceFormat::Codex => "codex",
                SourceFormat::Generic => "generic",
            };
            let home = home_dir();
            let root_str = if let Ok(rel) = s.root.strip_prefix(&home) {
                format!("~/{}", rel.to_string_lossy().replace('\\', "/"))
            } else {
                s.root.to_string_lossy().into_owned()
            };
            serde_json::json!({
                "name": s.name,
                "format": format_str,
                "root": root_str,
                "layout": s.layout,
            })
        })
        .collect()
}

fn load_available_themes() -> Vec<(String, String, Theme)> {
    let mut themes = Vec::new();
    let mut seen = HashSet::new();

    // Built-in themes embedded at compile time
    let builtin: &[(&str, &str)] = &[
        ("catppuccin", include_str!("../themes/catppuccin.json")),
        ("dracula", include_str!("../themes/dracula.json")),
        ("gruvbox", include_str!("../themes/gruvbox.json")),
        ("light", include_str!("../themes/light.json")),
        ("midnight", include_str!("../themes/midnight.json")),
        ("monokai", include_str!("../themes/monokai.json")),
        ("nord", include_str!("../themes/nord.json")),
        ("rose-pine", include_str!("../themes/rose-pine.json")),
        ("solarized-dark", include_str!("../themes/solarized-dark.json")),
        ("tokyo-night", include_str!("../themes/tokyo-night.json")),
    ];
    for (stem, data) in builtin {
        if let Ok(val) = serde_json::from_str::<Value>(data) {
            let name = val.get("name").and_then(|n| n.as_str()).unwrap_or(stem).to_string();
            let theme = load_theme_from_value(&val);
            seen.insert(stem.to_string());
            themes.push((stem.to_string(), name, theme));
        }
    }

    // User themes from ~/.chat-daddy/themes/ (override built-in by name)
    let user_dir = chat_daddy_dir().join("themes");
    if let Ok(entries) = fs::read_dir(&user_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            if seen.contains(&stem) { continue; }
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(val) = serde_json::from_str::<Value>(&data) {
                    let name = val.get("name").and_then(|n| n.as_str()).unwrap_or(&stem).to_string();
                    let theme = load_theme_from_value(&val);
                    seen.insert(stem.clone());
                    themes.push((stem, name, theme));
                }
            }
        }
    }
    themes.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    themes
}

fn save_config(cfg: &AppConfig) {
    let config = serde_json::json!({
        "font": cfg.font,
        "font_weight": cfg.font_weight,
        "llm_endpoint": cfg.llm_endpoint,
        "join_code": cfg.join_code,
        "colors": theme_to_json(&cfg.theme),
        "sources": sources_to_json(&cfg.sources),
    });
    if let Ok(json) = serde_json::to_string_pretty(&config) {
        let _ = fs::write(config_path(), json);
    }
}

// --- per-chat metadata (persisted in chats.json) ---

#[derive(Clone, Default)]
struct ChatMeta {
    name: Option<String>,   // user-given or LLM-generated name
    starred: bool,
    auto_named: bool,       // true if name came from LLM (user rename clears this)
}

fn chats_path() -> PathBuf {
    chat_daddy_dir().join("chats.json")
}

fn load_chats() -> HashMap<String, ChatMeta> {
    let path = chats_path();

    // migrate from old separate files if chats.json doesn't exist
    if !path.exists() {
        let mut map = HashMap::new();

        // migrate favorites
        let fav_path = favorites_path();
        if let Ok(data) = fs::read_to_string(&fav_path) {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(&data) {
                for uuid in arr {
                    map.entry(uuid).or_insert_with(ChatMeta::default).starred = true;
                }
            }
        }

        // migrate names
        let names_p = names_path();
        if let Ok(data) = fs::read_to_string(&names_p) {
            if let Ok(names) = serde_json::from_str::<HashMap<String, String>>(&data) {
                for (uuid, name) in names {
                    let meta = map.entry(uuid).or_insert_with(ChatMeta::default);
                    meta.name = Some(name);
                }
            }
        }

        if !map.is_empty() {
            save_chats(&map);
        }
        return map;
    }

    let Ok(data) = fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let Ok(val) = serde_json::from_str::<Value>(&data) else {
        return HashMap::new();
    };
    let Some(obj) = val.as_object() else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for (uuid, meta_val) in obj {
        let meta = ChatMeta {
            name: meta_val.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()),
            starred: meta_val.get("starred").and_then(|s| s.as_bool()).unwrap_or(false),
            auto_named: meta_val.get("auto_named").and_then(|a| a.as_bool()).unwrap_or(false),
        };
        map.insert(uuid.clone(), meta);
    }
    map
}

fn save_chats(chats: &HashMap<String, ChatMeta>) {
    let mut obj = serde_json::Map::new();
    for (uuid, meta) in chats {
        let mut entry = serde_json::Map::new();
        if let Some(ref name) = meta.name {
            entry.insert("name".into(), Value::String(name.clone()));
        }
        if meta.starred {
            entry.insert("starred".into(), Value::Bool(true));
        }
        if meta.auto_named {
            entry.insert("auto_named".into(), Value::Bool(true));
        }
        // only write entry if it has data
        if !entry.is_empty() {
            obj.insert(uuid.clone(), Value::Object(entry));
        }
    }
    if let Ok(json) = serde_json::to_string_pretty(&Value::Object(obj)) {
        let _ = fs::write(chats_path(), json);
    }
}

// --- LLM auto-naming ---

/// Build a naming prompt from user messages: first 2 + last 2, truncated to 200 chars each
fn build_naming_prompt(msgs: &[MessageLine]) -> String {
    let user_msgs: Vec<&MessageLine> = msgs.iter().filter(|m| m.role == "user").collect();
    if user_msgs.is_empty() {
        return String::new();
    }
    let mut selected: Vec<&MessageLine> = Vec::new();
    // first 2
    for m in user_msgs.iter().take(2) {
        selected.push(m);
    }
    // last 2 (deduped by pointer)
    let len = user_msgs.len();
    for m in user_msgs.iter().skip(len.saturating_sub(2)) {
        if !selected.iter().any(|s| std::ptr::eq(*s, *m)) {
            selected.push(m);
        }
    }
    let transcript: String = selected
        .iter()
        .map(|m| {
            let trunc: String = m.text.chars().take(200).collect();
            format!("User: {}", trunc)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Name this conversation:\n\n{}", transcript)
}

/// Ask local LLM to name a chat. Returns None if server is down or error.
fn llm_auto_name(endpoint: &str, msgs: &[MessageLine]) -> Option<String> {
    let prompt = build_naming_prompt(msgs);
    if prompt.is_empty() {
        return None;
    }
    let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": "qwen2.5-0.5b",
        "messages": [
            {
                "role": "system",
                "content": "You are a chat naming assistant. Given a conversation, output ONLY a short title (2-6 words). No quotes, no explanation, no punctuation at the end."
            },
            {
                "role": "user",
                "content": prompt
            }
        ],
        "max_tokens": 20,
        "temperature": 0.3
    });
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(5))
        .send_string(&body.to_string())
        .ok()?;
    let resp_body = resp.into_string().ok()?;
    let val: Value = serde_json::from_str(&resp_body).ok()?;
    let name = val
        .get("choices")
        .and_then(|c: &Value| c.get(0))
        .and_then(|c: &Value| c.get("message"))
        .and_then(|m: &Value| m.get("content"))
        .and_then(|c: &Value| c.as_str())
        .map(|s: &str| s.trim().to_string())?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Check LLM health (returns true if server responds)
fn llm_is_healthy(endpoint: &str) -> bool {
    let url = format!("{}/health", endpoint.trim_end_matches('/'));
    ureq::get(&url)
        .timeout(std::time::Duration::from_secs(1))
        .call()
        .is_ok()
}

// --- transcript data ---

#[derive(Clone)]
struct RemoteOrigin {
    hostname: String,
    tcp_addr: SocketAddr,
    key: join::GroupKey,
}

#[derive(Clone)]
struct TranscriptEntry {
    path: PathBuf,
    uuid: String,
    project: String, // display label: "claude", "cursor", "codex", or custom name
    source_format: SourceFormat,
    mtime_secs: u64,
    preview: String,
    last_preview: String,
    timestamp: String,
    remote: Option<RemoteOrigin>,
    remote_name: Option<String>, // chat name from peer's chats.json
}

// --- LAN peer sync ---

const LAN_PORT: u16 = 21847;
const MAX_PEER_CONNS: usize = 16;

/// Current group key; None means sync is off.
type SharedKey = Arc<Mutex<Option<join::GroupKey>>>;

struct PeerInfo {
    hostname: String,
    #[allow(dead_code)]
    tcp_port: u16,
    last_seen: Instant,
}

struct PeerState {
    peers: HashMap<SocketAddr, PeerInfo>,
    remote_entries: Vec<TranscriptEntry>,
    dirty: bool,
}

fn format_timestamp(secs: u64) -> String {
    let Some(dt) = Local.timestamp_opt(secs as i64, 0).single() else {
        return String::new();
    };
    let now = Local::now();
    let today = now.date_naive();
    let entry_date = dt.date_naive();
    let days_ago = (today - entry_date).num_days();
    if days_ago == 0 {
        dt.format("today %l:%M %p").to_string().trim().to_string()
    } else if days_ago == 1 {
        dt.format("yesterday %l:%M %p")
            .to_string()
            .trim()
            .to_string()
    } else if days_ago < 7 {
        dt.format("%a %l:%M %p").to_string().trim().to_string()
    } else {
        dt.format("%b %d, %l:%M %p").to_string().trim().to_string()
    }
}

/// Format a UNIX timestamp suitable for per-message display in view mode
fn format_message_time(secs: u64) -> String {
    let Some(dt) = Local.timestamp_opt(secs as i64, 0).single() else {
        return String::new();
    };
    let now = Local::now();
    let today = now.date_naive();
    let entry_date = dt.date_naive();
    let days_ago = (today - entry_date).num_days();
    if days_ago == 0 {
        dt.format("%l:%M %p").to_string().trim().to_string()
    } else if days_ago == 1 {
        format!("yesterday {}", dt.format("%l:%M %p").to_string().trim())
    } else if days_ago < 7 {
        dt.format("%a %l:%M %p").to_string().trim().to_string()
    } else {
        dt.format("%b %d %l:%M %p").to_string().trim().to_string()
    }
}

fn find_all_jsonl(dir: &Path, acc: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            find_all_jsonl(&path, acc);
        } else if path.extension().map_or(false, |e| e == "jsonl") {
            acc.push(path);
        }
    }
}

fn is_top_level_session(base: &Path, path: &Path) -> bool {
    path.parent()
        .and_then(|p| p.strip_prefix(base).ok())
        .map(|rel| rel.components().count() == 1)
        .unwrap_or(false)
}

/// Discover JSONL files for a Claude-layout source: root/projects/<dir>/UUID.jsonl
fn find_claude_transcripts(src: &SourceConfig) -> Vec<TranscriptEntry> {
    let base = src.root.join("projects");
    if !base.exists() {
        return vec![];
    }
    let mut files = Vec::new();
    find_all_jsonl(&base, &mut files);
    files
        .into_iter()
        .filter(|p| is_top_level_session(&base, p))
        .filter_map(|path| make_entry(path, &src.name, src.format.clone()))
        .collect()
}

/// Discover JSONL files for Cursor: root/projects/<dir>/agent-transcripts/UUID/UUID.jsonl
fn find_cursor_transcripts(src: &SourceConfig) -> Vec<TranscriptEntry> {
    let base = src.root.join("projects");
    if !base.exists() {
        return vec![];
    }
    let mut out = Vec::new();
    // iterate project dirs
    let Ok(proj_dirs) = fs::read_dir(&base) else {
        return vec![];
    };
    for proj in proj_dirs.flatten() {
        if !proj.path().is_dir() {
            continue;
        }
        let at_dir = proj.path().join("agent-transcripts");
        if !at_dir.exists() {
            continue;
        }
        let Ok(sessions) = fs::read_dir(&at_dir) else {
            continue;
        };
        for session in sessions.flatten() {
            if !session.path().is_dir() {
                continue;
            }
            // look for UUID.jsonl inside the session dir
            let mut files = Vec::new();
            find_all_jsonl(&session.path(), &mut files);
            for path in files {
                if let Some(entry) = make_entry(path, &src.name, src.format.clone()) {
                    out.push(entry);
                }
            }
        }
    }
    out
}

/// Discover JSONL files for Codex: root/sessions/YYYY/MM/DD/*.jsonl
fn find_codex_transcripts(src: &SourceConfig) -> Vec<TranscriptEntry> {
    let base = src.root.join("sessions");
    if !base.exists() {
        return vec![];
    }
    let mut files = Vec::new();
    find_all_jsonl(&base, &mut files);
    files
        .into_iter()
        .filter_map(|path| make_entry(path, &src.name, src.format.clone()))
        .collect()
}

/// Discover JSONL files with flat recursive scan
fn find_flat_transcripts(src: &SourceConfig) -> Vec<TranscriptEntry> {
    if !src.root.exists() {
        return vec![];
    }
    let mut files = Vec::new();
    find_all_jsonl(&src.root, &mut files);
    files
        .into_iter()
        .filter_map(|path| make_entry(path, &src.name, src.format.clone()))
        .collect()
}

fn make_entry(path: PathBuf, source_name: &str, format: SourceFormat) -> Option<TranscriptEntry> {
    let meta = fs::metadata(&path).ok()?;
    let uuid = path.file_stem()?.to_string_lossy().into_owned();
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let preview = preview_first_line(&path, &format).unwrap_or_default();
    let last_preview = preview_last_line(&path, &format).unwrap_or_default();
    let timestamp = format_timestamp(mtime_secs);
    Some(TranscriptEntry {
        path,
        uuid,
        project: source_name.to_string(),
        source_format: format,
        mtime_secs,
        preview,
        last_preview,
        timestamp,
        remote: None,
        remote_name: None,
    })
}

fn get_all_transcripts(sources: &[SourceConfig]) -> Vec<TranscriptEntry> {
    let mut all = Vec::new();
    for src in sources {
        let entries = match src.layout.as_str() {
            "projects" => find_claude_transcripts(src),
            "agent-transcripts" => find_cursor_transcripts(src),
            "sessions" => find_codex_transcripts(src),
            _ => find_flat_transcripts(src),
        };
        all.extend(entries);
    }
    all.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    all
}

/// Extract preview text from a JSONL value (format-aware)
fn preview_from_value(v: &Value, format: &SourceFormat) -> String {
    match format {
        SourceFormat::Codex => {
            // Codex: payload.content[].text or payload.content[].input_text
            let content = v
                .get("payload")
                .and_then(|p| p.get("content"))
                .or_else(|| v.get("message").and_then(|m| m.get("content")));
            let Some(c) = content else {
                return String::new();
            };
            if let Some(arr) = c.as_array() {
                return arr
                    .iter()
                    .filter_map(|x| {
                        x.get("text")
                            .or_else(|| x.get("input_text"))
                            .and_then(|t| t.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string();
            }
            String::new()
        }
        _ => {
            // Claude + Cursor: message.content (string or array)
            let content = v.get("message").and_then(|m| m.get("content"));
            let Some(c) = content else {
                return String::new();
            };
            if let Some(s) = c.as_str() {
                return s.to_string();
            }
            if let Some(arr) = c.as_array() {
                return arr
                    .iter()
                    .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string();
            }
            String::new()
        }
    }
}

fn preview_first_line(path: &Path, format: &SourceFormat) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(20) {
        let line = line.ok()?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let j: Value = serde_json::from_str(&line).ok()?;
        // skip non-content entries
        let entry_type = j.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if entry_type == "queue-operation" || entry_type == "session_meta" {
            continue;
        }
        let text = strip_query_wrapper(&preview_from_value(&j, format));
        if !text.is_empty() {
            let short: String = text.chars().take(120).collect();
            let short = if text.chars().count() > 120 {
                format!("{}..", short)
            } else {
                short
            };
            return Some(short.replace('\n', " ").trim().to_string());
        }
    }
    None
}

fn preview_last_line(path: &Path, format: &SourceFormat) -> Option<String> {
    // Read from end — collect last valid content line
    let data = fs::read_to_string(path).ok()?;
    let mut last_text: Option<String> = None;
    for line in data.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let j: Value = serde_json::from_str(line).ok()?;
        let entry_type = j.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if entry_type == "queue-operation" || entry_type == "session_meta" {
            continue;
        }
        let text = strip_query_wrapper(&preview_from_value(&j, format));
        if !text.is_empty() {
            let short: String = text.chars().take(300).collect();
            let short = if text.chars().count() > 300 {
                format!("{}..", short)
            } else {
                short
            };
            last_text = Some(short.trim().to_string());
            break;
        }
    }
    last_text
}

// --- message parsing ---

#[derive(Clone)]
struct MessageLine {
    role: String,
    text: String,
    timestamp: u64, // unix seconds from JSONL timestamp field
}

fn extract_text(v: &Value) -> String {
    if v.get("type").and_then(|t| t.as_str()) == Some("queue-operation") {
        return String::new();
    }
    let content = v.get("message").and_then(|m| m.get("content"));
    let Some(c) = content else {
        return String::new();
    };
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(arr) = c.as_array() {
        return arr
            .iter()
            .filter_map(|x| {
                let t = x.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match t {
                    "text" => x.get("text").and_then(|t| t.as_str()),
                    "thinking" => x.get("thinking").and_then(|t| t.as_str()),
                    _ => None, // skip tool_use, tool_result, etc
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn get_role(v: &Value) -> &str {
    if let Some(t) = v.get("type").and_then(|t| t.as_str()) {
        if t == "user" || t == "assistant" {
            return t;
        }
    }
    v.get("role")
        .or_else(|| v.get("message").and_then(|m| m.get("role")))
        .and_then(|r| r.as_str())
        .unwrap_or("unknown")
}

fn extract_timestamp(v: &Value) -> u64 {
    // try "timestamp" field (ISO string or unix number)
    if let Some(ts) = v.get("timestamp") {
        if let Some(n) = ts.as_u64() {
            return n;
        }
        if let Some(n) = ts.as_f64() {
            return n as u64;
        }
        if let Some(s) = ts.as_str() {
            // try parsing ISO 8601
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return dt.timestamp() as u64;
            }
            // try "2024-01-15T10:30:00.000Z" variant
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ") {
                return dt.and_utc().timestamp() as u64;
            }
        }
    }
    0
}

fn strip_query_wrapper(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("<user_query>").unwrap_or(s).trim();
    let s = s.strip_suffix("</user_query>").unwrap_or(s).trim();
    s.to_string()
}

fn load_messages(path: &Path, format: &SourceFormat) -> Vec<MessageLine> {
    match format {
        SourceFormat::Codex => load_messages_codex(path),
        SourceFormat::Cursor => load_messages_cursor(path),
        _ => load_messages_claude(path),
    }
}

/// Claude format: event-based JSONL with type, message.role, message.content
fn load_messages_claude(path: &Path) -> Vec<MessageLine> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        _ => return vec![],
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(x) => x,
            _ => continue,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(x) => x,
            _ => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("queue-operation") {
            continue;
        }
        let text = extract_text(&v);
        if text.is_empty() {
            continue;
        }
        let role = get_role(&v).to_string();
        let text = strip_query_wrapper(&text);
        let timestamp = extract_timestamp(&v);
        out.push(MessageLine {
            role,
            text,
            timestamp,
        });
    }
    out
}

/// Cursor format: {role, message: {content: [{type, text}]}} per line
fn load_messages_cursor(path: &Path) -> Vec<MessageLine> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        _ => return vec![],
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(x) => x,
            _ => continue,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(x) => x,
            _ => continue,
        };
        let role = v
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown")
            .to_string();
        // content is array of {type, text}
        let content = v.get("message").and_then(|m| m.get("content"));
        let text = match content {
            Some(c) if c.is_array() => c
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            Some(c) if c.is_string() => c.as_str().unwrap_or("").to_string(),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        let text = strip_query_wrapper(&text);
        let timestamp = extract_timestamp(&v);
        out.push(MessageLine {
            role,
            text,
            timestamp,
        });
    }
    out
}

/// Codex format: {type: "response_item", payload: {role, content: [{type, text/input_text}]}}
fn load_messages_codex(path: &Path) -> Vec<MessageLine> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        _ => return vec![],
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(x) => x,
            _ => continue,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(x) => x,
            _ => continue,
        };
        let entry_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if entry_type == "session_meta" {
            continue;
        }
        // timestamp is top-level
        let timestamp = extract_timestamp(&v);

        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        let role = payload
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        // map codex roles: "developer" -> "user", "assistant" stays
        let role = match role {
            "developer" | "user" => "user".to_string(),
            r => r.to_string(),
        };

        let content = payload.get("content");
        let text = match content {
            Some(c) if c.is_array() => c
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|x| {
                    let t = x.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match t {
                        "output_text" | "input_text" | "text" => {
                            x.get("text")
                                .or_else(|| x.get("input_text"))
                                .and_then(|t| t.as_str())
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        let text = strip_query_wrapper(&text);
        out.push(MessageLine {
            role,
            text,
            timestamp,
        });
    }
    out
}

// --- message grouping ---

#[derive(Clone)]
struct MessageGroup {
    role: String,
    messages: Vec<MessageLine>,
}

fn group_messages(msgs: &[MessageLine]) -> Vec<MessageGroup> {
    let mut groups: Vec<MessageGroup> = Vec::new();
    for m in msgs {
        if let Some(last) = groups.last_mut() {
            if last.role == m.role {
                last.messages.push(m.clone());
                continue;
            }
        }
        groups.push(MessageGroup {
            role: m.role.clone(),
            messages: vec![m.clone()],
        });
    }
    groups
}

// --- layout helpers ---

fn chars_per_line(win_w: usize, advance: i32) -> usize {
    let usable = (win_w as i32) - 2 * PAD;
    (usable / advance).max(20) as usize
}

const LIST_LINES_PER_ITEM: i32 = 4;
const LIST_EXTRA_SELECTED: i32 = 2; // extra preview lines for highlighted item

/// Compute the y-offset (in lines) of item `idx` in the list, accounting for
/// the selected item being taller.
fn list_item_y_lines(idx: usize, selected: usize, count: usize) -> i32 {
    let mut y = 0i32;
    for i in 0..idx.min(count) {
        y += if i == selected { LIST_LINES_PER_ITEM + LIST_EXTRA_SELECTED } else { LIST_LINES_PER_ITEM };
    }
    y
}

fn list_total_lines(count: usize, selected: usize) -> i32 {
    (count as i32) * LIST_LINES_PER_ITEM + if selected < count { LIST_EXTRA_SELECTED } else { 0 }
}

fn wrap_str(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in s.lines() {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut pos = 0;
        while pos < chars.len() {
            let remaining = chars.len() - pos;
            if remaining <= width {
                out.push(chars[pos..].iter().collect());
                break;
            }
            let end = pos + width;
            let mut brk = end;
            for i in (pos..end).rev() {
                if chars[i] == ' ' {
                    brk = i;
                    break;
                }
            }
            if brk == end {
                let chunk: String = chars[pos..end].iter().collect();
                out.push(chunk);
                pos = end;
            } else {
                let chunk: String = chars[pos..brk].iter().collect();
                out.push(chunk);
                pos = brk + 1;
            }
        }
    }
    if out.is_empty() && !s.is_empty() {
        out.push(s.to_string());
    }
    out
}

/// Line metadata for the flattened view
#[derive(Clone)]
enum LineMeta {
    Normal,
    Timestamp,
    Toggle(usize), // group_idx — clickable to expand/collapse
}

fn flatten_groups(
    groups: &[MessageGroup],
    expanded: &HashSet<usize>,
    wrap_width: usize,
) -> (Vec<String>, Vec<LineMeta>) {
    let label_user = "[user] ";
    let label_asst = "[asst] ";
    let prefix_len = label_user.len();
    let text_width = wrap_width.saturating_sub(prefix_len).max(10);
    let continued = "       ";

    let mut lines = Vec::new();
    let mut meta = Vec::new();

    for (gi, group) in groups.iter().enumerate() {
        // blank separator between groups
        if gi > 0 {
            lines.push(String::new());
            meta.push(LineMeta::Normal);
        }

        let label = if group.role == "user" {
            label_user
        } else {
            label_asst
        };

        let is_asst = group.role != "user";
        let collapsed = is_asst && group.messages.len() > 1 && !expanded.contains(&gi);

        if collapsed {
            // show timestamp of last message
            let last = &group.messages[group.messages.len() - 1];
            if last.timestamp > 0 {
                let ts = format_message_time(last.timestamp);
                lines.push(format!("       {}", ts));
                meta.push(LineMeta::Timestamp);
            }

            // toggle indicator
            let hidden = group.messages.len() - 1;
            let toggle = format!(
                "       .. {} more response{} [click to expand]",
                hidden,
                if hidden == 1 { "" } else { "s" }
            );
            lines.push(toggle);
            meta.push(LineMeta::Toggle(gi));

            // render only last message text
            flatten_single_msg(last, label, continued, text_width, &mut lines, &mut meta);
        } else {
            // show all messages in group
            for (mi, m) in group.messages.iter().enumerate() {
                if mi > 0 {
                    lines.push(String::new());
                    meta.push(LineMeta::Normal);
                }

                // timestamp above each message
                if m.timestamp > 0 {
                    let ts = format_message_time(m.timestamp);
                    lines.push(format!("       {}", ts));
                    meta.push(LineMeta::Timestamp);
                }

                let msg_label = if mi == 0 { label } else { label };
                flatten_single_msg(m, msg_label, continued, text_width, &mut lines, &mut meta);
            }

            // collapse indicator for expanded multi-message groups
            if is_asst && group.messages.len() > 1 {
                let toggle = "       .. [click to collapse]".to_string();
                lines.push(toggle);
                meta.push(LineMeta::Toggle(gi));
            }
        }
    }
    (lines, meta)
}

fn flatten_single_msg(
    m: &MessageLine,
    label: &str,
    continued: &str,
    text_width: usize,
    lines: &mut Vec<String>,
    meta: &mut Vec<LineMeta>,
) {
    let text_lines: Vec<&str> = m.text.lines().collect();
    for (i, line) in text_lines.iter().enumerate() {
        let blocks = wrap_str(line, text_width);
        for (j, block) in blocks.iter().enumerate() {
            let prefix = if i == 0 && j == 0 { label } else { continued };
            lines.push(format!("{}{}", prefix, block));
            meta.push(LineMeta::Normal);
        }
    }
}

// --- markdown rendering ---

struct TextRun {
    text: String,
    color: (u8, u8, u8),
    bg: Option<(u8, u8, u8)>,
}

fn parse_inline_markdown(line: &str, base_color: (u8, u8, u8)) -> Vec<TextRun> {
    let chars: Vec<char> = line.chars().collect();
    let mut runs = Vec::new();
    let mut i = 0;
    let mut current = String::new();

    while i < chars.len() {
        // **bold**
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            let start = i + 2;
            let mut end = None;
            for j in start..chars.len().saturating_sub(1) {
                if chars[j] == '*' && chars[j + 1] == '*' {
                    end = Some(j);
                    break;
                }
            }
            if let Some(e) = end {
                if !current.is_empty() {
                    runs.push(TextRun {
                        text: std::mem::take(&mut current),
                        color: base_color,
                        bg: None,
                    });
                }
                let text: String = chars[start..e].iter().collect();
                runs.push(TextRun {
                    text,
                    color: COL_BOLD,
                    bg: None,
                });
                i = e + 2;
                continue;
            }
        }

        // `inline code`
        if chars[i] == '`' && (i + 1 >= chars.len() || chars[i + 1] != '`') {
            let start = i + 1;
            let mut end = None;
            for j in start..chars.len() {
                if chars[j] == '`' {
                    end = Some(j);
                    break;
                }
            }
            if let Some(e) = end {
                if !current.is_empty() {
                    runs.push(TextRun {
                        text: std::mem::take(&mut current),
                        color: base_color,
                        bg: None,
                    });
                }
                let text: String = chars[start..e].iter().collect();
                runs.push(TextRun {
                    text,
                    color: COL_CODE,
                    bg: Some(COL_CODE_BG),
                });
                i = e + 1;
                continue;
            }
        }

        current.push(chars[i]);
        i += 1;
    }

    if !current.is_empty() {
        runs.push(TextRun {
            text: current,
            color: base_color,
            bg: None,
        });
    }
    runs
}

fn compute_code_block_state(lines: &[String]) -> Vec<bool> {
    let mut state = Vec::with_capacity(lines.len());
    let mut inside = false;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            state.push(true); // fence line gets code styling
            inside = !inside;
        } else {
            state.push(inside);
        }
    }
    state
}

// --- rendering (fontdue) ---

fn draw_text_ttf(
    buf: &mut [u32],
    x: i32,
    y: i32,
    text: &str,
    color: (u8, u8, u8),
    atlas: &mut FontAtlas,
    buf_w: usize,
    buf_h: usize,
) {
    let mut px = x;
    let baseline = y + atlas.ascent;
    for ch in text.chars() {
        let g = atlas.glyph(ch);
        let gx = px + g.x_off;
        let gy = baseline - g.height as i32 - g.y_off;
        for row in 0..g.height {
            for col in 0..g.width {
                let alpha = g.bitmap[row * g.width + col];
                if alpha == 0 {
                    continue;
                }
                let rx = gx + col as i32;
                let ry = gy + row as i32;
                if rx >= 0 && ry >= 0 {
                    let ux = rx as usize;
                    let uy = ry as usize;
                    if ux < buf_w && uy < buf_h {
                        let a = alpha as u32;
                        let bg = buf[uy * buf_w + ux];
                        let bg_r = (bg >> 16) & 0xFF;
                        let bg_g = (bg >> 8) & 0xFF;
                        let bg_b = bg & 0xFF;
                        let r = (color.0 as u32 * a + bg_r * (255 - a)) / 255;
                        let g = (color.1 as u32 * a + bg_g * (255 - a)) / 255;
                        let b = (color.2 as u32 * a + bg_b * (255 - a)) / 255;
                        buf[uy * buf_w + ux] = (r << 16) | (g << 8) | b;
                    }
                }
            }
        }
        px += atlas.advance;
    }
}

fn draw_styled_runs(
    buf: &mut [u32],
    x: i32,
    y: i32,
    runs: &[TextRun],
    atlas: &mut FontAtlas,
    buf_w: usize,
    buf_h: usize,
) {
    let mut px = x;
    let lh = atlas.line_height;
    for run in runs {
        let char_count = run.text.chars().count() as i32;
        if let Some(bg) = run.bg {
            fill_rect(buf, px, y, char_count * atlas.advance + 2, lh, bg, buf_w, buf_h);
        }
        draw_text_ttf(buf, px, y, &run.text, run.color, atlas, buf_w, buf_h);
        px += char_count * atlas.advance;
    }
}

fn fill_rect(
    buf: &mut [u32],
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: (u8, u8, u8),
    buf_w: usize,
    buf_h: usize,
) {
    let pix = rgb(color.0, color.1, color.2);
    for dy in 0..h {
        for dx in 0..w {
            let ux = (x + dx) as usize;
            let uy = (y + dy) as usize;
            if ux < buf_w && uy < buf_h {
                buf[uy * buf_w + ux] = pix;
            }
        }
    }
}

/// Load icon PNG and return as (width, height, Vec<u32>) pixel buffer.
/// Applies circular mask. Returns None on failure.
fn load_icon_pixels(size: u32) -> Option<(u32, u32, Vec<u32>)> {
    // Icon is embedded in the binary at compile time
    let icon_bytes = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(icon_bytes).ok()?;
    // Resize to target size, crop to square first
    let (w, h) = img.dimensions();
    let s = w.min(h);
    let left = (w - s) / 2;
    let top = (h - s) / 2;
    let cropped = img.crop_imm(left, top, s, s);
    let resized = cropped.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
    let rgba = resized.to_rgba8();
    let mut pixels = Vec::with_capacity((size * size) as usize);
    let cx = size as f64 / 2.0;
    let cy = size as f64 / 2.0;
    let r = size as f64 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let px = rgba.get_pixel(x, y);
            let dist = ((x as f64 - cx + 0.5).powi(2) + (y as f64 - cy + 0.5).powi(2)).sqrt();
            if dist > r {
                pixels.push(0); // transparent (will blend with bg)
            } else {
                let a = px[3] as u32;
                let rv = (px[0] as u32 * a) / 255;
                let gv = (px[1] as u32 * a) / 255;
                let bv = (px[2] as u32 * a) / 255;
                pixels.push((rv << 16) | (gv << 8) | bv | (a << 24));
            }
        }
    }
    Some((size, size, pixels))
}

/// Blit an ARGB pixel buffer onto the framebuffer with alpha blending.
fn blit_icon(
    buf: &mut [u32],
    dx: i32,
    dy: i32,
    icon: &[u32],
    icon_w: u32,
    icon_h: u32,
    buf_w: usize,
    buf_h: usize,
) {
    for iy in 0..icon_h as i32 {
        for ix in 0..icon_w as i32 {
            let sx = dx + ix;
            let sy = dy + iy;
            if sx < 0 || sy < 0 || sx as usize >= buf_w || sy as usize >= buf_h {
                continue;
            }
            let src = icon[(iy as u32 * icon_w + ix as u32) as usize];
            let a = (src >> 24) & 0xFF;
            if a == 0 {
                continue;
            }
            let dst_idx = sy as usize * buf_w + sx as usize;
            if a == 255 {
                buf[dst_idx] = src & 0x00FFFFFF;
            } else {
                let bg = buf[dst_idx];
                let inv_a = 255 - a;
                let r = (((src >> 16) & 0xFF) * a + ((bg >> 16) & 0xFF) * inv_a) / 255;
                let g = (((src >> 8) & 0xFF) * a + ((bg >> 8) & 0xFF) * inv_a) / 255;
                let b = ((src & 0xFF) * a + (bg & 0xFF) * inv_a) / 255;
                buf[dst_idx] = (r << 16) | (g << 8) | b;
            }
        }
    }
}

// --- search / filter ---

fn file_contains_text(path: &Path, query_lower: &str) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    content.to_lowercase().contains(query_lower)
}

fn matches_quick(t: &TranscriptEntry, q: &str) -> bool {
    t.preview.to_lowercase().contains(q)
        || t.project.to_lowercase().contains(q)
        || t.uuid.to_lowercase().contains(q)
}

fn filter_transcripts_quick(
    all: &[TranscriptEntry],
    query: &str,
    favs_only: bool,
    chats: &HashMap<String, ChatMeta>,
) -> Vec<TranscriptEntry> {
    let q = query.to_lowercase();
    all.iter()
        .filter(|t| {
            if favs_only && !chats.get(&t.uuid).map_or(false, |m| m.starred) {
                return false;
            }
            if !q.is_empty() {
                // also search by chat name
                if let Some(meta) = chats.get(&t.uuid) {
                    if let Some(ref name) = meta.name {
                        if name.to_lowercase().contains(&q) {
                            return true;
                        }
                    }
                }
                return matches_quick(t, &q);
            }
            true
        })
        .cloned()
        .collect()
}

fn filter_transcripts_deep(
    all: &[TranscriptEntry],
    query: &str,
    favs_only: bool,
    chats: &HashMap<String, ChatMeta>,
) -> Vec<TranscriptEntry> {
    let q = query.to_lowercase();
    all.iter()
        .filter(|t| {
            if favs_only && !chats.get(&t.uuid).map_or(false, |m| m.starred) {
                return false;
            }
            if !q.is_empty() {
                if let Some(meta) = chats.get(&t.uuid) {
                    if let Some(ref name) = meta.name {
                        if name.to_lowercase().contains(&q) {
                            return true;
                        }
                    }
                }
                if matches_quick(t, &q) {
                    return true;
                }
                return file_contains_text(&t.path, &q);
            }
            true
        })
        .cloned()
        .collect()
}

fn key_to_char(key: Key, shift: bool) -> Option<char> {
    let c = match key {
        Key::A => 'a', Key::B => 'b', Key::C => 'c', Key::D => 'd',
        Key::E => 'e', Key::F => 'f', Key::G => 'g', Key::H => 'h',
        Key::I => 'i', Key::J => 'j', Key::K => 'k', Key::L => 'l',
        Key::M => 'm', Key::N => 'n', Key::O => 'o', Key::P => 'p',
        Key::Q => 'q', Key::R => 'r', Key::S => 's', Key::T => 't',
        Key::U => 'u', Key::V => 'v', Key::W => 'w', Key::X => 'x',
        Key::Y => 'y', Key::Z => 'z',
        Key::Key0 | Key::NumPad0 => '0', Key::Key1 | Key::NumPad1 => '1',
        Key::Key2 | Key::NumPad2 => '2', Key::Key3 | Key::NumPad3 => '3',
        Key::Key4 | Key::NumPad4 => '4', Key::Key5 | Key::NumPad5 => '5',
        Key::Key6 | Key::NumPad6 => '6', Key::Key7 | Key::NumPad7 => '7',
        Key::Key8 | Key::NumPad8 => '8', Key::Key9 | Key::NumPad9 => '9',
        Key::Space => ' ',
        Key::Minus => if shift { '_' } else { '-' },
        Key::Period => '.', Key::Slash => '/',
        _ => return None,
    };
    if shift && c.is_ascii_alphabetic() {
        Some(c.to_ascii_uppercase())
    } else {
        Some(c)
    }
}

// --- selection state ---

#[derive(Clone, Default)]
struct Selection {
    active: bool,
    anchor_line: usize,
    anchor_col: usize,
    cursor_line: usize,
    cursor_col: usize,
}

impl Selection {
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let a = (self.anchor_line, self.anchor_col);
        let b = (self.cursor_line, self.cursor_col);
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    fn extract_text(&self, lines: &[String]) -> String {
        if !self.active {
            return String::new();
        }
        let ((sl, sc), (el, ec)) = self.ordered();
        let mut out = String::new();
        for i in sl..=el {
            let Some(line) = lines.get(i) else { continue };
            let chars: Vec<char> = line.chars().collect();
            let start = if i == sl { sc.min(chars.len()) } else { 0 };
            let end = if i == el { ec.min(chars.len()) } else { chars.len() };
            if start <= end {
                out.extend(&chars[start..end]);
            }
            if i < el {
                out.push('\n');
            }
        }
        out
    }

    fn is_click(&self) -> bool {
        self.anchor_line == self.cursor_line && self.anchor_col == self.cursor_col
    }
}

// --- file mtime helper ---

fn get_file_mtime(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// --- app state ---

enum AppState {
    List {
        transcripts: Vec<TranscriptEntry>,
        filtered: Vec<TranscriptEntry>,
        selected: usize,
        scroll: i32,
        searching: bool,
        search_term: String,
        favs_only: bool,
        last_scan: Instant,
        renaming: bool,
        rename_buf: String,
    },
    View {
        path: PathBuf,
        source_format: SourceFormat,
        groups: Vec<MessageGroup>,
        expanded: HashSet<usize>,
        lines: Vec<String>,
        line_meta: Vec<LineMeta>,
        in_code: Vec<bool>,
        last_wrap_w: usize,
        scroll: i32,
        sel: Selection,
        file_mtime: u64,
        last_check: Instant,
        remote: Option<RemoteOrigin>,
        chat_uuid: String,
    },
}

const HEADER_H: i32 = 28;

/// Rebuild flattened lines from groups + expansion state
fn rebuild_view(
    groups: &[MessageGroup],
    expanded: &HashSet<usize>,
    wrap_w: usize,
) -> (Vec<String>, Vec<LineMeta>, Vec<bool>) {
    let (lines, meta) = flatten_groups(groups, expanded, wrap_w);
    let in_code = compute_code_block_state(&lines);
    (lines, meta, in_code)
}

// --- LAN peer sync functions ---
// Silent until a join code is set; see join.rs for the wire format.

fn get_hostname() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or(env::VarError::NotPresent)
        })
        .unwrap_or_else(|_| "unknown".into())
}

fn current_key(key: &SharedKey) -> Option<join::GroupKey> {
    key.lock().ok().and_then(|k| k.clone())
}

/// Bind and spawn the sync threads. Returns the TCP port, or 0 if binding failed.
fn start_sync(
    sources: Vec<SourceConfig>,
    key: SharedKey,
    peer_state: Arc<Mutex<PeerState>>,
    hostname: String,
) -> u16 {
    let Ok(listener) = TcpListener::bind("0.0.0.0:0") else { return 0 };
    let Ok(port) = listener.local_addr().map(|a| a.port()) else { return 0 };
    let instance = join::instance_id();
    start_tcp_server(listener, sources, Arc::clone(&key));
    start_beacon_sender(hostname, port, instance.clone(), Arc::clone(&key));
    start_beacon_listener(peer_state, instance, key);
    port
}

/// Set or clear the join code, drop peers from the old group, persist.
fn apply_join_code(
    code: Option<String>,
    cfg: &mut AppConfig,
    key: &SharedKey,
    peer_state: &Arc<Mutex<PeerState>>,
) {
    if let Ok(mut k) = key.lock() {
        *k = code.as_deref().and_then(join::GroupKey::from_code);
    }
    if let Ok(mut ps) = peer_state.lock() {
        ps.peers.clear();
        ps.remote_entries.clear();
        ps.dirty = true;
    }
    cfg.join_code = code;
    save_config(cfg);
}

fn start_tcp_server(listener: TcpListener, sources: Vec<SourceConfig>, key: SharedKey) {
    let active = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Some(k) = current_key(&key) else { continue };
            if active.load(Ordering::SeqCst) >= MAX_PEER_CONNS {
                continue;
            }
            active.fetch_add(1, Ordering::SeqCst);
            let srcs = sources.clone();
            let active = Arc::clone(&active);
            thread::spawn(move || {
                handle_tcp_client(stream, &srcs, &k);
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
}

fn handle_tcp_client(stream: TcpStream, sources: &[SourceConfig], key: &join::GroupKey) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    let Some(mut ch) = join::Channel::accept(stream, key) else { return };
    let Some(req) = ch.recv(join::MAX_REQUEST) else { return };
    let req = String::from_utf8_lossy(&req);
    let line = req.trim();
    if line == "LIST" {
        let transcripts = get_all_transcripts(sources);
        let chats = load_chats();
        let arr: Vec<Value> = transcripts.iter().map(|t| {
            let name = chats.get(&t.uuid).and_then(|m| m.name.as_ref()).cloned();
            let mut obj = serde_json::json!({
                "uuid": t.uuid,
                "project": t.project,
                "mtime": t.mtime_secs,
                "preview": t.preview,
                "last_preview": t.last_preview,
                "timestamp": t.timestamp,
            });
            if let Some(n) = name {
                obj["name"] = serde_json::Value::String(n);
            }
            obj
        }).collect();
        let json = serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into());
        let _ = ch.send(json.as_bytes());
    } else if let Some(uuid) = line.strip_prefix("GET ") {
        let uuid = uuid.trim();
        // find the transcript by uuid
        let transcripts = get_all_transcripts(sources);
        if let Some(entry) = transcripts.iter().find(|t| t.uuid == uuid) {
            let msgs = load_messages(&entry.path, &entry.source_format);
            let arr: Vec<Value> = msgs.iter().map(|m| {
                serde_json::json!({
                    "role": m.role,
                    "text": m.text,
                    "timestamp": m.timestamp,
                })
            }).collect();
            let json = serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into());
            let _ = ch.send(json.as_bytes());
        } else {
            let _ = ch.send(b"[]");
        }
    }
}

fn get_broadcast_addrs() -> Vec<SocketAddr> {
    let mut addrs = Vec::new();
    // Try to get local IP by connecting to a public address (doesn't actually send data)
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        // Connect to a public IP just to determine our local interface address
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(local) = s.local_addr() {
                if let std::net::IpAddr::V4(ip) = local.ip() {
                    let octets = ip.octets();
                    // Assume /24 subnet — broadcast is x.x.x.255
                    let bcast = std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], 255);
                    addrs.push(SocketAddr::new(std::net::IpAddr::V4(bcast), LAN_PORT));
                }
            }
        }
    }
    // Always also try the global broadcast as fallback
    addrs.push(format!("255.255.255.255:{}", LAN_PORT).parse().unwrap());
    addrs.dedup();
    addrs
}

fn start_beacon_sender(hostname: String, tcp_port: u16, instance: String, key: SharedKey) {
    thread::spawn(move || {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
        let _ = sock.set_broadcast(true);
        let beacon = serde_json::json!({ "h": hostname, "p": tcp_port, "i": instance }).to_string();
        loop {
            if let Some(k) = current_key(&key) {
                let pkt = k.seal_beacon(beacon.as_bytes());
                for dest in &get_broadcast_addrs() {
                    let _ = sock.send_to(&pkt, dest);
                }
            }
            thread::sleep(std::time::Duration::from_secs(3));
        }
    });
}

fn refresh_remote_entries(peer_state: &Arc<Mutex<PeerState>>, key: &SharedKey) {
    let Some(k) = current_key(key) else { return };
    let addrs: Vec<(SocketAddr, String)> = {
        let Ok(ps) = peer_state.lock() else { return };
        ps.peers.iter().map(|(addr, info)| (*addr, info.hostname.clone())).collect()
    };
    let mut all_remote = Vec::new();
    for (addr, hostname) in &addrs {
        let entries = fetch_peer_list(*addr, hostname, &k);
        all_remote.extend(entries);
    }
    if let Ok(mut ps) = peer_state.lock() {
        ps.remote_entries = all_remote;
        ps.dirty = true;
    }
}

fn start_beacon_listener(peer_state: Arc<Mutex<PeerState>>, instance: String, key: SharedKey) {
    thread::spawn(move || {
        let Ok(sock) = UdpSocket::bind(format!("0.0.0.0:{}", LAN_PORT)) else {
            eprintln!("[chat-daddy] failed to bind UDP port {}", LAN_PORT);
            return;
        };
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let mut buf = [0u8; 1024];
        let mut last_refresh = Instant::now();
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, src_addr)) => {
                    // only beacons sealed with our code get through
                    let Some(k) = current_key(&key) else { continue };
                    let Some(plain) = k.open_beacon(&buf[..n]) else { continue };
                    let Ok(val) = serde_json::from_slice::<Value>(&plain) else { continue };
                    let Some(h) = val.get("h").and_then(|v| v.as_str()) else { continue };
                    let Some(p) = val.get("p").and_then(|v| v.as_u64()) else { continue };
                    if val.get("i").and_then(|v| v.as_str()) == Some(instance.as_str()) {
                        continue; // skip self
                    }
                    let tcp_port = p as u16;
                    let peer_tcp_addr: SocketAddr = SocketAddr::new(src_addr.ip(), tcp_port);
                    let is_new;
                    if let Ok(mut ps) = peer_state.lock() {
                        is_new = !ps.peers.contains_key(&peer_tcp_addr);
                        ps.peers.insert(peer_tcp_addr, PeerInfo {
                            hostname: h.to_string(),
                            tcp_port,
                            last_seen: Instant::now(),
                        });
                        ps.peers.retain(|_, info| info.last_seen.elapsed().as_secs() < 9);
                    } else {
                        is_new = false;
                    }
                    // immediate fetch on new peer, periodic refresh otherwise
                    if is_new || last_refresh.elapsed().as_secs() >= 5 {
                        refresh_remote_entries(&peer_state, &key);
                        last_refresh = Instant::now();
                    }
                }
                Err(_) => {
                    // timeout — evict stale peers
                    if let Ok(mut ps) = peer_state.lock() {
                        ps.peers.retain(|_, info| info.last_seen.elapsed().as_secs() < 9);
                    }
                }
            }
            // always refresh periodically regardless of beacon activity
            if last_refresh.elapsed().as_secs() >= 5 {
                refresh_remote_entries(&peer_state, &key);
                last_refresh = Instant::now();
            }
        }
    });
}

/// One sealed request/response round trip with a peer.
fn peer_request(addr: SocketAddr, key: &join::GroupKey, req: &str) -> Option<Vec<Value>> {
    let stream = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let mut ch = join::Channel::connect(stream, key)?;
    ch.send(req.as_bytes())?;
    let resp = ch.recv(join::MAX_RESPONSE)?;
    serde_json::from_slice(&resp).ok()
}

fn fetch_peer_list(addr: SocketAddr, hostname: &str, key: &join::GroupKey) -> Vec<TranscriptEntry> {
    let Some(arr) = peer_request(addr, key, "LIST") else {
        return vec![];
    };
    arr.iter().filter_map(|v| {
        let uuid = v.get("uuid")?.as_str()?.to_string();
        let project = v.get("project")?.as_str()?.to_string();
        let mtime = v.get("mtime")?.as_u64().unwrap_or(0);
        let preview = v.get("preview").and_then(|p| p.as_str()).unwrap_or("").to_string();
        let last_preview = v.get("last_preview").and_then(|p| p.as_str()).unwrap_or("").to_string();
        let timestamp = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let remote_name = v.get("name").and_then(|n| n.as_str()).map(|s| s.to_string());
        Some(TranscriptEntry {
            path: PathBuf::new(),
            uuid,
            project,
            source_format: SourceFormat::Generic,
            mtime_secs: mtime,
            preview,
            last_preview,
            timestamp,
            remote: Some(RemoteOrigin {
                hostname: hostname.to_string(),
                tcp_addr: addr,
                key: key.clone(),
            }),
            remote_name,
        })
    }).collect()
}

fn fetch_remote_messages(origin: &RemoteOrigin, uuid: &str) -> Vec<MessageLine> {
    let Some(arr) = peer_request(origin.tcp_addr, &origin.key, &format!("GET {}", uuid)) else {
        return vec![];
    };
    arr.iter().filter_map(|v| {
        let role = v.get("role")?.as_str()?.to_string();
        let text = v.get("text")?.as_str()?.to_string();
        let timestamp = v.get("timestamp").and_then(|t| t.as_u64()).unwrap_or(0);
        Some(MessageLine { role, text, timestamp })
    }).collect()
}

fn main() {
    set_macos_dock_icon();
    let mut cfg = load_config();
    let sources = cfg.sources.clone();
    let mut theme = cfg.theme.clone();
    let font_name = cfg.font.clone();
    let font_weight = cfg.font_weight;
    let snapshot = env::var("CHAT_DADDY_SNAP").is_ok();

    // Restore saved window state
    let window_state_path = chat_daddy_dir().join("window.json");
    let (init_w, init_h, saved_pos) = if let Ok(data) = fs::read_to_string(&window_state_path) {
        if let Ok(j) = serde_json::from_str::<Value>(&data) {
            let w = j.get("w").and_then(|v| v.as_u64()).unwrap_or(WIN_W as u64) as usize;
            let h = j.get("h").and_then(|v| v.as_u64()).unwrap_or(WIN_H as u64) as usize;
            let pos = match (j.get("x").and_then(|v| v.as_i64()), j.get("y").and_then(|v| v.as_i64())) {
                (Some(x), Some(y)) => Some((x as isize, y as isize)),
                _ => None,
            };
            (w.max(400), h.max(300), pos)
        } else {
            (WIN_W, WIN_H, None)
        }
    } else {
        (WIN_W, WIN_H, None)
    };

    let mut window = Window::new(
        "chat-daddy",
        init_w,
        init_h,
        WindowOptions {
            resize: true,
            ..WindowOptions::default()
        },
    )
    .expect("window");
    window.set_target_fps(60);

    if let Some((x, y)) = saved_pos {
        window.set_position(x, y);
    }

    // Load icon pixels for help overlay (96px circle-cropped)
    let help_icon: Option<(u32, u32, Vec<u32>)> = load_icon_pixels(96);

    // Kick off transcript loading in background
    let sources_clone = sources.clone();
    let load_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load_done2 = load_done.clone();
    let load_result: Arc<Mutex<Option<(Vec<TranscriptEntry>, HashMap<String, ChatMeta>)>>> = Arc::new(Mutex::new(None));
    let load_result2 = load_result.clone();
    thread::spawn(move || {
        let transcripts = get_all_transcripts(&sources_clone);
        let chats = load_chats();
        *load_result2.lock().unwrap() = Some((transcripts, chats));
        load_done2.store(true, std::sync::atomic::Ordering::Release);
    });

    // Pulsing icon splash while loading
    {
        let splash_icon = load_icon_pixels(128);
        if let Some((iw, ih, ref icon_px)) = splash_icon {
            let bg_color = rgb(0x0d, 0x11, 0x17);
            let mut buf = vec![bg_color; init_w * init_h];
            let ox = (init_w as u32 - iw) / 2;
            let oy = (init_h as u32 - ih) / 2;
            // Pre-blit static icon onto buffer
            for iy in 0..ih {
                for ix in 0..iw {
                    let src = icon_px[(iy * iw + ix) as usize];
                    let a = (src >> 24) & 0xFF;
                    if a == 0 { continue; }
                    let bx = (ox + ix) as usize;
                    let by = (oy + iy) as usize;
                    if bx < init_w && by < init_h {
                        buf[by * init_w + bx] = src & 0x00FFFFFF;
                    }
                }
            }
            let static_buf = buf.clone();
            let mut frame = 0u32;
            while !load_done.load(std::sync::atomic::Ordering::Acquire) && window.is_open() {
                // Pulse: fade icon brightness using a sine wave
                let pulse = ((frame as f64 * 0.15).sin() * 0.3 + 0.7) as f32; // 0.4 to 1.0
                for i in 0..buf.len() {
                    let s = static_buf[i];
                    if s == bg_color {
                        buf[i] = bg_color;
                    } else {
                        let r = ((((s >> 16) & 0xFF) as f32) * pulse) as u32;
                        let g = ((((s >> 8) & 0xFF) as f32) * pulse) as u32;
                        let b = (((s & 0xFF) as f32) * pulse) as u32;
                        buf[i] = (r.min(255) << 16) | (g.min(255) << 8) | b.min(255);
                    }
                }
                let _ = window.update_with_buffer(&buf, init_w, init_h);
                std::thread::sleep(std::time::Duration::from_millis(16));
                frame += 1;
            }
        } else {
            while !load_done.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        }
    }

    let (all_transcripts, mut chats) = load_result.lock().unwrap().take().unwrap();
    let mut state = AppState::List {
        filtered: all_transcripts.clone(),
        transcripts: all_transcripts,
        selected: 0,
        scroll: 0,
        searching: false,
        search_term: String::new(),
        favs_only: false,
        last_scan: Instant::now(),
        renaming: false,
        rename_buf: String::new(),
    };

    let mut buffer = vec![rgb(theme.bg.0, theme.bg.1, theme.bg.2); init_w * init_h];
    let mut buf_w: usize = init_w;
    let mut buf_h: usize = init_h;
    let mut transition: Option<AppState> = None;
    let mut size_idx = DEFAULT_SIZE_IDX;
    let mut atlas = FontAtlas::with_font(FONT_SIZES[size_idx], &font_name, font_weight);
    let mut clipboard = Clipboard::new().ok();
    let mut mouse_dragging = false;
    let mut needs_rebuild = false;
    let mut show_help = false;
    let mut multi_selected: HashSet<String> = HashSet::new(); // UUIDs of multi-selected chats
    let llm_endpoint = cfg.llm_endpoint.clone();
    let mut llm_healthy = false;
    let mut llm_last_check = Instant::now() - std::time::Duration::from_secs(60); // force first check
    let mut auto_name_last = Instant::now() - std::time::Duration::from_secs(60);
    let mut last_window_pos: (isize, isize) = (0, 0);
    let mut last_window_size: (usize, usize) = (WIN_W, WIN_H);
    let mut saved_list_idx: usize = 0;
    let mut saved_list_scroll: i32 = 0;
    let mut quit_requested = false;
    let mut available_themes = load_available_themes();
    let mut theme_selected: usize = 0;
    let mut flash: Option<(Instant, String)> = None;

    // --- LAN peer sync setup ---
    // nothing binds until a join code is set
    let my_hostname = get_hostname();
    let sync_key: SharedKey = Arc::new(Mutex::new(
        cfg.join_code.as_deref().and_then(join::GroupKey::from_code),
    ));
    let peer_state = Arc::new(Mutex::new(PeerState {
        peers: HashMap::new(),
        remote_entries: Vec::new(),
        dirty: false,
    }));
    let mut my_tcp_port: u16 = 0;
    if cfg.join_code.is_some() {
        my_tcp_port = start_sync(sources.clone(), Arc::clone(&sync_key), Arc::clone(&peer_state), my_hostname.clone());
    }
    let mut joining = false;
    let mut join_buf = String::new();

    let mut last_merged: Vec<TranscriptEntry> = Vec::new();

    // theme-derived color aliases (lowercase, used by render code)
    let mut c_bg = theme.bg;
    let mut c_dim = theme.dim;
    let mut c_user = theme.user;
    let mut c_asst = theme.asst;
    let mut c_text = theme.text;
    let mut c_sel = theme.sel;
    let mut c_sep = theme.sep;
    let mut c_header_bg = theme.header_bg;
    let mut c_accent = theme.accent;
    let mut c_search_bg = theme.search_bg;
    let mut _c_timestamp = theme.timestamp;
    let mut c_select_bg = theme.select_bg;
    let mut c_code = theme.code;
    let mut c_code_bg = theme.code_bg;
    let mut _c_bold = theme.bold;
    let mut c_toggle = theme.toggle;
    let mut c_heading = theme.heading;
    let mut c_msg_time = theme.msg_time;
    let mut c_remote_host = theme.remote_host;
    let mut c_platform = theme.platform;
    let mut c_datetime = theme.datetime;
    let mut needs_theme_reload = false;

    while window.is_open() && !quit_requested {
        if needs_theme_reload {
            theme = cfg.theme.clone();
            c_bg = theme.bg;
            c_dim = theme.dim;
            c_user = theme.user;
            c_asst = theme.asst;
            c_text = theme.text;
            c_sel = theme.sel;
            c_sep = theme.sep;
            c_header_bg = theme.header_bg;
            c_accent = theme.accent;
            c_search_bg = theme.search_bg;
            _c_timestamp = theme.timestamp;
            c_select_bg = theme.select_bg;
            c_code = theme.code;
            c_code_bg = theme.code_bg;
            _c_bold = theme.bold;
            c_toggle = theme.toggle;
            c_heading = theme.heading;
            c_msg_time = theme.msg_time;
            c_remote_host = theme.remote_host;
            c_platform = theme.platform;
            c_datetime = theme.datetime;
            needs_theme_reload = false;
        }
        if let Some(s) = transition.take() {
            state = s;
        }

        // Prefer native NSView size (handles tile/maximize on macOS); fall back to minifb
        let (win_w, win_h) = native_view_size(&window)
            .unwrap_or_else(|| window.get_size());
        if win_w != buf_w || win_h != buf_h {
            buf_w = win_w.max(1);
            buf_h = win_h.max(1);
            buffer = vec![rgb(c_bg.0, c_bg.1, c_bg.2); buf_w * buf_h];
        }
        let lh = atlas.line_height;
        let advance = atlas.advance;
        let content_top = HEADER_H + PAD;

        for p in &mut buffer {
            *p = rgb(c_bg.0, c_bg.1, c_bg.2);
        }

        // --- periodic refresh ---
        match &mut state {
            AppState::List {
                transcripts,
                filtered,
                search_term,
                favs_only,
                last_scan,
                selected,
                scroll,
                ..
            } => {
                if last_scan.elapsed().as_secs() >= 2 {
                    *last_scan = Instant::now();
                    let mut fresh = get_all_transcripts(&sources);

                    // merge remote peers
                    if let Ok(mut ps) = peer_state.try_lock() {
                        ps.dirty = false;
                        let remote = ps.remote_entries.clone();
                        fresh.extend(remote);
                        fresh.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
                    }

                    last_merged = fresh.clone();

                    if fresh.len() != transcripts.len()
                        || fresh
                            .first()
                            .map(|t| t.mtime_secs)
                            != transcripts.first().map(|t| t.mtime_secs)
                        || fresh.iter().any(|t| t.remote.is_some()) != transcripts.iter().any(|t| t.remote.is_some())
                    {
                        *transcripts = fresh.clone();
                        *filtered = filter_transcripts_quick(
                            &fresh,
                            search_term,
                            *favs_only,
                            &chats,
                        );
                        *selected = (*selected).min(filtered.len().saturating_sub(1));
                        let visible_lines = ((win_h as i32) - content_top - PAD) / lh;
                        let total_lines = list_total_lines(filtered.len(), *selected);
                        let sel_y = list_item_y_lines(*selected, *selected, filtered.len());
                        *scroll = (sel_y - visible_lines / 2)
                            .max(0)
                            .min((total_lines - visible_lines).max(0));
                    }
                }
            }
            AppState::View {
                path,
                source_format,
                groups,
                expanded,
                lines,
                line_meta,
                in_code,
                last_wrap_w,
                scroll,
                file_mtime,
                last_check,
                remote,
                chat_uuid,
                sel: _,
            } => {
                if last_check.elapsed().as_secs() >= 1 {
                    *last_check = Instant::now();
                    match remote {
                        Some(origin) => {
                            // re-fetch from peer
                            let msgs = fetch_remote_messages(origin, chat_uuid);
                            if !msgs.is_empty() {
                                let old_len = lines.len();
                                *groups = group_messages(&msgs);
                                let wrap_w = chars_per_line(win_w, advance);
                                let (nl, nm, nc) = rebuild_view(groups, expanded, wrap_w);
                                *lines = nl;
                                *line_meta = nm;
                                *in_code = nc;
                                *last_wrap_w = wrap_w;
                                // only scroll to bottom if new content arrived
                                if lines.len() != old_len {
                                    let visible = ((win_h as i32) - content_top - PAD) / lh;
                                    *scroll = (lines.len() as i32 - visible).max(0);
                                }
                            }
                        }
                        None => {
                            let new_mtime = get_file_mtime(path);
                            if new_mtime != *file_mtime {
                                *file_mtime = new_mtime;
                                let visible = ((win_h as i32) - content_top - PAD) / lh;
                                let was_at_bottom = *scroll >= (lines.len() as i32 - visible).max(0);
                                let msgs = load_messages(path, source_format);
                                *groups = group_messages(&msgs);
                                let wrap_w = chars_per_line(win_w, advance);
                                let (nl, nm, nc) = rebuild_view(groups, expanded, wrap_w);
                                *lines = nl;
                                *line_meta = nm;
                                *in_code = nc;
                                *last_wrap_w = wrap_w;
                                // only auto-scroll to bottom if user was already there
                                if was_at_bottom {
                                    *scroll = (lines.len() as i32 - visible).max(0);
                                }
                            }
                        }
                    }
                }
            }
        }

        // --- LLM auto-naming scan (list mode, every 10s, one chat per tick) ---
        if let AppState::List { transcripts, .. } = &state {
            // check LLM health every 30s
            if llm_last_check.elapsed().as_secs() >= 30 {
                llm_last_check = Instant::now();
                llm_healthy = llm_is_healthy(&llm_endpoint);
            }
            if llm_healthy && auto_name_last.elapsed().as_secs() >= 10 {
                auto_name_last = Instant::now();
                // find first unnamed chat
                let unnamed: Option<TranscriptEntry> = transcripts.iter().find(|t| {
                    !chats.get(&t.uuid).and_then(|m| m.name.as_ref()).is_some()
                }).cloned();
                if let Some(entry) = unnamed {
                    let msgs = load_messages(&entry.path, &entry.source_format);
                    if !msgs.is_empty() {
                        if let Some(name) = llm_auto_name(&llm_endpoint, &msgs) {
                            let meta = chats.entry(entry.uuid).or_insert_with(ChatMeta::default);
                            meta.name = Some(name);
                            meta.auto_named = true;
                            save_chats(&chats);
                        }
                    }
                }
            }
        }

        // --- mouse: selection + click-to-expand in view mode ---
        if let AppState::View {
            lines,
            line_meta,
            scroll,
            sel,
            groups: _,
            expanded,
            in_code: _,
            last_wrap_w: _,
            ..
        } = &mut state
        {
            if let Some((mx, my)) = window.get_mouse_pos(MouseMode::Clamp) {
                let line_idx = ((my as i32 - content_top + *scroll * lh) / lh).max(0) as usize;
                let col_idx = ((mx as i32 - PAD) / advance).max(0) as usize;

                if window.get_mouse_down(MouseButton::Left) {
                    if !mouse_dragging {
                        mouse_dragging = true;
                        sel.active = true;
                        sel.anchor_line = line_idx;
                        sel.anchor_col = col_idx;
                        sel.cursor_line = line_idx;
                        sel.cursor_col = col_idx;
                    } else {
                        sel.cursor_line = line_idx.min(lines.len().saturating_sub(1));
                        sel.cursor_col = col_idx;
                    }
                } else if mouse_dragging {
                    mouse_dragging = false;

                    if sel.is_click() {
                        // check for toggle click
                        let click_line = sel.anchor_line;
                        if let Some(LineMeta::Toggle(gi)) = line_meta.get(click_line) {
                            let gi = *gi;
                            if expanded.contains(&gi) {
                                expanded.remove(&gi);
                            } else {
                                expanded.insert(gi);
                            }
                            needs_rebuild = true;
                        }
                        sel.active = false;
                    } else {
                        // drag completed — copy selection
                        let text = sel.extract_text(lines);
                        if !text.is_empty() {
                            if let Some(ref mut cb) = clipboard {
                                let _ = cb.set_text(&text);
                            }
                        }
                    }
                }
            }
        } else {
            mouse_dragging = false;
        }

        // rebuild lines if toggle was clicked
        if needs_rebuild {
            needs_rebuild = false;
            if let AppState::View {
                groups,
                expanded,
                lines,
                line_meta,
                in_code,
                last_wrap_w,
                ..
            } = &mut state
            {
                let wrap_w = chars_per_line(win_w, advance);
                let (nl, nm, nc) = rebuild_view(groups, expanded, wrap_w);
                *lines = nl;
                *line_meta = nm;
                *in_code = nc;
                *last_wrap_w = wrap_w;
            }
        }

        // --- mouse scroll ---
        if let Some((_sx, sy)) = window.get_scroll_wheel() {
            let scroll_lines = (sy / 3.0) as i32;
            match &mut state {
                AppState::List {
                    filtered,
                    selected,
                    scroll,
                    ..
                } => {
                    if scroll_lines > 0 {
                        *selected = (*selected as i32 - scroll_lines).max(0) as usize;
                    } else {
                        *selected = (*selected as i32 - scroll_lines)
                            .min(filtered.len() as i32 - 1)
                            .max(0) as usize;
                    }
                    let visible_lines = ((win_h as i32) - content_top - PAD) / lh;
                    let total_lines = list_total_lines(filtered.len(), *selected);
                    let sel_y = list_item_y_lines(*selected, *selected, filtered.len());
                    *scroll = (sel_y - visible_lines / 2)
                        .max(0)
                        .min((total_lines - visible_lines).max(0));
                }
                AppState::View { lines, scroll, .. } => {
                    let delta = -scroll_lines * 3;
                    let visible = ((win_h as i32) - content_top - PAD) / lh;
                    let max_scroll = (lines.len() as i32 - visible).max(0);
                    *scroll = (*scroll + delta).max(0).min(max_scroll);
                }
            }
        }

        // --- keyboard ---
        let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
        let shift = window.is_key_down(Key::LeftShift) || window.is_key_down(Key::RightShift);

        for key in window.get_keys_pressed(KeyRepeat::Yes) {
            if joining {
                match key {
                    Key::Escape => {
                        joining = false;
                        join_buf.clear();
                    }
                    Key::Enter => {
                        let input = join_buf.trim().to_string();
                        let code = if input.is_empty() { None } else { join::normalize(&input) };
                        if !input.is_empty() && code.is_none() {
                            flash = Some((Instant::now(), "bad code".into()));
                            continue;
                        }
                        apply_join_code(code, &mut cfg, &sync_key, &peer_state);
                        if cfg.join_code.is_some() && my_tcp_port == 0 {
                            my_tcp_port = start_sync(sources.clone(), Arc::clone(&sync_key), Arc::clone(&peer_state), my_hostname.clone());
                        }
                        let label = if cfg.join_code.is_some() { "joined" } else { "sync off" };
                        flash = Some((Instant::now(), label.into()));
                        joining = false;
                        join_buf.clear();
                    }
                    Key::Backspace => {
                        join_buf.pop();
                    }
                    Key::V if ctrl => {
                        if let Some(text) = clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                            join_buf.push_str(text.trim());
                        }
                    }
                    other => {
                        if let Some(ch) = key_to_char(other, shift) {
                            join_buf.push(ch);
                        }
                    }
                }
                continue;
            }
            if ctrl {
                match key {
                    Key::Equal | Key::NumPadPlus => {
                        size_idx = (size_idx + 1).min(FONT_SIZES.len() - 1);
                        atlas = FontAtlas::with_font(FONT_SIZES[size_idx], &font_name, font_weight);
                        if let AppState::View { last_wrap_w, .. } = &mut state {
                            *last_wrap_w = 0; // force re-wrap
                        }
                        continue;
                    }
                    Key::Minus | Key::NumPadMinus => {
                        size_idx = size_idx.saturating_sub(1);
                        atlas = FontAtlas::with_font(FONT_SIZES[size_idx], &font_name, font_weight);
                        if let AppState::View { last_wrap_w, .. } = &mut state {
                            *last_wrap_w = 0;
                        }
                        continue;
                    }
                    Key::C => {
                        if let AppState::View { lines, sel, .. } = &state {
                            let text = if sel.active {
                                sel.extract_text(lines)
                            } else {
                                lines.join("\n")
                            };
                            if let Some(ref mut cb) = clipboard {
                                let _ = cb.set_text(&text);
                            }
                        }
                        continue;
                    }
                    Key::A => {
                        if let AppState::View { lines, sel, .. } = &mut state {
                            sel.active = true;
                            sel.anchor_line = 0;
                            sel.anchor_col = 0;
                            sel.cursor_line = lines.len().saturating_sub(1);
                            sel.cursor_col = lines.last().map_or(0, |l| l.chars().count());
                        }
                        continue;
                    }
                    Key::E => {
                        // export multi-selected chats (or current chat in view)
                        if let AppState::List { filtered, .. } = &state {
                            if !multi_selected.is_empty() {
                                let mut export: Vec<Value> = Vec::new();
                                for entry in filtered.iter() {
                                    if !multi_selected.contains(&entry.uuid) {
                                        continue;
                                    }
                                    let msgs = match &entry.remote {
                                        Some(origin) => fetch_remote_messages(origin, &entry.uuid),
                                        None => load_messages(&entry.path, &entry.source_format),
                                    };
                                    let name = chats.get(&entry.uuid).and_then(|m| m.name.as_ref()).cloned();
                                    for m in &msgs {
                                        export.push(serde_json::json!({
                                            "timestamp": m.timestamp,
                                            "platform": entry.project,
                                            "role": m.role,
                                            "text": m.text,
                                            "meta": {
                                                "uuid": entry.uuid,
                                                "name": name,
                                            }
                                        }));
                                    }
                                }
                                export.sort_by_key(|v| v.get("timestamp").and_then(|t| t.as_u64()).unwrap_or(0));
                                if let Ok(json) = serde_json::to_string_pretty(&export) {
                                    // save to ~/.chat-daddy/export.json and copy to clipboard
                                    let export_path = chat_daddy_dir().join("export.json");
                                    let _ = fs::write(&export_path, &json);
                                    if let Some(ref mut cb) = clipboard {
                                        let _ = cb.set_text(&json);
                                    }
                                }
                                multi_selected.clear();
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
            }

            match &mut state {
                AppState::List {
                    transcripts,
                    filtered,
                    selected,
                    scroll,
                    searching,
                    search_term,
                    favs_only,
                    renaming,
                    rename_buf,
                    ..
                } => {
                    if *renaming {
                        match key {
                            Key::Escape => {
                                *renaming = false;
                                rename_buf.clear();
                            }
                            Key::Enter => {
                                if let Some(entry) = filtered.get(*selected) {
                                    let uuid = entry.uuid.clone();
                                    let meta = chats.entry(uuid).or_insert_with(ChatMeta::default);
                                    if rename_buf.trim().is_empty() {
                                        meta.name = None;
                                        meta.auto_named = false;
                                    } else {
                                        meta.name = Some(rename_buf.trim().to_string());
                                        meta.auto_named = false;
                                    }
                                    save_chats(&chats);
                                }
                                *renaming = false;
                                rename_buf.clear();
                            }
                            Key::Backspace => {
                                rename_buf.pop();
                            }
                            other => {
                                if let Some(ch) = key_to_char(other, shift) {
                                    rename_buf.push(ch);
                                }
                            }
                        }
                    } else if *searching {
                        match key {
                            Key::Escape => {
                                *searching = false;
                                search_term.clear();
                                *filtered = filter_transcripts_quick(
                                    transcripts,
                                    "",
                                    *favs_only,
                                    &chats,
                                );
                                *selected = 0;
                                *scroll = 0;
                            }
                            Key::Enter => {
                                *searching = false;
                                *filtered = filter_transcripts_deep(
                                    transcripts,
                                    search_term,
                                    *favs_only,
                                    &chats,
                                );
                                *selected = 0;
                                *scroll = 0;
                            }
                            Key::Backspace => {
                                search_term.pop();
                                *filtered = filter_transcripts_quick(
                                    transcripts,
                                    search_term,
                                    *favs_only,
                                    &chats,
                                );
                                *selected = 0;
                                *scroll = 0;
                            }
                            other => {
                                if let Some(ch) = key_to_char(other, shift) {
                                    search_term.push(ch);
                                    *filtered = filter_transcripts_quick(
                                        transcripts,
                                        search_term,
                                        *favs_only,
                                        &chats,
                                    );
                                    *selected = 0;
                                    *scroll = 0;
                                }
                            }
                        }
                    } else {
                        match key {
                            Key::Escape => {
                                if show_help {
                                    show_help = false;
                                } else if !multi_selected.is_empty() {
                                    multi_selected.clear();
                                } else {
                                    quit_requested = true;
                                }
                            }
                            Key::T => {
                                if available_themes.is_empty() {
                                    available_themes = load_available_themes();
                                }
                                if !available_themes.is_empty() {
                                    theme_selected = (theme_selected + 1) % available_themes.len();
                                    cfg.theme = available_themes[theme_selected].2.clone();
                                    save_config(&cfg);
                                    needs_theme_reload = true;
                                    flash = Some((Instant::now(), format!("theme: {}", available_themes[theme_selected].1)));
                                }
                            }
                            Key::Slash if shift => {
                                // ? key
                                show_help = !show_help;
                            }
                            Key::H if !show_help => {
                                show_help = true;
                            }
                            Key::J if shift => {
                                let code = join::new_code();
                                if let Some(ref mut cb) = clipboard {
                                    let _ = cb.set_text(&code);
                                }
                                apply_join_code(Some(code), &mut cfg, &sync_key, &peer_state);
                                if my_tcp_port == 0 {
                                    my_tcp_port = start_sync(sources.clone(), Arc::clone(&sync_key), Arc::clone(&peer_state), my_hostname.clone());
                                }
                                flash = Some((Instant::now(), "code copied".into()));
                            }
                            Key::J => {
                                joining = true;
                                join_buf.clear();
                            }
                            Key::N if !show_help => {
                                *renaming = true;
                                rename_buf.clear();
                                // pre-fill with existing name if any
                                if let Some(entry) = filtered.get(*selected) {
                                    if let Some(meta) = chats.get(&entry.uuid) {
                                        if let Some(ref name) = meta.name {
                                            *rename_buf = name.clone();
                                        }
                                    }
                                }
                            }
                            Key::Space => {
                                // toggle multi-select on current item
                                if let Some(entry) = filtered.get(*selected) {
                                    let uuid = entry.uuid.clone();
                                    if multi_selected.contains(&uuid) {
                                        multi_selected.remove(&uuid);
                                    } else {
                                        multi_selected.insert(uuid);
                                    }
                                    // advance cursor
                                    *selected =
                                        (*selected + 1).min(filtered.len().saturating_sub(1));
                                }
                            }
                            Key::S => {
                                *searching = true;
                                search_term.clear();
                            }
                            Key::F if shift => {
                                *favs_only = !*favs_only;
                                *filtered = filter_transcripts_quick(
                                    transcripts,
                                    search_term,
                                    *favs_only,
                                    &chats,
                                );
                                *selected = 0;
                                *scroll = 0;
                            }
                            Key::F => {
                                if !multi_selected.is_empty() {
                                    // bulk star/unstar all selected
                                    // if any are unstarred, star them all; otherwise unstar all
                                    let any_unstarred = multi_selected.iter().any(|uuid| {
                                        !chats.get(uuid).map_or(false, |m| m.starred)
                                    });
                                    for uuid in &multi_selected {
                                        let meta = chats.entry(uuid.clone()).or_insert_with(ChatMeta::default);
                                        meta.starred = any_unstarred;
                                    }
                                    save_chats(&chats);
                                    multi_selected.clear();
                                    if *favs_only {
                                        *filtered = filter_transcripts_quick(
                                            transcripts,
                                            search_term,
                                            true,
                                            &chats,
                                        );
                                        *selected =
                                            (*selected).min(filtered.len().saturating_sub(1));
                                    }
                                } else if let Some(entry) = filtered.get(*selected) {
                                    let uuid = entry.uuid.clone();
                                    let meta = chats.entry(uuid).or_insert_with(ChatMeta::default);
                                    meta.starred = !meta.starred;
                                    save_chats(&chats);
                                    if *favs_only {
                                        *filtered = filter_transcripts_quick(
                                            transcripts,
                                            search_term,
                                            true,
                                            &chats,
                                        );
                                        *selected =
                                            (*selected).min(filtered.len().saturating_sub(1));
                                    }
                                }
                            }
                            Key::Up => {
                                *selected = (*selected as i32 - 1).max(0) as usize;
                            }
                            Key::Down => {
                                *selected =
                                    (*selected + 1).min(filtered.len().saturating_sub(1));
                            }
                            Key::Enter => {
                                saved_list_idx = *selected;
                                saved_list_scroll = *scroll;
                                if let Some(entry) = filtered.get(*selected).cloned() {
                                    let fmt = entry.source_format.clone();
                                    let msgs = match &entry.remote {
                                        Some(origin) => fetch_remote_messages(origin, &entry.uuid),
                                        None => load_messages(&entry.path, &fmt),
                                    };
                                    let groups = group_messages(&msgs);
                                    let expanded = HashSet::new();
                                    let wrap_w = chars_per_line(win_w, advance);
                                    let (lines, line_meta, in_code) =
                                        rebuild_view(&groups, &expanded, wrap_w);
                                    // scroll to bottom
                                    let visible = ((win_h as i32) - content_top - PAD) / lh;
                                    let scroll = (lines.len() as i32 - visible).max(0);
                                    let mtime = get_file_mtime(&entry.path);
                                    let chat_uuid = entry.uuid.clone();
                                    transition = Some(AppState::View {
                                        path: entry.path,
                                        source_format: fmt,
                                        groups,
                                        expanded,
                                        lines,
                                        line_meta,
                                        in_code,
                                        last_wrap_w: wrap_w,
                                        scroll,
                                        sel: Selection::default(),
                                        file_mtime: mtime,
                                        last_check: Instant::now(),
                                        remote: entry.remote.clone(),
                                        chat_uuid,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    let visible_lines = ((win_h as i32) - content_top - PAD) / lh;
                    let total_lines = list_total_lines(filtered.len(), *selected);
                    let sel_y = list_item_y_lines(*selected, *selected, filtered.len());
                    *scroll = (sel_y - visible_lines / 2)
                        .max(0)
                        .min((total_lines - visible_lines).max(0));
                }
                AppState::View {
                    path: _, source_format: _, lines, scroll, sel, chat_uuid, ..
                } => match key {
                    Key::Escape => {
                        if show_help {
                            show_help = false;
                        } else if sel.active {
                            sel.active = false;
                        } else {
                            let mut all = get_all_transcripts(&sources);
                            // merge remote peers back in
                            if let Ok(ps) = peer_state.try_lock() {
                                all.extend(ps.remote_entries.clone());
                                all.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
                            }
                            last_merged = all.clone();
                            let idx = saved_list_idx.min(all.len().saturating_sub(1));
                            transition = Some(AppState::List {
                                filtered: all.clone(),
                                transcripts: all,
                                selected: idx,
                                scroll: saved_list_scroll,
                                searching: false,
                                search_term: String::new(),
                                favs_only: false,
                                last_scan: Instant::now(),
                                renaming: false,
                                rename_buf: String::new(),
                            });
                        }
                    }
                    Key::Left | Key::Right => {
                        // navigate to prev/next chat using last_merged list
                        let all = last_merged.clone();
                        if let Some(idx) = all.iter().position(|t| t.uuid == *chat_uuid) {
                            let next_idx = if key == Key::Left {
                                if idx == 0 { all.len() - 1 } else { idx - 1 }
                            } else {
                                if idx + 1 >= all.len() { 0 } else { idx + 1 }
                            };
                            if let Some(entry) = all.get(next_idx).cloned() {
                                let fmt = entry.source_format.clone();
                                let msgs = match &entry.remote {
                                    Some(origin) => fetch_remote_messages(origin, &entry.uuid),
                                    None => load_messages(&entry.path, &fmt),
                                };
                                let groups = group_messages(&msgs);
                                let expanded = HashSet::new();
                                let wrap_w = chars_per_line(win_w, advance);
                                let (lines, line_meta, in_code) =
                                    rebuild_view(&groups, &expanded, wrap_w);
                                let visible = ((win_h as i32) - content_top - PAD) / lh;
                                let scroll = (lines.len() as i32 - visible).max(0);
                                let mtime = get_file_mtime(&entry.path);
                                let nav_uuid = entry.uuid.clone();
                                transition = Some(AppState::View {
                                    path: entry.path,
                                    source_format: fmt,
                                    groups,
                                    expanded,
                                    lines,
                                    line_meta,
                                    in_code,
                                    last_wrap_w: wrap_w,
                                    scroll,
                                    sel: Selection::default(),
                                    file_mtime: mtime,
                                    last_check: Instant::now(),
                                    remote: entry.remote.clone(),
                                    chat_uuid: nav_uuid,
                                });
                            }
                        }
                    }
                    Key::Slash if shift => {
                        // ? key
                        show_help = !show_help;
                    }
                    Key::H => {
                        show_help = !show_help;
                    }
                    Key::T => {
                        if available_themes.is_empty() {
                            available_themes = load_available_themes();
                        }
                        if !available_themes.is_empty() {
                            theme_selected = (theme_selected + 1) % available_themes.len();
                            cfg.theme = available_themes[theme_selected].2.clone();
                            save_config(&cfg);
                            needs_theme_reload = true;
                            flash = Some((Instant::now(), format!("theme: {}", available_themes[theme_selected].1)));
                        }
                    }
                    Key::Up if show_help => {}
                    Key::Down if show_help => {}
                    Key::Up => *scroll = (*scroll - 1).max(0),
                    Key::Down => {
                        let visible = ((win_h as i32) - content_top - PAD) / lh;
                        let max_scroll = (lines.len() as i32 - visible).max(0);
                        *scroll = (*scroll + 1).min(max_scroll);
                    }
                    Key::PageUp => *scroll = (*scroll - 15).max(0),
                    Key::PageDown => {
                        let visible = ((win_h as i32) - content_top - PAD) / lh;
                        let max_scroll = (lines.len() as i32 - visible).max(0);
                        *scroll = (*scroll + 15).min(max_scroll);
                    }
                    Key::Home => *scroll = 0,
                    Key::End => {
                        let visible = ((win_h as i32) - content_top - PAD) / lh;
                        *scroll = (lines.len() as i32 - visible).max(0);
                    }
                    _ => {}
                },
            }
        }

        // re-wrap if needed
        if let AppState::View {
            groups,
            expanded,
            lines,
            line_meta,
            in_code,
            last_wrap_w,
            ..
        } = &mut state
        {
            let wrap_w = chars_per_line(win_w, advance);
            if wrap_w != *last_wrap_w {
                let (nl, nm, nc) = rebuild_view(groups, expanded, wrap_w);
                *lines = nl;
                *line_meta = nm;
                *in_code = nc;
                *last_wrap_w = wrap_w;
            }
        }

        // ========== RENDER ==========

        // content
        match &state {
            AppState::List {
                filtered: list,
                selected,
                scroll,
                ..
            } => {
                let max_chars = chars_per_line(win_w, advance);
                let extra_preview_lines: i32 = 2; // extra lines for selected item
                let mut y = content_top - *scroll * lh;
                for i in 0..list.len() {
                    let t = &list[i];
                    let is_selected = i == *selected;
                    let is_multi = multi_selected.contains(&t.uuid);
                    let item_lines = if is_selected { LIST_LINES_PER_ITEM + extra_preview_lines } else { LIST_LINES_PER_ITEM };
                    let item_h = item_lines * lh;

                    // skip if entirely off-screen
                    if y + item_h < content_top {
                        y += item_h;
                        continue;
                    }
                    if y > win_h as i32 {
                        break;
                    }

                    // Background highlight
                    let content_h = item_h - lh; // exclude separator line
                    if is_selected {
                        fill_rect(&mut buffer, 0, y, win_w as i32, content_h, c_sel, buf_w, buf_h);
                    } else if is_multi {
                        fill_rect(&mut buffer, 0, y, win_w as i32, content_h, c_select_bg, buf_w, buf_h);
                    }

                    // Line 0: name + metadata
                    let star = if is_multi {
                        "> "
                    } else if chats.get(&t.uuid).map_or(false, |m| m.starred) {
                        "* "
                    } else {
                        "  "
                    };
                    let display_name = chats.get(&t.uuid).and_then(|m| m.name.as_ref())
                        .or(t.remote_name.as_ref());
                    let left_text = match display_name {
                        Some(name) => format!("{}{}", star, name),
                        None => {
                            let max_left = max_chars / 2;
                            let prev: String = t.preview.chars().take(max_left).collect();
                            format!("{}{}", star, prev)
                        }
                    };
                    draw_text_ttf(
                        &mut buffer, PAD, y, &left_text,
                        if is_selected { c_accent } else { c_dim },
                        &mut atlas, buf_w, buf_h,
                    );

                    // Right side: remote host · platform  timestamp (each with its own color)
                    {
                        let ts_text = &t.timestamp;
                        let ts_w = ts_text.len() as i32 * advance;
                        let mut rx = win_w as i32 - PAD - ts_w;
                        draw_text_ttf(&mut buffer, rx, y, ts_text, c_datetime, &mut atlas, buf_w, buf_h);

                        let plat_text = format!("{}  ", t.project);
                        let plat_w = plat_text.len() as i32 * advance;
                        rx -= plat_w;
                        draw_text_ttf(&mut buffer, rx, y, &plat_text, c_platform, &mut atlas, buf_w, buf_h);

                        if let Some(origin) = &t.remote {
                            let host_text = format!("{} · ", origin.hostname);
                            let host_w = host_text.len() as i32 * advance;
                            rx -= host_w;
                            draw_text_ttf(&mut buffer, rx, y, &host_text, c_remote_host, &mut atlas, buf_w, buf_h);
                        }
                    }

                    // Preview lines (respects newlines in text)
                    let preview_text = if !t.last_preview.is_empty() { &t.last_preview } else { &t.preview };
                    if !preview_text.is_empty() {
                        let max_lines = if is_selected { 1 + extra_preview_lines as usize } else { 1 };
                        let line_max = max_chars.saturating_sub(5);
                        // Split on newlines first, then wrap each line
                        let mut preview_lines: Vec<String> = Vec::new();
                        for paragraph in preview_text.split('\n') {
                            let trimmed = paragraph.trim();
                            if trimmed.is_empty() { continue; }
                            let chars: Vec<char> = trimmed.chars().collect();
                            let mut pos = 0;
                            while pos < chars.len() && preview_lines.len() < max_lines {
                                let end = (pos + line_max).min(chars.len());
                                let segment: String = chars[pos..end].iter().collect();
                                preview_lines.push(segment);
                                pos = end;
                            }
                            if preview_lines.len() >= max_lines { break; }
                        }
                        for (pl, segment) in preview_lines.iter().enumerate().take(max_lines) {
                            let line_text = if pl == max_lines - 1 && preview_text.chars().count() > line_max * max_lines {
                                format!("     {}..", segment)
                            } else {
                                format!("     {}", segment)
                            };
                            draw_text_ttf(
                                &mut buffer, PAD, y + (1 + pl as i32) * lh, &line_text,
                                if is_selected { c_text } else { c_msg_time },
                                &mut atlas, buf_w, buf_h,
                            );
                        }
                    }

                    // Separator at bottom of item
                    let sep_y = y + content_h + lh / 2;
                    fill_rect(&mut buffer, PAD, sep_y, (win_w as i32) - 2 * PAD, 1, c_sep, buf_w, buf_h);

                    y += item_h;
                }
            }
            AppState::View {
                lines,
                line_meta,
                in_code,
                scroll,
                sel,
                ..
            } => {
                let mut y = content_top - *scroll * lh;
                let ((sel_sl, sel_sc), (sel_el, sel_ec)) = sel.ordered();
                for (li, line) in lines.iter().enumerate() {
                    if y + lh < content_top || y > win_h as i32 {
                        y += lh;
                        continue;
                    }

                    // selection highlight
                    if sel.active && li >= sel_sl && li <= sel_el {
                        let chars_count = line.chars().count();
                        let start_col = if li == sel_sl {
                            sel_sc.min(chars_count)
                        } else {
                            0
                        };
                        let end_col = if li == sel_el {
                            sel_ec.min(chars_count)
                        } else {
                            chars_count
                        };
                        if end_col > start_col {
                            let hx = PAD + start_col as i32 * advance;
                            let hw = (end_col - start_col) as i32 * advance;
                            fill_rect(
                                &mut buffer, hx, y, hw, lh, c_select_bg, buf_w, buf_h,
                            );
                        }
                    }

                    let meta = line_meta.get(li);

                    // timestamp lines
                    if matches!(meta, Some(LineMeta::Timestamp)) {
                        draw_text_ttf(
                            &mut buffer, PAD, y, line, c_msg_time, &mut atlas, buf_w, buf_h,
                        );
                        y += lh;
                        continue;
                    }

                    // toggle lines (clickable)
                    if matches!(meta, Some(LineMeta::Toggle(_))) {
                        draw_text_ttf(
                            &mut buffer, PAD, y, line, c_toggle, &mut atlas, buf_w, buf_h,
                        );
                        y += lh;
                        continue;
                    }

                    // code block lines
                    if *in_code.get(li).unwrap_or(&false) {
                        let usable_w = (win_w as i32) - 2 * PAD;
                        fill_rect(
                            &mut buffer,
                            PAD - 2,
                            y,
                            usable_w + 4,
                            lh,
                            c_code_bg,
                            buf_w,
                            buf_h,
                        );
                        draw_text_ttf(
                            &mut buffer, PAD, y, line, c_code, &mut atlas, buf_w, buf_h,
                        );
                        y += lh;
                        continue;
                    }

                    // determine base color from role prefix
                    let base_color = if line.starts_with("[user] ") {
                        c_user
                    } else if line.starts_with("[asst] ") {
                        c_asst
                    } else {
                        c_text
                    };

                    // check for headers (# or ##)
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("## ") || trimmed.starts_with("# ") {
                        draw_text_ttf(
                            &mut buffer, PAD, y, line, c_heading, &mut atlas, buf_w, buf_h,
                        );
                        y += lh;
                        continue;
                    }

                    // inline markdown parsing
                    let runs = parse_inline_markdown(line, base_color);
                    draw_styled_runs(&mut buffer, PAD, y, &runs, &mut atlas, buf_w, buf_h);
                    y += lh;
                }
            }
        }

        // ===== HEADER (drawn OVER content) =====
        fill_rect(
            &mut buffer, 0, 0, win_w as i32, HEADER_H, c_header_bg, buf_w, buf_h,
        );
        fill_rect(
            &mut buffer, 0, HEADER_H, win_w as i32, 1, c_sep, buf_w, buf_h,
        );
        {
            let header_y = (HEADER_H - lh) / 2;
            match &state {
                AppState::List {
                    filtered,
                    searching,
                    search_term,
                    favs_only,
                    renaming,
                    rename_buf,
                    ..
                } => {
                    let title = if *favs_only {
                        "chat-daddy *favs*"
                    } else {
                        "chat-daddy"
                    };
                    draw_text_ttf(
                        &mut buffer, PAD, header_y, title, c_accent, &mut atlas, buf_w, buf_h,
                    );
                    if joining {
                        let prompt = format!("code: {}_", join_buf);
                        let prompt_w = prompt.len() as i32 * advance;
                        let sx = win_w as i32 - PAD - prompt_w - 8;
                        fill_rect(
                            &mut buffer, sx - 4, 2, prompt_w + 12, HEADER_H - 4, c_search_bg,
                            buf_w, buf_h,
                        );
                        draw_text_ttf(
                            &mut buffer, sx, header_y, &prompt, c_text, &mut atlas, buf_w, buf_h,
                        );
                    } else if *renaming {
                        let prompt = format!("name: {}_", rename_buf);
                        let prompt_w = prompt.len() as i32 * advance;
                        let sx = win_w as i32 - PAD - prompt_w - 8;
                        fill_rect(
                            &mut buffer, sx - 4, 2, prompt_w + 12, HEADER_H - 4, c_search_bg,
                            buf_w, buf_h,
                        );
                        draw_text_ttf(
                            &mut buffer, sx, header_y, &prompt, c_text, &mut atlas, buf_w, buf_h,
                        );
                    } else if *searching {
                        let prompt = format!("/{}_  [Enter]deep", search_term);
                        let prompt_w = prompt.len() as i32 * advance;
                        let sx = win_w as i32 - PAD - prompt_w - 8;
                        fill_rect(
                            &mut buffer, sx - 4, 2, prompt_w + 12, HEADER_H - 4, c_search_bg,
                            buf_w, buf_h,
                        );
                        draw_text_ttf(
                            &mut buffer, sx, header_y, &prompt, c_text, &mut atlas, buf_w, buf_h,
                        );
                    } else {
                        let count_str = if multi_selected.is_empty() {
                            format!("{} chats  ?", filtered.len())
                        } else {
                            format!("{} selected  Ctrl+E export  ?", multi_selected.len())
                        };
                        let hint_w = count_str.len() as i32 * advance;
                        draw_text_ttf(
                            &mut buffer,
                            win_w as i32 - PAD - hint_w,
                            header_y,
                            &count_str,
                            c_dim,
                            &mut atlas,
                            buf_w,
                            buf_h,
                        );
                    }
                }
                AppState::View { path, chat_uuid, remote, .. } => {
                    let title = path
                        .file_stem()
                        .map(|s| s.to_string_lossy())
                        .unwrap_or_default();
                    // show chat name if available, else uuid
                    let uuid_str = if chat_uuid.is_empty() { title.to_string() } else { chat_uuid.clone() };
                    let _remote_label = remote.as_ref().map(|o| o.hostname.clone());
                    let display = chats
                        .get(&uuid_str)
                        .and_then(|m| m.name.as_deref())
                        .unwrap_or(&uuid_str);
                    draw_text_ttf(
                        &mut buffer,
                        PAD,
                        header_y,
                        &format!("chat-daddy > {}", display),
                        c_accent,
                        &mut atlas,
                        buf_w,
                        buf_h,
                    );
                    let hint = "[</>] ?";
                    let hint_w = hint.len() as i32 * advance;
                    draw_text_ttf(
                        &mut buffer,
                        win_w as i32 - PAD - hint_w,
                        header_y,
                        hint,
                        c_dim,
                        &mut atlas,
                        buf_w,
                        buf_h,
                    );
                }
            }
        }

        // ===== HELP OVERLAY =====
        if show_help {
            let hotkeys: &[&str] = match &state {
                AppState::List { .. } => &[
                    "  Up/Down      navigate",
                    "  Enter        open chat",
                    "  Left/Right   prev/next chat",
                    "  S            search",
                    "  N            rename chat",
                    "  Space        multi-select",
                    "  F            star/unstar",
                    "  Shift+F      toggle favs filter",
                    "  Ctrl+E       export selected",
                    "  J            join code",
                    "  Shift+J      new code",
                    "  Ctrl+/\u{2212}      font size",
                    "  Esc          clear / quit",
                    "  ? / H        toggle help",
                ],
                AppState::View { .. } => &[
                    "  Up/Down      scroll",
                    "  PgUp/PgDn    fast scroll",
                    "  Home/End     top/bottom",
                    "  Left/Right   prev/next chat",
                    "  Ctrl+C       copy selection",
                    "  Ctrl+A       select all",
                    "  Ctrl+/\u{2212}      font size",
                    "  Esc          back to list",
                    "  ? / H        toggle help",
                ],
            };
            let icon_size: i32 = if help_icon.is_some() { 96 } else { 0 };
            let icon_gap: i32 = if help_icon.is_some() { 8 } else { 0 };
            let title = format!("chat-daddy v{}", env!("CARGO_PKG_VERSION"));
            let mode_label = match &state {
                AppState::List { .. } => "LIST",
                AppState::View { .. } => "VIEW",
            };
            let footer = "lucianlabs.ca";

            // Gather connected peers
            let peer_names: Vec<String> = if let Ok(ps) = peer_state.try_lock() {
                let mut names: Vec<String> = ps.peers.values().map(|p| p.hostname.clone()).collect();
                names.sort();
                names
            } else {
                vec![]
            };

            // Calculate panel dimensions
            let panel_w = 38 * advance;
            let sep_h = 1;
            let gap = lh / 2;
            let mut total_h: i32 = 0;
            total_h += icon_size + icon_gap; // icon
            total_h += lh; // title
            total_h += lh / 2; // mode label
            total_h += gap + sep_h + gap; // separator 1
            total_h += hotkeys.len() as i32 * lh; // hotkeys
            total_h += lh; // "T  cycle theme" line
            total_h += gap + sep_h + gap; // separator 2
            // Connected peers section (always show)
            total_h += lh; // join code or "sync off"
            if cfg.join_code.is_some() {
                total_h += peer_names.len().max(1) as i32 * lh; // peer names or "no peers"
            }
            total_h += gap + sep_h + gap; // separator 3
            total_h += lh; // theme label
            total_h += lh; // footer

            let px = (win_w as i32 - panel_w) / 2;
            let py = (win_h as i32 - total_h) / 2;
            let pad2 = PAD * 2;
            let panel_full_w = panel_w + 4 * PAD;
            let panel_full_h = total_h + 4 * PAD;
            // Drop shadow (offset -6px left, +8px down — light source top-right)
            let shadow_dx: i32 = -6;
            let shadow_dy: i32 = 8;
            for layer in 0..4i32 {
                fill_rect(
                    &mut buffer,
                    px - pad2 + shadow_dx - layer,
                    py - pad2 + shadow_dy + layer,
                    panel_full_w + 2 * layer,
                    panel_full_h + 2 * layer,
                    (0, 0, 0),
                    buf_w,
                    buf_h,
                );
            }
            // dark background
            fill_rect(&mut buffer, px - pad2, py - pad2, panel_full_w, panel_full_h, c_header_bg, buf_w, buf_h);
            // border (1px all around)
            fill_rect(&mut buffer, px - pad2, py - pad2, panel_full_w, 1, c_sep, buf_w, buf_h);
            fill_rect(&mut buffer, px - pad2, py - pad2 + panel_full_h - 1, panel_full_w, 1, c_sep, buf_w, buf_h);
            fill_rect(&mut buffer, px - pad2, py - pad2, 1, panel_full_h, c_sep, buf_w, buf_h);
            fill_rect(&mut buffer, px - pad2 + panel_full_w - 1, py - pad2, 1, panel_full_h, c_sep, buf_w, buf_h);

            let mut cy = py;
            // Icon (centered)
            if let Some((iw, ih, ref icon_pixels)) = help_icon {
                let icon_x = px + (panel_w - iw as i32) / 2;
                blit_icon(&mut buffer, icon_x, cy, icon_pixels, iw, ih, buf_w, buf_h);
                cy += icon_size + icon_gap;
            }
            // Title "chat-daddy" (centered)
            let title_w = title.len() as i32 * advance;
            let title_x = px + (panel_w - title_w) / 2;
            draw_text_ttf(&mut buffer, title_x, cy, &title, c_accent, &mut atlas, buf_w, buf_h);
            cy += lh;
            // Mode label (centered, dim)
            let mode_w = mode_label.len() as i32 * advance;
            let mode_x = px + (panel_w - mode_w) / 2;
            draw_text_ttf(&mut buffer, mode_x, cy, mode_label, c_dim, &mut atlas, buf_w, buf_h);
            cy += lh / 2;
            // Separator 1
            cy += gap;
            fill_rect(&mut buffer, px, cy, panel_w, sep_h, c_sep, buf_w, buf_h);
            cy += sep_h + gap;
            // Hotkeys
            for line in hotkeys {
                draw_text_ttf(&mut buffer, px, cy, line, c_text, &mut atlas, buf_w, buf_h);
                cy += lh;
            }
            // Theme selector hint
            draw_text_ttf(&mut buffer, px, cy, "  T            cycle theme", c_toggle, &mut atlas, buf_w, buf_h);
            cy += lh;
            // Separator 2
            cy += gap;
            fill_rect(&mut buffer, px, cy, panel_w, sep_h, c_sep, buf_w, buf_h);
            cy += sep_h + gap;
            // Connected peers section (always show)
            match &cfg.join_code {
                Some(code) => {
                    let net_label = format!("  {}  {}", my_hostname, code);
                    draw_text_ttf(&mut buffer, px, cy, &net_label, c_dim, &mut atlas, buf_w, buf_h);
                }
                None => draw_text_ttf(&mut buffer, px, cy, "  sync off", c_dim, &mut atlas, buf_w, buf_h),
            }
            cy += lh;
            if cfg.join_code.is_none() {
                // sync off: no peer list
            } else if !peer_names.is_empty() {
                for name in &peer_names {
                    let peer_line = format!("    {}", name);
                    draw_text_ttf(&mut buffer, px, cy, &peer_line, c_accent, &mut atlas, buf_w, buf_h);
                    cy += lh;
                }
            } else {
                draw_text_ttf(&mut buffer, px, cy, "    no peers", c_dim, &mut atlas, buf_w, buf_h);
                cy += lh;
            }
            cy += gap;
            fill_rect(&mut buffer, px, cy, panel_w, sep_h, c_sep, buf_w, buf_h);
            cy += sep_h + gap;
            // Current theme name (centered)
            let theme_label = if let Some((_, ref name, _)) = available_themes.get(theme_selected) {
                format!("theme: {}", name)
            } else {
                "theme: default".to_string()
            };
            let theme_w = theme_label.len() as i32 * advance;
            let theme_x = px + (panel_w - theme_w) / 2;
            draw_text_ttf(&mut buffer, theme_x, cy, &theme_label, c_toggle, &mut atlas, buf_w, buf_h);
            cy += lh;
            // Footer "lucianlabs.ca" (centered, dim)
            let footer_w = footer.len() as i32 * advance;
            let footer_x = px + (panel_w - footer_w) / 2;
            draw_text_ttf(&mut buffer, footer_x, cy, footer, c_dim, &mut atlas, buf_w, buf_h);

        }

        // Status flash at bottom of window (theme name, join code) for 2 seconds
        if let Some((flash_time, ref label)) = flash {
            if flash_time.elapsed().as_secs_f32() < 2.0 {
                let label_w = label.chars().count() as i32 * advance;
                let tx = (win_w as i32 - label_w) / 2;
                let ty = win_h as i32 - lh - PAD;
                fill_rect(&mut buffer, tx - PAD, ty - 2, label_w + PAD * 2, lh + 4, c_header_bg, buf_w, buf_h);
                draw_text_ttf(&mut buffer, tx, ty, label, c_accent, &mut atlas, buf_w, buf_h);
            } else {
                flash = None;
            }
        }

        if snapshot {
            let _ = dump_png("debug-frame.png", &buffer, buf_w as u32, buf_h as u32);
            break;
        }

        // Track window position and size each frame (get_position returns 0,0 after close)
        let (wx, wy) = window.get_position();
        if wx != 0 || wy != 0 {
            last_window_pos = (wx, wy);
        }
        last_window_size = (win_w, win_h);

        // Use buf_w/buf_h (captured at frame start) so dimensions always match the buffer.
        // During a live resize, get_size() may have changed since we allocated — this prevents
        // a mismatch panic or stretched pixels.
        let _ = window.update_with_buffer(&buffer, buf_w, buf_h);
    }

    // Save last known window position and size on exit
    let wstate = serde_json::json!({
        "x": last_window_pos.0,
        "y": last_window_pos.1,
        "w": last_window_size.0,
        "h": last_window_size.1,
    });
    let _ = fs::write(&window_state_path, serde_json::to_string_pretty(&wstate).unwrap_or_default());
}

fn dump_png(path: &str, buf: &[u32], w: u32, h: u32) -> Result<(), image::ImageError> {
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for &pix in buf {
        let r = ((pix >> 16) & 0xFF) as u8;
        let g = ((pix >> 8) & 0xFF) as u8;
        let b = (pix & 0xFF) as u8;
        out.push(r);
        out.push(g);
        out.push(b);
    }
    save_buffer(path, &out, w, h, ColorType::Rgb8)
}
