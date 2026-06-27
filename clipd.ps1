<#
.SYNOPSIS
    clipd - Windows クリップボードの内容を HTTP で返す軽量サービス

.DESCRIPTION
    Tailscale 網内認証を前提とし、localhost と Tailscale IP にのみバインドする。
    リモート (tssh でログインした Linux サーバ等) から同じ tailnet 経由で叩くと、
    現在の Windows クリップボードの内容を返す。

    クリップボードの種別を自動判別して返す:
      - 画像 (ビットマップ)         -> 200  image/png
      - 実ファイル (CF_HDROP)       -> 200  application/json  (Windows パスの配列)
      - 仮想ファイル (Outlook 添付等)-> 200  application/json  (ファイル名の配列)
      - 音声 (CF_WAVE)              -> 200  audio/wav
      - HTML (HTML Format)          -> 200  text/html
      - URL                         -> 200  text/plain
      - RTF (Rich Text Format)      -> 200  text/rtf
      - テキスト                    -> 200  text/plain
      - 空                          -> 200  text/plain (空文字列)

    種別はレスポンスヘッダ X-Clip-Kind で返す:
      image | files | vfiles | audio | html | url | rtf | text | empty

    ルーティング:
      GET /             自動判別
      GET /clip         自動判別 (/ と同じ)
      GET /file?path=   CF_HDROP ファイルの実体 (クリップボード照合あり)
      GET /vfile?i=N    仮想ファイルの実体 (インデックス指定)
      GET /health       死活確認
      それ以外          404

.PARAMETER Port
    待ち受けポート。既定 9999。

.PARAMETER Token
    指定すると Authorization: Bearer <token> を要求する。
    環境変数 CLIPD_TOKEN でも渡せる。
    Tailscale へ公開する場合は token を強く推奨 (下記ガード参照)。

.PARAMETER BindLocalhostOnly
    Tailscale IP を使わず localhost だけで起動する。

.PARAMETER AllowNoToken
    Tailscale へ公開しつつ token なしで起動することを明示的に許可する。
    付けないと、token なし & 非 localhost-only での起動は拒否される。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File clipd.ps1 -Token mysecret
    powershell -ExecutionPolicy Bypass -File clipd.ps1 -BindLocalhostOnly
    powershell -ExecutionPolicy Bypass -File clipd.ps1 -AllowNoToken
#>

[CmdletBinding()]
param(
    [int]$Port = 9999,
    [string]$Token = $env:CLIPD_TOKEN,
    [switch]$BindLocalhostOnly,
    [switch]$AllowNoToken
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

# ---- 仮想ファイル (Outlook 添付等) 取得ヘルパ ----------------------------------
# CF_HDROP にパスが存在しない仮想ファイルを COM IDataObject 経由で取得する。
# Add-Type はプロセス AppDomain に型をロードするため、STA ランスペースでも参照可能。
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
using System.Windows.Forms;

public static class VirtualFileHelper {
    [DllImport("user32.dll", CharSet = CharSet.Auto)]
    private static extern ushort RegisterClipboardFormat(string name);
    [DllImport("ole32.dll")]
    private static extern void ReleaseStgMedium(ref STGMEDIUM p);
    [DllImport("kernel32.dll")]
    private static extern IntPtr GlobalLock(IntPtr h);
    [DllImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GlobalUnlock(IntPtr h);
    [DllImport("kernel32.dll")]
    private static extern UIntPtr GlobalSize(IntPtr h);

    public static bool HasVirtualFiles() {
        var d = Clipboard.GetDataObject();
        return d != null
            && (d.GetDataPresent("FileGroupDescriptorW") || d.GetDataPresent("FileGroupDescriptor"))
            && d.GetDataPresent("FileContents");
    }

    public static string[] GetFileNames() {
        var com = Clipboard.GetDataObject() as System.Runtime.InteropServices.ComTypes.IDataObject;
        if (com == null) return null;
        foreach (var fmt in new[] { "FileGroupDescriptorW", "FileGroupDescriptor" }) {
            bool uni = fmt == "FileGroupDescriptorW";
            var fe = new FORMATETC();
            fe.cfFormat = (short)RegisterClipboardFormat(fmt);
            fe.ptd      = IntPtr.Zero;
            fe.dwAspect = DVASPECT.DVASPECT_CONTENT;
            fe.lindex   = -1;
            fe.tymed    = TYMED.TYMED_HGLOBAL;
            var sm = new STGMEDIUM();
            try {
                com.GetData(ref fe, out sm);
                if (sm.tymed == TYMED.TYMED_HGLOBAL) return ParseFGD(sm.unionmember, uni);
            } catch { }
            finally { ReleaseStgMedium(ref sm); }
        }
        return null;
    }

    public static byte[] GetFileContents(int index) {
        var com = Clipboard.GetDataObject() as System.Runtime.InteropServices.ComTypes.IDataObject;
        if (com == null) return null;
        var fe = new FORMATETC();
        fe.cfFormat = (short)RegisterClipboardFormat("FileContents");
        fe.ptd      = IntPtr.Zero;
        fe.dwAspect = DVASPECT.DVASPECT_CONTENT;
        fe.lindex   = index;
        fe.tymed    = TYMED.TYMED_ISTREAM | TYMED.TYMED_HGLOBAL;
        var sm = new STGMEDIUM();
        try {
            com.GetData(ref fe, out sm);
            if (sm.tymed == TYMED.TYMED_ISTREAM) {
                var ist = (System.Runtime.InteropServices.ComTypes.IStream)
                    Marshal.GetObjectForIUnknown(sm.unionmember);
                return DrainIStream(ist);
            }
            if (sm.tymed == TYMED.TYMED_HGLOBAL) return ReadHGlobal(sm.unionmember);
        } catch { }
        finally { ReleaseStgMedium(ref sm); }
        return null;
    }

    private static string[] ParseFGD(IntPtr hGlobal, bool unicode) {
        // FILEDESCRIPTORW: 72 bytes metadata + MAX_PATH(260) WCHAR = 592 bytes/entry
        // FILEDESCRIPTORA: 72 bytes metadata + MAX_PATH(260) CHAR  = 332 bytes/entry
        IntPtr ptr = GlobalLock(hGlobal);
        try {
            int count    = Marshal.ReadInt32(ptr);
            int entSize  = unicode ? 592 : 332;
            int nameOff  = 72;
            var names = new string[count];
            for (int i = 0; i < count; i++) {
                IntPtr p = IntPtr.Add(ptr, 4 + i * entSize + nameOff);
                names[i] = unicode
                    ? Marshal.PtrToStringUni(p, 260).TrimEnd('\0')
                    : Marshal.PtrToStringAnsi(p, 260).TrimEnd('\0');
            }
            return names;
        } finally { GlobalUnlock(hGlobal); }
    }

    private static byte[] DrainIStream(System.Runtime.InteropServices.ComTypes.IStream ist) {
        var ms  = new MemoryStream();
        var buf = new byte[65536];
        var np  = Marshal.AllocHGlobal(IntPtr.Size);
        try {
            while (true) {
                ist.Read(buf, buf.Length, np);
                int n = IntPtr.Size == 8 ? (int)Marshal.ReadInt64(np) : Marshal.ReadInt32(np);
                if (n <= 0) break;
                ms.Write(buf, 0, n);
            }
        } finally { Marshal.FreeHGlobal(np); }
        return ms.ToArray();
    }

    private static byte[] ReadHGlobal(IntPtr hGlobal) {
        IntPtr ptr = GlobalLock(hGlobal);
        try {
            int size = (int)GlobalSize(hGlobal).ToUInt64();
            var data = new byte[size];
            Marshal.Copy(ptr, data, 0, size);
            return data;
        } finally { GlobalUnlock(hGlobal); }
    }
}
'@ -ReferencedAssemblies 'System.Windows.Forms'

$ErrorActionPreference = 'Stop'

# ---- token なしで tailnet 公開しようとしたら止める --------------------------
# localhost only なら token なしでよい。Tailscale 公開時は token を要求し、
# どうしても token なしにするなら -AllowNoToken を明示させる。
if (-not $Token -and -not $AllowNoToken -and -not $BindLocalhostOnly) {
    Write-Host "Refusing to expose clipboard on Tailscale without a token." -ForegroundColor Red
    Write-Host "Use -Token <secret>, or -BindLocalhostOnly, or -AllowNoToken to override." -ForegroundColor Red
    exit 1
}

# ---- 多重起動防止 (名前付き Mutex) -------------------------------------------
$mutexName = "Global\clipd_$Port"
$createdNew = $false
$mutex = New-Object System.Threading.Mutex($true, $mutexName, [ref]$createdNew)
if (-not $createdNew) {
    Write-Host "clipd is already running on port $Port. Exiting." -ForegroundColor Yellow
    exit 0
}

# ---- Tailscale IP の検出 -----------------------------------------------------
# まず `tailscale ip -4` を信頼し、だめなら InterfaceAlias が Tailscale* の
# CGNAT 帯 (100.64.0.0/10) を拾う。素の 100.x への無条件 bind は避ける。
function Get-TailscaleIPv4 {
    try {
        $ts = Get-Command tailscale.exe -ErrorAction SilentlyContinue
        if (-not $ts) {
            $cand = Join-Path $env:ProgramFiles 'Tailscale\tailscale.exe'
            if (Test-Path $cand) { $ts = Get-Command $cand }
        }
        if ($ts) {
            $ip = & $ts.Source ip -4 2>$null | Select-Object -First 1
            if ($ip) { return $ip.Trim() }
        }
    } catch { }

    try {
        $addr = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
            Where-Object {
                $_.InterfaceAlias -like 'Tailscale*' -and
                $_.IPAddress -match '^100\.(6[4-9]|[7-9]\d|1[01]\d|12[0-7])\.'
            } | Select-Object -First 1
        if ($addr) { return $addr.IPAddress }
    } catch { }

    return $null
}

# ---- バインド先 prefix を組み立てる -----------------------------------------
$prefixes = @("http://127.0.0.1:$Port/")
$tsIP = $null
if ($BindLocalhostOnly) {
    Write-Host "Binding to localhost only by option." -ForegroundColor Yellow
} else {
    $tsIP = Get-TailscaleIPv4
    if ($tsIP) {
        $prefixes += "http://$($tsIP):$Port/"
        Write-Host "Tailscale IP detected: $tsIP" -ForegroundColor Green
    } else {
        Write-Host "Tailscale IP not found. Binding to localhost only." -ForegroundColor Yellow
    }
}

if ($Token) {
    Write-Host "Bearer token auth is ENABLED." -ForegroundColor Green
} else {
    Write-Host "Bearer token auth is disabled." -ForegroundColor DarkGray
}

$listener = New-Object System.Net.HttpListener
foreach ($p in $prefixes) { $listener.Prefixes.Add($p) }

try {
    $listener.Start()
} catch {
    Write-Host "Failed to start HttpListener: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host "Tailscale IP への bind は権限が要る場合があります。管理者で一度:" -ForegroundColor Red
    if ($tsIP) {
        $u = "$env:USERDOMAIN\$env:USERNAME"
        Write-Host "  netsh http add urlacl url=http://$($tsIP):$Port/ user=`"$u`"" -ForegroundColor Cyan
    }
    $mutex.ReleaseMutex()
    exit 1
}

Write-Host "clipd listening on:" -ForegroundColor Green
foreach ($p in $prefixes) { Write-Host "  $p" }
Write-Host "Press Ctrl+C to stop." -ForegroundColor DarkGray

# ---- STA で scriptblock を実行 (例外時も finally で確実に後始末) -------------
function Invoke-InSTA {
    param([scriptblock]$Script, [object[]]$Arguments = @())
    $ps = $null
    $rs = $null
    try {
        $ps = [PowerShell]::Create()
        $rs = [RunspaceFactory]::CreateRunspace()
        $rs.ApartmentState = 'STA'
        $rs.ThreadOptions = 'ReuseThread'
        $rs.Open()
        $ps.Runspace = $rs
        [void]$ps.AddScript($Script)
        foreach ($a in $Arguments) { [void]$ps.AddArgument($a) }
        $result = $ps.Invoke()
        if ($ps.HadErrors) {
            $err = $ps.Streams.Error | Select-Object -First 1
            if ($err) { throw $err.Exception }
        }
        return $result
    }
    finally {
        if ($ps) { $ps.Dispose() }
        if ($rs) { $rs.Close(); $rs.Dispose() }
    }
}

# ---- クリップボード読み取り (全種別対応 + retry) ------------------------------
function Read-Clipboard {
    $script = {
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        for ($i = 0; $i -lt 5; $i++) {
            try {
                # 1. 画像 (ビットマップ / スクリーンショット等)
                if ([System.Windows.Forms.Clipboard]::ContainsImage()) {
                    $img = [System.Windows.Forms.Clipboard]::GetImage()
                    $ms = New-Object System.IO.MemoryStream
                    try {
                        $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
                        return [pscustomobject]@{ Kind = 'image'; Bytes = $ms.ToArray() }
                    } finally {
                        if ($img) { $img.Dispose() }
                        if ($ms)  { $ms.Dispose() }
                    }
                }
                # 2. 実ファイル (Explorer でコピーした既存ファイル)
                elseif ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) {
                    $files = [System.Windows.Forms.Clipboard]::GetFileDropList()
                    $list = @()
                    foreach ($f in $files) { $list += $f }
                    return [pscustomobject]@{ Kind = 'files'; Files = $list }
                }
                # 3. 仮想ファイル (Outlook 添付 / SharePoint 等、Windows パスなし)
                elseif ([VirtualFileHelper]::HasVirtualFiles()) {
                    $names = [VirtualFileHelper]::GetFileNames()
                    return [pscustomobject]@{ Kind = 'vfiles'; Files = $names }
                }
                # 4. 音声 (CF_WAVE)
                elseif ([System.Windows.Forms.Clipboard]::ContainsAudio()) {
                    $stream = [System.Windows.Forms.Clipboard]::GetAudioStream()
                    $ms = New-Object System.IO.MemoryStream
                    try {
                        $stream.CopyTo($ms)
                        return [pscustomobject]@{ Kind = 'audio'; Bytes = $ms.ToArray() }
                    } finally {
                        if ($stream) { $stream.Dispose() }
                        if ($ms)     { $ms.Dispose() }
                    }
                }
                # 5. URL (アドレスバー / リンクのコピー)
                else {
                    $do = [System.Windows.Forms.Clipboard]::GetDataObject()
                    $url = $null
                    foreach ($fmt in @('UniformResourceLocatorW', 'UniformResourceLocator')) {
                        if ($do -and $do.GetDataPresent($fmt)) {
                            $raw = $do.GetData($fmt)
                            if ($raw -is [System.IO.Stream]) {
                                $enc  = if ($fmt -eq 'UniformResourceLocatorW') { [System.Text.Encoding]::Unicode } else { [System.Text.Encoding]::UTF8 }
                                $buf  = New-Object byte[] 4096
                                $n    = $raw.Read($buf, 0, $buf.Length)
                                $url  = $enc.GetString($buf, 0, $n).TrimEnd([char]0, [char]13, [char]10, ' ')
                            } elseif ($raw -is [string]) {
                                $url = $raw.Trim()
                            }
                            if ($url) { break }
                        }
                    }
                    if ($url -and ($url.StartsWith('http://') -or $url.StartsWith('https://'))) {
                        return [pscustomobject]@{ Kind = 'url'; Text = $url }
                    }
                    # 6. HTML (ブラウザ / Office からのリッチコピー)
                    elseif ([System.Windows.Forms.Clipboard]::ContainsText([System.Windows.Forms.TextDataFormat]::Html)) {
                        $raw  = [System.Windows.Forms.Clipboard]::GetText([System.Windows.Forms.TextDataFormat]::Html)
                        # Windows HTML Format ヘッダを除去して純粋な HTML を返す
                        $html = $raw
                        if ($raw -match 'StartHTML:(\d+)') {
                            $off  = [int]$Matches[1]
                            $bytes = [System.Text.Encoding]::UTF8.GetBytes($raw)
                            if ($off -lt $bytes.Length) {
                                $html = [System.Text.Encoding]::UTF8.GetString($bytes, $off, $bytes.Length - $off)
                            }
                        }
                        return [pscustomobject]@{ Kind = 'html'; Text = $html }
                    }
                    # 7. RTF (Word / Wordpad 等)
                    elseif ([System.Windows.Forms.Clipboard]::ContainsText([System.Windows.Forms.TextDataFormat]::Rtf)) {
                        $rtf = [System.Windows.Forms.Clipboard]::GetText([System.Windows.Forms.TextDataFormat]::Rtf)
                        return [pscustomobject]@{ Kind = 'rtf'; Text = $rtf }
                    }
                    # 8. プレーンテキスト
                    elseif ([System.Windows.Forms.Clipboard]::ContainsText()) {
                        $t = [System.Windows.Forms.Clipboard]::GetText()
                        return [pscustomobject]@{ Kind = 'text'; Text = $t }
                    }
                    # 9. 空
                    else {
                        return [pscustomobject]@{ Kind = 'empty' }
                    }
                }
            } catch {
                if ($i -eq 4) { throw }
                Start-Sleep -Milliseconds 80
            }
        }
    }
    return (Invoke-InSTA -Script $script)[0]
}

# ---- レスポンス送信ヘルパ (HEAD は禁止済みなので body は常に書く) -----------
function Send-Bytes {
    param($Context, [byte[]]$Bytes, [string]$ContentType, [string]$Kind, [int]$Status = 200)
    $res = $Context.Response
    try {
        $res.StatusCode = $Status
        $res.Headers.Add('X-Clip-Kind', $Kind)
        $res.Headers.Add('Cache-Control', 'no-store')
        if ($ContentType) { $res.ContentType = $ContentType }
        if ($null -ne $Bytes -and $Bytes.Length -gt 0) {
            $res.ContentLength64 = $Bytes.Length
            $res.OutputStream.Write($Bytes, 0, $Bytes.Length)
        } else {
            $res.ContentLength64 = 0
        }
    } finally {
        $res.OutputStream.Close()
    }
}

function Send-Text {
    param($Context, [string]$Text, [string]$Kind, [int]$Status = 200, [string]$ContentType = 'text/plain; charset=utf-8')
    $bytes = [System.Text.Encoding]::UTF8.GetBytes([string]$Text)
    Send-Bytes -Context $Context -Bytes $bytes -ContentType $ContentType -Kind $Kind -Status $Status
}

function Send-Json {
    param($Context, [string]$Json, [string]$Kind, [int]$Status = 200)
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Json)
    Send-Bytes -Context $Context -Bytes $bytes -ContentType 'application/json; charset=utf-8' -Kind $Kind -Status $Status
}

# ---- メインループ ------------------------------------------------------------
try {
    while ($listener.IsListening) {
        $context = $listener.GetContext()
        $req = $context.Request
        try {
            # GET のみ許可 (HEAD は本文を返さない実装が要るため、単純に禁止)
            if ($req.HttpMethod -ne 'GET') {
                Send-Text -Context $context -Text 'method not allowed' -Kind 'error' -Status 405
                continue
            }

            # パス正規化
            $path = $req.Url.AbsolutePath.TrimEnd('/')
            if ($path -eq '') { $path = '/' }

            # health は認可前に通す
            if ($path -eq '/health') {
                Send-Text -Context $context -Text 'ok' -Kind 'health' -Status 200
                continue
            }

            # 既知パス以外は 404
            if ($path -ne '/' -and $path -ne '/clip' -and $path -ne '/file' -and $path -ne '/vfile') {
                Send-Text -Context $context -Text 'not found' -Kind 'error' -Status 404
                continue
            }

            # トークン認可 (指定時のみ)
            if ($Token) {
                $auth = $req.Headers['Authorization']
                if ($auth -ne "Bearer $Token") {
                    Send-Text -Context $context -Text 'unauthorized' -Kind 'error' -Status 401
                    continue
                }
            }

            # /file?path=<encoded>: クリップボードの FileDropList にあるファイルのみ返す
            if ($path -eq '/file') {
                # HttpListener は QueryString を自動デコードする
                $filePath = $req.QueryString['path']
                if (-not $filePath) {
                    Send-Text -Context $context -Text 'missing path parameter' -Kind 'error' -Status 400
                    continue
                }

                # セキュリティ: 今クリップボードにあるパスのみ許可
                $clip = Read-Clipboard
                if ($clip.Kind -ne 'files') {
                    Send-Text -Context $context -Text 'no files in clipboard' -Kind 'error' -Status 409
                    continue
                }
                $requestFull = [System.IO.Path]::GetFullPath($filePath)
                $allowed = $clip.Files | Where-Object {
                    [System.IO.Path]::GetFullPath($_) -eq $requestFull
                }
                if (-not $allowed) {
                    Send-Text -Context $context -Text 'path not in clipboard' -Kind 'error' -Status 403
                    continue
                }

                $fileName = [System.IO.Path]::GetFileName($filePath)
                $res = $context.Response
                try {
                    $res.StatusCode = 200
                    $res.Headers.Add('X-Clip-Kind', 'file')
                    $res.Headers.Add('X-Clip-Filename', $fileName)
                    $res.Headers.Add('Cache-Control', 'no-store')
                    $res.ContentType = 'application/octet-stream'
                    $stream = [System.IO.File]::OpenRead($filePath)
                    try {
                        $res.ContentLength64 = $stream.Length
                        $stream.CopyTo($res.OutputStream)
                    } finally {
                        $stream.Dispose()
                    }
                } finally {
                    $res.OutputStream.Close()
                }
                continue
            }

            # /vfile?i=N: 仮想ファイル (Outlook 添付等) の実体を返す
            if ($path -eq '/vfile') {
                $idxStr = $req.QueryString['i']
                $idx = 0
                if (-not [int]::TryParse($idxStr, [ref]$idx) -or $idx -lt 0) {
                    Send-Text -Context $context -Text 'invalid index' -Kind 'error' -Status 400
                    continue
                }
                $capturedIdx = $idx
                $vfile = (Invoke-InSTA -Script {
                    param($fileIndex)
                    Add-Type -AssemblyName System.Windows.Forms
                    $names = [VirtualFileHelper]::GetFileNames()
                    if (-not $names -or $fileIndex -ge $names.Length) { return $null }
                    $bytes = [VirtualFileHelper]::GetFileContents($fileIndex)
                    if (-not $bytes) { return $null }
                    return [pscustomobject]@{ Bytes = $bytes; Name = $names[$fileIndex] }
                } -Arguments @($capturedIdx))[0]

                if (-not $vfile) {
                    Send-Text -Context $context -Text 'virtual file not available' -Kind 'error' -Status 404
                    continue
                }
                $res = $context.Response
                try {
                    $res.StatusCode = 200
                    $res.Headers.Add('X-Clip-Kind', 'vfile')
                    $res.Headers.Add('X-Clip-Filename', $vfile.Name)
                    $res.Headers.Add('Cache-Control', 'no-store')
                    $res.ContentType = 'application/octet-stream'
                    $res.ContentLength64 = $vfile.Bytes.Length
                    $res.OutputStream.Write($vfile.Bytes, 0, $vfile.Bytes.Length)
                } finally {
                    $res.OutputStream.Close()
                }
                continue
            }

            # / or /clip: 自動判別して返す
            $clip = Read-Clipboard
            switch ($clip.Kind) {
                'image'  { Send-Bytes -Context $context -Bytes $clip.Bytes -ContentType 'image/png' -Kind 'image' }
                'files'  {
                    $json = if ($null -eq $clip.Files -or $clip.Files.Count -eq 0) { '[]' } else { ConvertTo-Json @($clip.Files) -Compress }
                    Send-Json -Context $context -Json $json -Kind 'files'
                }
                'vfiles' {
                    $json = if ($null -eq $clip.Files -or $clip.Files.Count -eq 0) { '[]' } else { ConvertTo-Json @($clip.Files) -Compress }
                    Send-Json -Context $context -Json $json -Kind 'vfiles'
                }
                'audio'  { Send-Bytes -Context $context -Bytes $clip.Bytes -ContentType 'audio/wav' -Kind 'audio' }
                'html'   { Send-Text -Context $context -Text $clip.Text -Kind 'html' -ContentType 'text/html; charset=utf-8' }
                'url'    { Send-Text -Context $context -Text $clip.Text -Kind 'url' }
                'rtf'    { Send-Text -Context $context -Text $clip.Text -Kind 'rtf' -ContentType 'text/rtf; charset=utf-8' }
                'text'   { Send-Text -Context $context -Text $clip.Text -Kind 'text' }
                default  { Send-Text -Context $context -Text '' -Kind 'empty' }
            }
        } catch {
            try {
                Send-Text -Context $context -Text "error: $($_.Exception.Message)" -Kind 'error' -Status 500
            } catch { }
        }
    }
}
finally {
    $listener.Stop()
    $listener.Close()
    $mutex.ReleaseMutex()
    Write-Host "clipd stopped." -ForegroundColor DarkGray
}
