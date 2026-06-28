//! clipwire — Tailscale 越しに Windows クリップボードを操作するツール
//!
//! サーバー起動 (Windows):
//!   clipwire serve --token <secret>
//!
//! クリップボード取得 (Linux/Mac/Windows):
//!   clipwire get
//!   clipwire get -q          # quiet: パス/本文だけ出力
//!   clipwire get -w          # Wayland にも書き込む (Linux)
//!   clipwire get -d ~/pics   # 画像保存先を指定
//!
//! クリップボードに書き込み:
//!   echo "hello" | clipwire put
//!   wl-paste | clipwire put

use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::{info, warn};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "clipwire", about = "Tailscale 越しに Windows クリップボードを操作する")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Windows クリップボード HTTP サーバーを起動 (Windows 推奨)
    Serve(ServeArgs),
    /// Windows クリップボードの内容を取得して出力
    Get(GetArgs),
    /// stdin の内容を Windows クリップボードに書き込む
    Put(PutArgs),
    /// Windows のデフォルトブラウザで Web サービスを開く
    Open(OpenArgs),
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// 待ち受けポート
    #[arg(long, default_value = "9999")]
    port: u16,

    /// Bearer トークン (または CLIPD_TOKEN 環境変数)
    #[arg(long, env = "CLIPD_TOKEN")]
    token: Option<String>,

    /// localhost のみにバインド (Tailscale IP をスキップ)
    #[arg(long)]
    bind_localhost_only: bool,

    /// トークンなしで tailnet に公開することを明示許可
    #[arg(long)]
    allow_no_token: bool,
}

#[derive(Args, Debug)]
struct GetArgs {
    /// パスや URL だけを出力 (quiet モード)
    #[arg(short, long)]
    quiet: bool,

    /// 画像・大容量テキストの保存先ディレクトリ
    #[arg(short = 'd', long)]
    dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct PutArgs;

#[derive(Args, Debug)]
struct OpenArgs {
    /// 開くサービス
    #[arg(value_enum)]
    target: OpenTarget,
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum OpenTarget {
    /// ChatGPT (https://chatgpt.com)
    Chatgpt,
    /// Claude AI (https://claude.ai)
    Claude,
    /// Tailscale 管理画面 (https://login.tailscale.com/admin)
    Tailscale,
}

impl OpenTarget {
    fn url(&self) -> &'static str {
        match self {
            Self::Chatgpt   => "https://chatgpt.com",
            Self::Claude    => "https://claude.ai",
            Self::Tailscale => "https://login.tailscale.com/admin",
        }
    }
    fn as_str(&self) -> &'static str {
        match self {
            Self::Chatgpt   => "chatgpt",
            Self::Claude    => "claude",
            Self::Tailscale => "tailscale",
        }
    }
}

// ── Client config ─────────────────────────────────────────────────────────────

struct ClientConfig {
    host:  String,
    port:  u16,
    token: Option<String>,
}

impl ClientConfig {
    fn from_env() -> Result<Self> {
        let host = std::env::var("CLIPD_HOST")
            .context("CLIPD_HOST が設定されていません (例: export CLIPD_HOST=my-windows)")?;
        let port = std::env::var("CLIPD_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9999u16);
        let token = std::env::var("CLIPD_TOKEN").ok();
        Ok(Self { host, port, token })
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    fn set_auth<'a>(&self, req: ureq::Request) -> ureq::Request {
        if let Some(token) = &self.token {
            req.set("Authorization", &format!("Bearer {}", token))
        } else {
            req
        }
    }
}

// ── Client: get ───────────────────────────────────────────────────────────────

fn cmd_get(cfg: &ClientConfig, args: &GetArgs) -> Result<()> {
    let url  = format!("{}/clip", cfg.base_url());
    let req  = cfg.set_auth(ureq::get(&url).timeout(Duration::from_secs(10)));

    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(code, r)) => {
            bail!("HTTP {}: {}", code, r.into_string().unwrap_or_default().trim().to_string())
        }
        Err(e) => bail!("{} に接続できません: {}", cfg.base_url(), e),
    };

    let kind = resp.header("X-Clip-Kind").unwrap_or("").to_string();
    let mut body = Vec::new();
    resp.into_reader().read_to_end(&mut body)?;

    match kind.as_str() {
        "image" => {
            let path = save_file(&body, ".png", args.dir.as_deref())?;
            if args.quiet { println!("{}", path.display()); }
            else          { println!("画像を保存しました: {}", path.display()); }
        }

        "text" | "html" | "rtf" => {
            let (suffix, label) = match kind.as_str() {
                "html" => (".html", "HTML"),
                "rtf"  => (".rtf",  "RTF"),
                _      => (".txt",  "テキスト"),
            };
            if body.len() > 1024 {
                let path = save_file(&body, suffix, args.dir.as_deref())?;
                if args.quiet { println!("{}", path.display()); }
                else          { println!("{} を保存しました (大容量): {}", label, path.display()); }
            } else {
                io::stdout().write_all(&body)?;
                if !args.quiet { println!(); }
            }
        }

        "url" => {
            io::stdout().write_all(&body)?;
            if !args.quiet { println!(); }
        }

        "audio" => {
            let path = save_file(&body, ".wav", args.dir.as_deref())?;
            if args.quiet { println!("{}", path.display()); }
            else          { println!("音声を保存しました: {}", path.display()); }
        }

        "files" => {
            let paths: Vec<String> = serde_json::from_slice(&body)?;
            if paths.is_empty() {
                if !args.quiet { println!("クリップボードにファイルがありません。"); }
                return Ok(());
            }
            let auth = cfg.token.as_ref()
                .map(|t| format!("-H 'Authorization: Bearer {}' ", t))
                .unwrap_or_default();
            if !args.quiet { println!("クリップボード: ファイル {}件\n", paths.len()); }
            for win_path in &paths {
                let enc   = percent_encode(win_path);
                let fname = win_fname(win_path);
                let cmd   = format!("curl -fsSL {}'{}/file?path={}' -o '{}'",
                                    auth, cfg.base_url(), enc, fname);
                if args.quiet { println!("{cmd}"); }
                else          { println!("  {win_path}\n  → {cmd}\n"); }
            }
        }

        "vfiles" => {
            let names: Vec<String> = serde_json::from_slice(&body)?;
            if names.is_empty() {
                if !args.quiet { println!("仮想ファイルがありません。"); }
                return Ok(());
            }
            let auth = cfg.token.as_ref()
                .map(|t| format!("-H 'Authorization: Bearer {}' ", t))
                .unwrap_or_default();
            if !args.quiet { println!("クリップボード: 仮想ファイル {}件 (Outlook 添付等)\n", names.len()); }
            for (i, name) in names.iter().enumerate() {
                let cmd = format!("curl -fsSL {}'{}/vfile?i={}' -o '{}'",
                                  auth, cfg.base_url(), i, name);
                if args.quiet { println!("{cmd}"); }
                else          { println!("  [{i}] {name}\n  → {cmd}\n"); }
            }
        }

        "empty" | "" => { /* サイレント */ }

        "error" => bail!("clipwire error: {}", String::from_utf8_lossy(&body).trim()),

        other => {
            eprintln!("unknown kind: {other}");
            io::stdout().write_all(&body)?;
        }
    }

    Ok(())
}

// ── Client: put ───────────────────────────────────────────────────────────────

fn cmd_put(cfg: &ClientConfig) -> Result<()> {
    let mut body = Vec::new();
    io::stdin().read_to_end(&mut body)?;

    let url = format!("{}/clip", cfg.base_url());
    let req = cfg.set_auth(
        ureq::post(&url)
            .set("Content-Type", "text/plain; charset=utf-8")
            .timeout(Duration::from_secs(30)),
    );

    match req.send_bytes(&body) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(code, r)) => {
            bail!("HTTP {}: {}", code, r.into_string().unwrap_or_default().trim().to_string())
        }
        Err(e) => bail!("{} への送信に失敗: {}", cfg.base_url(), e),
    }
}

// ── Client: open ──────────────────────────────────────────────────────────────

fn cmd_open(cfg: &ClientConfig, args: &OpenArgs) -> Result<()> {
    let url = format!("{}/open?name={}", cfg.base_url(), args.target.as_str());
    let req = cfg.set_auth(ureq::get(&url).timeout(Duration::from_secs(10)));
    match req.call() {
        Ok(_) => { println!("Windows ブラウザで {} を開きました", args.target.url()); Ok(()) }
        Err(ureq::Error::Status(401, _)) => bail!("Unauthorized (CLIPD_TOKEN を確認)"),
        Err(ureq::Error::Status(code, r)) => {
            bail!("HTTP {}: {}", code, r.into_string().unwrap_or_default().trim().to_string())
        }
        Err(e) => bail!("{} に接続できません: {}", cfg.base_url(), e),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn save_file(data: &[u8], suffix: &str, dir: Option<&Path>) -> Result<PathBuf> {
    let ts   = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let name = format!("clipwire_{}{}", ts, suffix);
    let path = match dir {
        Some(d) => { std::fs::create_dir_all(d)?; d.join(&name) }
        None    => std::env::temp_dir().join(&name),
    };
    std::fs::write(&path, data)?;
    Ok(path)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn win_fname(win_path: &str) -> String {
    let norm = win_path.replace('\\', "/");
    Path::new(&norm)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string())
}

// ── Domain types (serve) ──────────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)]
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

#[allow(dead_code)]
enum ClipRequest {
    GetClip  { reply: oneshot::Sender<ClipKind> },
    SetClip  { text: String,  reply: oneshot::Sender<Result<()>> },
    GetFile  { path: String,  reply: oneshot::Sender<Option<Vec<u8>>> },
    GetVFile { index: usize,  reply: oneshot::Sender<Option<Vec<u8>>> },
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
                    CoInitializeEx, CoUninitialize, IStream,
                    COINIT_APARTMENTTHREADED, DVASPECT_CONTENT, FORMATETC,
                    TYMED_HGLOBAL, TYMED_ISTREAM,
                },
                DataExchange::{
                    CloseClipboard, EmptyClipboard, GetClipboardData,
                    IsClipboardFormatAvailable, OpenClipboard,
                    RegisterClipboardFormatW, SetClipboardData,
                },
                Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
                Ole::{OleGetClipboard, OleInitialize, ReleaseStgMedium},
                Threading::CreateMutexW,
            },
            UI::Shell::{DragQueryFileW, HDROP},
        },
        core::w,
    };

    const CF_DIB:         u32 = 8;
    const CF_WAVE:        u32 = 12;
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP:       u32 = 15;

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

    unsafe fn hglobal_bytes(h: HANDLE) -> Vec<u8> {
        let hg   = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        let ptr  = GlobalLock(hg);
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _    = GlobalUnlock(hg);
        data
    }

    pub unsafe fn read_clipboard() -> ClipKind {
        let fmt_fgd  = reg_fmt(w!("FileGroupDescriptorW"));
        let fmt_fc   = reg_fmt(w!("FileContents"));
        let fmt_html = reg_fmt(w!("HTML Format"));
        let fmt_url  = reg_fmt(w!("UniformResourceLocatorW"));
        let fmt_rtf  = reg_fmt(w!("Rich Text Format"));

        if fmt_avail(CF_DIB)  { if let Ok(d) = read_image()            { return ClipKind::Image(d); } }
        if fmt_avail(CF_HDROP){ if let Ok(v) = read_files() { if !v.is_empty() { return ClipKind::Files(v); } } }
        if fmt_avail(fmt_fgd) && fmt_avail(fmt_fc) {
            if let Ok(v) = read_vfile_names() { if !v.is_empty() { return ClipKind::VFiles(v); } }
        }
        if fmt_avail(CF_WAVE)   { if let Ok(d) = read_wave()            { return ClipKind::Audio(d); } }
        if fmt_avail(fmt_url)   { if let Ok(u) = read_url(fmt_url)      { return ClipKind::Url(u);   } }
        if fmt_avail(fmt_html)  { if let Ok(h) = read_html(fmt_html)    { return ClipKind::Html(h);  } }
        if fmt_avail(fmt_rtf)   { if let Ok(r) = read_rtf(fmt_rtf)      { return ClipKind::Rtf(r);   } }
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
        bmp.extend_from_slice(&0u32.to_le_bytes());
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
        let hdrop = HDROP(h.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            paths.push(String::from_utf16_lossy(&buf[..len]));
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
        const ENTRY: usize = 592;
        const NAME:  usize = 72;
        let mut names = Vec::with_capacity(count);
        for i in 0..count {
            let base = 4 + i * ENTRY;
            if base + ENTRY > data.len() { break; }
            let wdata: &[u8] = &data[base + NAME .. base + NAME + 520];
            let words: Vec<u16> = wdata.chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
            names.push(String::from_utf16_lossy(&words[..end]));
        }
        Ok(names)
    }

    pub unsafe fn get_vfile_contents(index: usize) -> Result<Vec<u8>> {
        let fmt_fc = reg_fmt(w!("FileContents"));
        let _ = OleInitialize(None);

        let data_obj = OleGetClipboard()?;

        let mut fetc = FORMATETC {
            cfFormat: fmt_fc as u16,
            ptd:      std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex:   index as i32,
            tymed:    TYMED_ISTREAM.0 as u32,
        };

        if let Ok(mut stgm) = data_obj.GetData(&fetc) {
            if stgm.tymed == TYMED_ISTREAM.0 as u32 {
                let data = {
                    let stream: &Option<IStream> = &*stgm.u.pstm;
                    if let Some(s) = stream { drain_istream(s)? } else { bail!("null IStream") }
                };
                ReleaseStgMedium(&mut stgm);
                return Ok(data);
            }
            ReleaseStgMedium(&mut stgm);
        }

        fetc.tymed = TYMED_HGLOBAL.0 as u32;
        let mut stgm = data_obj.GetData(&fetc)?;
        let hg   = stgm.u.hGlobal;
        let size = GlobalSize(hg);
        let ptr  = GlobalLock(hg);
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _    = GlobalUnlock(hg);
        ReleaseStgMedium(&mut stgm);
        Ok(data)
    }

    unsafe fn drain_istream(stream: &IStream) -> Result<Vec<u8>> {
        let mut buf   = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let mut read = 0u32;
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
        let hdr  = String::from_utf8_lossy(&data[..data.len().min(512)]);
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
        let _g = ClipGuard::open()?;
        let h  = GetClipboardData(fmt).context("RTF")?;
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
        Ok(String::from_utf16_lossy(&words[..end]))
    }

    pub unsafe fn write_clipboard_text(text: &str) -> Result<()> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_len = wide.len() * 2;

        let hg  = GlobalAlloc(GMEM_MOVEABLE, byte_len)?;
        let ptr = GlobalLock(hg) as *mut u16;
        if ptr.is_null() { bail!("GlobalLock failed"); }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(hg);

        OpenClipboard(None)?;
        EmptyClipboard()?;
        if let Err(e) = SetClipboardData(CF_UNICODETEXT, HANDLE(hg.0)) {
            let _ = CloseClipboard();
            bail!("SetClipboardData: {e}");
        }
        CloseClipboard()?;
        Ok(())
    }

    pub fn show_balloon(msg: &str) {
        let msg = msg.to_string();
        std::thread::spawn(move || { let _ = show_toast(&msg); });
    }

    #[allow(unused_variables)]
    fn show_toast(msg: &str) -> Result<()> {
        std::process::Command::new("powershell")
            .args([
                "-NoProfile", "-WindowStyle", "Hidden", "-Command",
                &format!(
                    "[void][System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms');\
                     $n=New-Object System.Windows.Forms.NotifyIcon;\
                     $n.Icon=[System.Drawing.SystemIcons]::Information;\
                     $n.Visible=$true;\
                     $n.ShowBalloonTip(4000,'clipwire','{}',\
                     [System.Windows.Forms.ToolTipIcon]::Info);\
                     Start-Sleep 5;$n.Dispose()",
                    msg.replace('\'', "")
                ),
            ])
            .spawn()?;
        Ok(())
    }

    pub unsafe fn acquire_mutex() -> windows::Win32::Foundation::HANDLE {
        match CreateMutexW(None, true, w!("Global\\clipwire_singleton")) {
            Ok(h) => {
                if GetLastError() == WIN32_ERROR(183) {
                    eprintln!("clipwire serve は既に起動中です。");
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

    pub fn sta_loop(rx: std::sync::mpsc::Receiver<super::ClipRequest>) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            use super::ClipRequest;
            for req in rx {
                match req {
                    ClipRequest::GetClip  { reply }       => { let _ = reply.send(read_clipboard()); }
                    ClipRequest::SetClip  { text, reply } => { let _ = reply.send(write_clipboard_text(&text)); }
                    ClipRequest::GetFile  { path, reply } => { let _ = reply.send(std::fs::read(&path).ok()); }
                    ClipRequest::GetVFile { index, reply }=> { let _ = reply.send(get_vfile_contents(index).ok()); }
                }
            }
            CoUninitialize();
        }
    }
}

// ── Network helpers (serve) ───────────────────────────────────────────────────

fn find_tailscale_ip() -> Option<Ipv4Addr> {
    if let Ok(out) = std::process::Command::new("tailscale").args(["ip", "-4"]).output() {
        if out.status.success() {
            if let Ok(ip) = String::from_utf8_lossy(&out.stdout).trim().parse::<Ipv4Addr>() {
                return Some(ip);
            }
        }
    }
    if let Ok(out) = std::process::Command::new("ipconfig").output() {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            let line = line.trim();
            if line.starts_with("IPv4") || line.contains("IP Address") {
                if let Some(part) = line.split(':').nth(1) {
                    if let Ok(ip) = part.trim().parse::<Ipv4Addr>() {
                        if is_cgnat(ip) { return Some(ip); }
                    }
                }
            }
        }
    }
    None
}

fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xC0) == 0x40
}

// ── Auth (serve) ──────────────────────────────────────────────────────────────

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

// ── HTTP handlers (serve) ─────────────────────────────────────────────────────

async fn handle_health() -> &'static str { "OK\n" }

async fn handle_clip(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !check_auth(&s.token, &headers) { return unauthorized(); }

    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::GetClip { reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let Ok(kind) = rx.await else { return StatusCode::INTERNAL_SERVER_ERROR.into_response(); };

    match kind {
        ClipKind::Image(data)  => { s.last_clip.lock().unwrap().files.clear(); clip_resp("image", "image/png", data) }
        ClipKind::Files(paths) => { let j = serde_json::to_vec(&paths).unwrap(); s.last_clip.lock().unwrap().files = paths; clip_resp("files", "application/json", j) }
        ClipKind::VFiles(names)=> { let j = serde_json::to_vec(&names).unwrap(); s.last_clip.lock().unwrap().vfiles = names; clip_resp("vfiles", "application/json", j) }
        ClipKind::Audio(data)  => clip_resp("audio", "audio/wav",                  data),
        ClipKind::Html(html)   => clip_resp("html",  "text/html; charset=utf-8",   html.into_bytes()),
        ClipKind::Url(url)     => clip_resp("url",   "text/plain; charset=utf-8",  url.into_bytes()),
        ClipKind::Rtf(rtf)     => clip_resp("rtf",   "text/rtf",                   rtf.into_bytes()),
        ClipKind::Text(text)   => clip_resp("text",  "text/plain; charset=utf-8",  text.into_bytes()),
        ClipKind::Empty => {
            #[cfg(windows)] win_clip::show_balloon("クリップボードが空です");
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

async fn handle_clip_post(State(s): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if !check_auth(&s.token, &headers) { return unauthorized(); }
    let text = match String::from_utf8(body.to_vec()) {
        Ok(t) => t,
        Err(_) => return (StatusCode::BAD_REQUEST, "Body must be UTF-8\n").into_response(),
    };
    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::SetClip { text, reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Ok(())) => (StatusCode::NO_CONTENT, "").into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
        Err(_)     => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)] struct FileQuery  { path: String }
#[derive(Deserialize)] struct VFileQuery { i: usize }
#[derive(Deserialize)] struct OpenQuery  { name: String }

async fn handle_open(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<OpenQuery>) -> Response {
    if !check_auth(&s.token, &headers) { return unauthorized(); }
    let url = match q.name.as_str() {
        "chatgpt"   => "https://chatgpt.com",
        "claude"    => "https://claude.ai",
        "tailscale" => "https://login.tailscale.com/admin",
        other => return (StatusCode::BAD_REQUEST, format!("unknown target: {other}\n")).into_response(),
    };
    #[cfg(windows)]
    if let Err(e) = std::process::Command::new("cmd").args(["/c", "start", "", url]).spawn() {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("ブラウザを開けませんでした: {e}\n")).into_response();
    }
    (StatusCode::OK, format!("{url}\n")).into_response()
}

async fn handle_file(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<FileQuery>) -> Response {
    if !check_auth(&s.token, &headers) { return unauthorized(); }
    let req_path = match std::path::Path::new(&q.path).canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let allowed = s.last_clip.lock().unwrap().files.iter().any(|f| {
        std::path::Path::new(f).canonicalize().map(|p| p == req_path).unwrap_or(false)
    });
    if !allowed { return StatusCode::FORBIDDEN.into_response(); }
    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::GetFile { path: q.path, reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Some(data)) => {
            let mime = mime_for_ext(req_path.extension().and_then(|e| e.to_str()).unwrap_or(""));
            Response::builder().status(StatusCode::OK).header(header::CONTENT_TYPE, mime).body(Body::from(data)).unwrap()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn handle_vfile(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<VFileQuery>) -> Response {
    if !check_auth(&s.token, &headers) { return unauthorized(); }
    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::GetVFile { index: q.i, reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Some(data)) => {
            let fname = s.last_clip.lock().unwrap().vfiles.get(q.i).cloned()
                .unwrap_or_else(|| format!("file_{}", q.i));
            let mime  = mime_for_ext(std::path::Path::new(&fname).extension().and_then(|e| e.to_str()).unwrap_or(""));
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header("Content-Disposition", format!("attachment; filename=\"{fname}\""))
                .body(Body::from(data)).unwrap()
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

// ── serve entry point ─────────────────────────────────────────────────────────

async fn run_serve(args: ServeArgs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipwire=info".into()),
        )
        .init();

    if !args.bind_localhost_only && args.token.is_none() && !args.allow_no_token {
        bail!(
            "トークンなしでの起動を拒否しました。\n\
             --token <secret>、CLIPD_TOKEN 環境変数、または --allow-no-token を指定してください。"
        );
    }

    #[cfg(windows)]
    let _mutex = unsafe { win_clip::acquire_mutex() };

    let (clip_tx, clip_rx) = mpsc::sync_channel::<ClipRequest>(32);
    thread::Builder::new()
        .name("clipboard-sta".into())
        .spawn(move || {
            #[cfg(windows)]      win_clip::sta_loop(clip_rx);
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
        .route("/clip",   get(handle_clip).post(handle_clip_post))
        .route("/file",   get(handle_file))
        .route("/vfile",  get(handle_vfile))
        .route("/open",   get(handle_open))
        .with_state(state.clone());

    let localhost = SocketAddr::from(([127, 0, 0, 1], args.port));

    if args.bind_localhost_only {
        info!("Listening on http://{localhost}");
        axum::serve(tokio::net::TcpListener::bind(localhost).await?, app).await?;
    } else {
        match find_tailscale_ip() {
            Some(ts_ip) => {
                let ts_addr = SocketAddr::from((ts_ip, args.port));
                info!("Listening on http://{localhost}");
                info!("Listening on http://{ts_addr}  (Tailscale)");
                let app2 = app.clone();
                tokio::spawn(async move {
                    if let Ok(l) = tokio::net::TcpListener::bind(localhost).await {
                        axum::serve(l, app2).await.ok();
                    }
                });
                axum::serve(tokio::net::TcpListener::bind(ts_addr).await?, app).await?;
            }
            None => {
                warn!("Tailscale IP not found; falling back to localhost-only");
                info!("Listening on http://{localhost}");
                axum::serve(tokio::net::TcpListener::bind(localhost).await?, app).await?;
            }
        }
    }
    Ok(())
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve(args) => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(run_serve(args))
        }
        Cmd::Get(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_get(&cfg, &args)
        }
        Cmd::Put(_) => {
            let cfg = ClientConfig::from_env()?;
            cmd_put(&cfg)
        }
        Cmd::Open(args) => {
            let cfg = ClientConfig::from_env()?;
            cmd_open(&cfg, &args)
        }
    }
}
