<#
.SYNOPSIS
    clipd - Windows クリップボードの内容を HTTP で返す軽量サービス

.DESCRIPTION
    Tailscale 網内認証を前提とし、localhost と Tailscale IP にのみバインドする。
    リモート (tssh でログインした Linux サーバ等) から同じ tailnet 経由で叩くと、
    現在の Windows クリップボードの内容を返す。

    クリップボードの種別を自動判別して返す:
      - 画像        -> 200  image/png            (PNG バイナリ)
      - ファイル一覧 -> 200  application/json      (パスの配列)
      - テキスト    -> 200  text/plain; utf-8
      - 空          -> 200  text/plain; utf-8     (空文字列)

    種別はレスポンスヘッダ X-Clip-Kind (image|files|text|empty) でも返す。

    ルーティング (使い分けは想定せず自動判別に一本化):
      GET /        自動判別
      GET /clip    自動判別 (/ と同じ)
      GET /health  死活確認
      それ以外     404

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
    param([scriptblock]$Script)
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

# ---- クリップボード読み取り (自動判別 + retry) ------------------------------
# エンドポイントを自動判別に一本化したので、読み取りも1種類だけ。
function Read-Clipboard {
    $script = {
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        for ($i = 0; $i -lt 5; $i++) {
            try {
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
                elseif ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) {
                    $files = [System.Windows.Forms.Clipboard]::GetFileDropList()
                    $list = @()
                    foreach ($f in $files) { $list += $f }
                    return [pscustomobject]@{ Kind = 'files'; Files = $list }
                }
                elseif ([System.Windows.Forms.Clipboard]::ContainsText()) {
                    $t = [System.Windows.Forms.Clipboard]::GetText()
                    return [pscustomobject]@{ Kind = 'text'; Text = $t }
                }
                else {
                    return [pscustomobject]@{ Kind = 'empty' }
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
    param($Context, [string]$Text, [string]$Kind, [int]$Status = 200)
    $bytes = [System.Text.Encoding]::UTF8.GetBytes([string]$Text)
    Send-Bytes -Context $Context -Bytes $bytes -ContentType 'text/plain; charset=utf-8' -Kind $Kind -Status $Status
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
            if ($path -ne '/' -and $path -ne '/clip' -and $path -ne '/file') {
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

            # / or /clip: 自動判別して返す
            $clip = Read-Clipboard
            switch ($clip.Kind) {
                'image' { Send-Bytes -Context $context -Bytes $clip.Bytes -ContentType 'image/png' -Kind 'image' }
                'files' {
                    $json = if ($null -eq $clip.Files -or $clip.Files.Count -eq 0) { '[]' } else { ConvertTo-Json @($clip.Files) -Compress }
                    Send-Json -Context $context -Json $json -Kind 'files'
                }
                'text'  { Send-Text -Context $context -Text $clip.Text -Kind 'text' }
                default { Send-Text -Context $context -Text '' -Kind 'empty' }
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
