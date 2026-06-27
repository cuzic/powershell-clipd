//! clipd — Windows clipboard HTTP server (Rust port)
//!
//! Build on Windows: cargo build --release
//! Run: clipd.exe --token <secret>

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use anyhow::{bail, Result};
use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::Parser;
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::{info, warn};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "clipd", about = "Windows clipboard HTTP server")]
struct Args {
    #[arg(long, default_value = "9999")]
    port: u16,

    /// Bearer token (or set CLIPD_TOKEN env var)
    #[arg(long, env = "CLIPD_TOKEN")]
    token: Option<String>,

    /// Bind to localhost only (skip Tailscale detection)
    #[arg(long)]
    bind_localhost_only: bool,

    /// Allow serving over the network without any token
    #[arg(long)]
    allow_no_token: bool,
}

// ── Domain types ──────────────────────────────────────────────────────────────

#[derive(Debug)]
enum ClipKind {
    Image(Vec<u8>),
    Files(Vec<String>),
    VFiles(Vec<String>),
    Audio(Vec<u8>),
    Html(String),
    Url(String),
    Rtf(String),
    Text(String),
    Empty,
}

enum ClipRequest {
    GetClip  { reply: oneshot::Sender<ClipKind> },
    GetFile  { path: String,   reply: oneshot::Sender<Option<Vec<u8>>> },
    GetVFile { index: usize,   reply: oneshot::Sender<Option<Vec<u8>>> },
}

#[derive(Clone, Default)]
struct LastClip {
    files:  Vec<String>,
    vfiles: Vec<String>,
}

#[derive(Clone)]
struct AppState {
    clip_tx:   mpsc::SyncSender<ClipRequest>,
    token:     Option<String>,
    last_clip: Arc<Mutex<LastClip>>,
}

// ── Windows clipboard implementation ──────────────────────────────────────────

#[cfg(windows)]
mod win_clip {
    use super::ClipKind;
    use anyhow::{bail, Context, Result};
    use windows::{
        Win32::{
            Foundation::{GetLastError, HANDLE, HGLOBAL, WIN32_ERROR},
            System::{
                Com::{
                    CoInitializeEx, CoUninitialize, IDataObject, IStream,
                    COINIT_APARTMENTTHREADED, DVASPECT_CONTENT, FORMATETC,
                    STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM,
                },
                DataExchange::{
                    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable,
                    OpenClipboard, RegisterClipboardFormatW,
                },
                Memory::{GlobalLock, GlobalSize, GlobalUnlock},
                Ole::{OleGetClipboard, OleInitialize, ReleaseStgMedium},
                Threading::CreateMutexW,
            },
            UI::Shell::DragQueryFileW,
        },
        core::{w, PWSTR},
    };

    // Predefined clipboard format IDs
    const CF_DIB:         u32 = 8;
    const CF_WAVE:        u32 = 12;
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP:       u32 = 15;

    // RAII clipboard open/close
    struct ClipGuard;
    impl ClipGuard {
        fn open() -> Result<Self> {
            unsafe { OpenClipboard(None)? };
            Ok(ClipGuard)
        }
    }
    impl Drop for ClipGuard {
        fn drop(&mut self) { unsafe { let _ = CloseClipboard(); } }
    }

    unsafe fn fmt_avail(fmt: u32) -> bool {
        IsClipboardFormatAvailable(fmt).is_ok()
    }

    unsafe fn reg_fmt(name: windows::core::PCWSTR) -> u32 {
        RegisterClipboardFormatW(name)
    }

    /// Copy HGLOBAL clipboard data into a Vec<u8>.
    unsafe fn hglobal_bytes(h: HANDLE) -> Vec<u8> {
        let hg = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        let ptr  = GlobalLock(hg);
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _ = GlobalUnlock(hg);
        data
    }

    // ── Per-format readers ────────────────────────────────────────────────────

    pub unsafe fn read_clipboard() -> ClipKind {
        let fmt_fgd  = reg_fmt(w!("FileGroupDescriptorW"));
        let fmt_fc   = reg_fmt(w!("FileContents"));
        let fmt_html = reg_fmt(w!("HTML Format"));
        let fmt_url  = reg_fmt(w!("UniformResourceLocatorW"));
        let fmt_rtf  = reg_fmt(w!("Rich Text Format"));

        if fmt_avail(CF_DIB) {
            if let Ok(data) = read_image() { return ClipKind::Image(data); }
        }
        if fmt_avail(CF_HDROP) {
            if let Ok(v) = read_files() { if !v.is_empty() { return ClipKind::Files(v); } }
        }
        if fmt_avail(fmt_fgd) && fmt_avail(fmt_fc) {
            if let Ok(v) = read_vfile_names() { if !v.is_empty() { return ClipKind::VFiles(v); } }
        }
        if fmt_avail(CF_WAVE) {
            if let Ok(data) = read_wave() { return ClipKind::Audio(data); }
        }
        if fmt_avail(fmt_url) {
            if let Ok(url) = read_url(fmt_url) { return ClipKind::Url(url); }
        }
        if fmt_avail(fmt_html) {
            if let Ok(html) = read_html(fmt_html) { return ClipKind::Html(html); }
        }
        if fmt_avail(fmt_rtf) {
            if let Ok(rtf) = read_rtf(fmt_rtf) { return ClipKind::Rtf(rtf); }
        }
        if fmt_avail(CF_UNICODETEXT) {
            if let Ok(t) = read_text() { if !t.is_empty() { return ClipKind::Text(t); } }
        }
        ClipKind::Empty
    }

    unsafe fn read_image() -> Result<Vec<u8>> {
        let _g = ClipGuard::open()?;
        let h  = GetClipboardData(CF_DIB).context("CF_DIB")?;
        dib_to_png(&hglobal_bytes(h))
    }

    fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
        if dib.len() < 40 { bail!("DIB too short"); }
        let info_size  = u32::from_le_bytes(dib[0..4].try_into()?) as usize;
        let bpp        = u16::from_le_bytes(dib[14..16].try_into()?) as usize;
        let clr_count  = if bpp <= 8 { 1usize << bpp } else { 0 };
        let pix_offset = 14 + info_size + clr_count * 4;
        let file_size  = 14 + dib.len();

        let mut bmp = Vec::with_capacity(file_size);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());           // reserved
        bmp.extend_from_slice(&(pix_offset as u32).to_le_bytes());
        bmp.extend_from_slice(dib);

        let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp)?;
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        Ok(png)
    }

    unsafe fn read_files() -> Result<Vec<String>> {
        let _g    = ClipGuard::open()?;
        let h     = GetClipboardData(CF_HDROP).context("CF_HDROP")?;
        // DragQueryFileW with iFile=0xFFFFFFFF returns the count
        let count = DragQueryFileW(h, 0xFFFF_FFFF, PWSTR::null(), 0);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len   = DragQueryFileW(h, i, PWSTR::null(), 0) as usize + 1;
            let mut buf = vec![0u16; len];
            DragQueryFileW(h, i, PWSTR(buf.as_mut_ptr()), len as u32);
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            paths.push(String::from_utf16_lossy(&buf[..end]).into_owned());
        }
        Ok(paths)
    }

    unsafe fn read_vfile_names() -> Result<Vec<String>> {
        let fmt = reg_fmt(w!("FileGroupDescriptorW"));
        let _g  = ClipGuard::open()?;
        let h   = GetClipboardData(fmt).context("FileGroupDescriptorW")?;
        parse_fgd(&hglobal_bytes(h))
    }

    fn parse_fgd(data: &[u8]) -> Result<Vec<String>> {
        if data.len() < 4 { bail!("FGD too short"); }
        let count = u32::from_le_bytes(data[0..4].try_into()?) as usize;
        const ENTRY: usize = 592; // sizeof(FILEDESCRIPTORW)
        const NAME:  usize = 72;  // offset of cFileName within entry
        let mut names = Vec::with_capacity(count);
        for i in 0..count {
            let base = 4 + i * ENTRY;
            if base + ENTRY > data.len() { break; }
            let wdata = &data[base + NAME .. base + NAME + 520]; // 260 WCHAR
            let words: Vec<u16> = wdata.chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
            names.push(String::from_utf16_lossy(&words[..end]).into_owned());
        }
        Ok(names)
    }

    pub unsafe fn get_vfile_contents(index: usize) -> Result<Vec<u8>> {
        let fmt_fc = reg_fmt(w!("FileContents"));
        let _ = OleInitialize(None); // idempotent, STA already inited

        let mut data_obj: Option<IDataObject> = None;
        OleGetClipboard(&mut data_obj)?;
        let data_obj = data_obj.context("OleGetClipboard returned null")?;

        let mut fetc = FORMATETC {
            cfFormat: fmt_fc as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: index as i32,
            tymed: TYMED_ISTREAM.0 as u32,
        };
        let mut stgm = STGMEDIUM::default();

        // Try IStream first
        if data_obj.GetData(&fetc, &mut stgm).is_ok()
            && stgm.tymed == TYMED_ISTREAM
        {
            let stream: &Option<IStream> = &*stgm.Anonymous.pstm;
            let data = if let Some(s) = stream { drain_istream(s)? } else { bail!("null IStream") };
            ReleaseStgMedium(&mut stgm);
            return Ok(data);
        }

        // Fall back to HGLOBAL
        fetc.tymed = TYMED_HGLOBAL.0 as u32;
        data_obj.GetData(&fetc, &mut stgm)?;
        let hg   = stgm.Anonymous.hGlobal;
        let size = GlobalSize(hg);
        let ptr  = GlobalLock(hg);
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _    = GlobalUnlock(hg);
        ReleaseStgMedium(&mut stgm);
        Ok(data)
    }

    unsafe fn drain_istream(stream: &IStream) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let mut read = 0u32;
            // S_FALSE (EOF) causes an Err here, which is fine — read will be 0
            let _ = stream.Read(chunk.as_mut_ptr() as *mut _, chunk.len() as u32, Some(&mut read));
            if read == 0 { break; }
            buf.extend_from_slice(&chunk[..read as usize]);
        }
        Ok(buf)
    }

    unsafe fn read_wave() -> Result<Vec<u8>> {
        let _g = ClipGuard::open()?;
        let h  = GetClipboardData(CF_WAVE).context("CF_WAVE")?;
        Ok(hglobal_bytes(h))
    }

    unsafe fn read_html(fmt: u32) -> Result<String> {
        let _g   = ClipGuard::open()?;
        let h    = GetClipboardData(fmt).context("HTML Format")?;
        let data = hglobal_bytes(h);
        // Windows HTML Format header: "StartHTML:000000071\r\n..."
        let hdr = String::from_utf8_lossy(&data[..data.len().min(512)]);
        let start = hdr.lines()
            .find(|l| l.starts_with("StartHTML:"))
            .and_then(|l| l[10..].trim().parse::<usize>().ok())
            .unwrap_or(0);
        Ok(String::from_utf8_lossy(if start < data.len() { &data[start..] } else { &data }).into_owned())
    }

    unsafe fn read_url(fmt: u32) -> Result<String> {
        let _g   = ClipGuard::open()?;
        let h    = GetClipboardData(fmt).context("URL format")?;
        let data = hglobal_bytes(h);
        let words: Vec<u16> = data.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
        Ok(String::from_utf16_lossy(&words[..end]).trim().to_string())
    }

    unsafe fn read_rtf(fmt: u32) -> Result<String> {
        let _g   = ClipGuard::open()?;
        let h    = GetClipboardData(fmt).context("RTF")?;
        Ok(String::from_utf8_lossy(&hglobal_bytes(h)).into_owned())
    }

    unsafe fn read_text() -> Result<String> {
        let _g   = ClipGuard::open()?;
        let h    = GetClipboardData(CF_UNICODETEXT).context("CF_UNICODETEXT")?;
        let data = hglobal_bytes(h);
        let words: Vec<u16> = data.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
        Ok(String::from_utf16_lossy(&words[..end]).into_owned())
    }

    // ── Balloon notification ──────────────────────────────────────────────────

    pub fn show_balloon(msg: &str) {
        // Spawn a short-lived thread to show a WinRT toast notification.
        // Requires Windows 10+. Falls back to a no-op on older systems.
        let msg = msg.to_string();
        thread::spawn(move || {
            // Best-effort: ignore errors
            let _ = show_toast(&msg);
        });
    }

    #[allow(unused_variables)]
    fn show_toast(msg: &str) -> Result<()> {
        // We avoid adding winrt-toast as a dep; use powershell as a thin shim.
        std::process::Command::new("powershell")
            .args([
                "-NoProfile", "-WindowStyle", "Hidden", "-Command",
                &format!(
                    "[void][System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms');\
                     $n=New-Object System.Windows.Forms.NotifyIcon;\
                     $n.Icon=[System.Drawing.SystemIcons]::Information;\
                     $n.Visible=$true;\
                     $n.ShowBalloonTip(4000,'clipd','{}',\
                     [System.Windows.Forms.ToolTipIcon]::Info);\
                     Start-Sleep 5;$n.Dispose()",
                    msg.replace('\'', "")
                ),
            ])
            .spawn()?;
        Ok(())
    }

    // ── Single-instance mutex ─────────────────────────────────────────────────

    pub unsafe fn acquire_mutex() -> windows::Win32::Foundation::HANDLE {
        match CreateMutexW(None, true, w!("Global\\clipd_singleton")) {
            Ok(h) => {
                if GetLastError() == WIN32_ERROR(183) { // ERROR_ALREADY_EXISTS
                    eprintln!("clipd is already running.");
                    std::process::exit(1);
                }
                h
            }
            Err(e) => {
                eprintln!("CreateMutexW failed: {e}");
                std::process::exit(1);
            }
        }
    }

    // ── STA thread entry point ────────────────────────────────────────────────

    pub fn sta_loop(rx: std::sync::mpsc::Receiver<super::ClipRequest>) {
        unsafe {
            // STA required for clipboard and COM/OLE operations
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

            use super::ClipRequest;
            for req in rx {
                match req {
                    ClipRequest::GetClip { reply } => {
                        let _ = reply.send(read_clipboard());
                    }
                    ClipRequest::GetFile { path, reply } => {
                        let _ = reply.send(std::fs::read(&path).ok());
                    }
                    ClipRequest::GetVFile { index, reply } => {
                        let _ = reply.send(get_vfile_contents(index).ok());
                    }
                }
            }

            CoUninitialize();
        }
    }
}

// ── Network helpers ────────────────────────────────────────────────────────────

fn find_tailscale_ip() -> Option<Ipv4Addr> {
    // 1. Ask tailscale directly
    if let Ok(out) = std::process::Command::new("tailscale").args(["ip", "-4"]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Ok(ip) = s.trim().parse::<Ipv4Addr>() {
                return Some(ip);
            }
        }
    }
    // 2. Scan ipconfig output for CGNAT range (100.64.0.0/10)
    if let Ok(out) = std::process::Command::new("ipconfig").output() {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            let line = line.trim();
            if line.starts_with("IPv4") || line.contains("IP Address") {
                if let Some(part) = line.split(':').nth(1) {
                    if let Ok(ip) = part.trim().parse::<Ipv4Addr>() {
                        if is_cgnat(ip) {
                            return Some(ip);
                        }
                    }
                }
            }
        }
    }
    None
}

fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // 100.64.0.0/10: first octet 100, second 64–127
    o[0] == 100 && (o[1] & 0xC0) == 0x40
}

// ── Auth helper ────────────────────────────────────────────────────────────────

fn check_auth(token: &Option<String>, headers: &HeaderMap) -> bool {
    let Some(expected) = token else { return true };
    headers.get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s == format!("Bearer {expected}"))
        .unwrap_or(false)
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "Unauthorized\n").into_response()
}

// ── HTTP handlers ──────────────────────────────────────────────────────────────

async fn handle_health() -> &'static str { "OK\n" }

async fn handle_clip(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&s.token, &headers) { return unauthorized(); }

    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::GetClip { reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let Ok(kind) = rx.await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    match kind {
        ClipKind::Image(data) => {
            s.last_clip.lock().unwrap().files.clear();
            clip_resp("image", "image/png", data)
        }
        ClipKind::Files(paths) => {
            let json = serde_json::to_vec(&paths).unwrap();
            s.last_clip.lock().unwrap().files = paths;
            clip_resp("files", "application/json", json)
        }
        ClipKind::VFiles(names) => {
            let json = serde_json::to_vec(&names).unwrap();
            { let mut lc = s.last_clip.lock().unwrap(); lc.vfiles = names; }
            clip_resp("vfiles", "application/json", json)
        }
        ClipKind::Audio(data)  => clip_resp("audio", "audio/wav", data),
        ClipKind::Html(html)   => clip_resp("html",  "text/html; charset=utf-8",  html.into_bytes()),
        ClipKind::Url(url)     => clip_resp("url",   "text/plain; charset=utf-8", url.into_bytes()),
        ClipKind::Rtf(rtf)     => clip_resp("rtf",   "text/rtf",                  rtf.into_bytes()),
        ClipKind::Text(text)   => clip_resp("text",  "text/plain; charset=utf-8", text.into_bytes()),
        ClipKind::Empty => {
            #[cfg(windows)]
            win_clip::show_balloon("クリップボードが空です");
            clip_resp("empty", "text/plain; charset=utf-8", b"".to_vec())
        }
    }
}

fn clip_resp(kind: &str, ct: &str, body: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("X-Clip-Kind", kind)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap()
}

#[derive(Deserialize)]
struct FileQuery { path: String }

async fn handle_file(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<FileQuery>,
) -> Response {
    if !check_auth(&s.token, &headers) { return unauthorized(); }

    // Canonicalize requested path
    let req_path = match std::path::Path::new(&q.path).canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // Security: allow only paths currently in the clipboard file list
    let allowed = s.last_clip.lock().unwrap().files.iter().any(|f| {
        std::path::Path::new(f).canonicalize()
            .map(|p| p == req_path)
            .unwrap_or(false)
    });
    if !allowed { return StatusCode::FORBIDDEN.into_response(); }

    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::GetFile { path: q.path.clone(), reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Some(data)) => {
            let mime = mime_for_ext(req_path.extension().and_then(|e| e.to_str()).unwrap_or(""));
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(data))
                .unwrap()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct VFileQuery { i: usize }

async fn handle_vfile(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<VFileQuery>,
) -> Response {
    if !check_auth(&s.token, &headers) { return unauthorized(); }

    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::GetVFile { index: q.i, reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Some(data)) => {
            let fname = s.last_clip.lock().unwrap()
                .vfiles.get(q.i).cloned()
                .unwrap_or_else(|| format!("file_{}", q.i));
            let mime = mime_for_ext(
                std::path::Path::new(&fname)
                    .extension().and_then(|e| e.to_str()).unwrap_or("")
            );
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header("Content-Disposition", format!("attachment; filename=\"{fname}\""))
                .body(Body::from(data))
                .unwrap()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "png"           => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif"           => "image/gif",
        "bmp"           => "image/bmp",
        "webp"          => "image/webp",
        "pdf"           => "application/pdf",
        "txt"           => "text/plain",
        "html" | "htm" => "text/html",
        "csv"           => "text/csv",
        "zip"           => "application/zip",
        "docx"          => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx"          => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx"          => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _               => "application/octet-stream",
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipd=info".into()),
        )
        .init();

    let args = Args::parse();

    if !args.bind_localhost_only && args.token.is_none() && !args.allow_no_token {
        bail!(
            "Refusing to start without a token.\n\
             Use --token <secret>, set CLIPD_TOKEN, or pass --allow-no-token."
        );
    }

    // Single-instance guard (Windows only)
    #[cfg(windows)]
    let _mutex = unsafe { win_clip::acquire_mutex() };

    // Spawn dedicated STA thread for all clipboard operations
    let (clip_tx, clip_rx) = mpsc::sync_channel::<ClipRequest>(32);
    thread::Builder::new()
        .name("clipboard-sta".into())
        .spawn(move || {
            #[cfg(windows)]    win_clip::sta_loop(clip_rx);
            #[cfg(not(windows))] { drop(clip_rx); }
        })?;

    let state = AppState {
        clip_tx,
        token:     args.token.clone(),
        last_clip: Arc::new(Mutex::new(LastClip::default())),
    };

    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/",       get(handle_clip))
        .route("/clip",   get(handle_clip))
        .route("/file",   get(handle_file))
        .route("/vfile",  get(handle_vfile))
        .with_state(state.clone());

    // Determine bind addresses
    let localhost = SocketAddr::from(([127, 0, 0, 1], args.port));

    if args.bind_localhost_only {
        info!("Listening on http://{localhost}");
        let listener = tokio::net::TcpListener::bind(localhost).await?;
        axum::serve(listener, app).await?;
    } else {
        match find_tailscale_ip() {
            Some(ts_ip) => {
                let ts_addr = SocketAddr::from((ts_ip, args.port));
                info!("Listening on http://{localhost}");
                info!("Listening on http://{ts_addr}  (Tailscale)");

                // Localhost listener in background
                let app2 = app.clone();
                tokio::spawn(async move {
                    let listener = tokio::net::TcpListener::bind(localhost).await
                        .expect("bind localhost");
                    axum::serve(listener, app2).await.ok();
                });

                let listener = tokio::net::TcpListener::bind(ts_addr).await?;
                axum::serve(listener, app).await?;
            }
            None => {
                warn!("Tailscale IP not found; falling back to localhost-only");
                info!("Listening on http://{localhost}");
                let listener = tokio::net::TcpListener::bind(localhost).await?;
                axum::serve(listener, app).await?;
            }
        }
    }

    Ok(())
}
