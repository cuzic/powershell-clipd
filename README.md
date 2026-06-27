# clipd

Windows クリップボードの内容を HTTP で返す軽量サービスと、Linux 側クライアント。  
Tailscale tailnet 内での使用を前提とする。

## 構成

| ファイル | 役割 |
|---|---|
| `clipd.ps1` | Windows 側サーバ。クリップボードを HTTP で返す |
| `clip` | Linux/Mac 側クライアント。`clipd` を叩いて内容を取得する |
| `tmux.conf.snippet` | tmux キーバインド設定例 (`<prefix> Ctrl-V` で貼り付け) |

## 動作イメージ

```
Windows (clipd.ps1)          Linux / Mac (clip)
┌──────────────────┐          ┌──────────────────────┐
│  クリップボード   │  tailnet │  tssh / ssh でログイン│
│  ↓ HTTP         │◀─────────│  $ clip               │
│  clipd.ps1:9999  │          │  → 内容が標準出力に   │
└──────────────────┘          └──────────────────────┘
```

## Windows 側: clipd.ps1

### 起動方法

```powershell
# tailnet に出す + Bearer token 認証 (推奨)
powershell -ExecutionPolicy Bypass -File clipd.ps1 -Token "好きな文字列"

# tailnet に出す + token なし (明示的に許可)
powershell -ExecutionPolicy Bypass -File clipd.ps1 -AllowNoToken

# localhost だけで使う (同じ Windows 上の tmux 等)
powershell -ExecutionPolicy Bypass -File clipd.ps1 -BindLocalhostOnly
```

環境変数でもトークンを渡せる:

```powershell
$env:CLIPD_TOKEN = "好きな文字列"
powershell -ExecutionPolicy Bypass -File clipd.ps1
```

### パラメータ

| パラメータ | 既定 | 説明 |
|---|---|---|
| `-Port` | `9999` | 待ち受けポート |
| `-Token` | `$env:CLIPD_TOKEN` | Bearer トークン (未指定で認証なし) |
| `-BindLocalhostOnly` | off | localhost のみバインド |
| `-AllowNoToken` | off | token なし tailnet 公開を明示許可 |

### セキュリティ

- token なし & tailnet 公開はデフォルトで拒否される (`-AllowNoToken` で上書き可)
- 多重起動防止に名前付き Mutex を使用
- Tailscale IP は `tailscale ip -4` または CGNAT 帯 (`100.64.0.0/10`) で自動検出

### Tailscale IP へのバインド権限

初回起動時に `Failed to start HttpListener` が出る場合、管理者 PowerShell で:

```powershell
netsh http add urlacl url=http://<tailscale-ip>:9999/ user="DOMAIN\username"
```

### API

| エンドポイント | 説明 |
|---|---|
| `GET /` | クリップボード自動判別 |
| `GET /clip` | 同上 |
| `GET /file?path=<encoded>` | クリップボードに存在するパスのファイルをストリーム (認証あり) |
| `GET /health` | 死活確認 (認証不要) |

レスポンスヘッダ `X-Clip-Kind` に種別が入る:

| 値 | Content-Type | 内容 |
|---|---|---|
| `image` | `image/png` | PNG バイナリ |
| `files` | `application/json` | Windows パスの配列 |
| `text` | `text/plain; charset=utf-8` | テキスト |
| `empty` | `text/plain; charset=utf-8` | 空文字列 |

## Linux 側: clip

### インストール

```bash
curl -o ~/bin/clip https://raw.githubusercontent.com/cuzic/powershell-clipd/main/clip
chmod +x ~/bin/clip
```

### 設定

```bash
export CLIPD_HOST=my-windows   # Tailscale MagicDNS 名 or 100.x.y.z
export CLIPD_PORT=9999         # 省略可 (既定 9999)
export CLIPD_TOKEN=secret      # clipd を -Token 付きで起動した場合のみ
```

`.bashrc` / `.zshrc` に書いておくと便利。

### 使い方

```bash
clip              # クリップボードの内容を自動判別して出力
clip -q           # パスや本文だけ (Claude Code などに渡しやすい)
clip -d ~/pics    # 画像の保存先を指定 (既定: mktemp で /tmp 以下に生成)
clip -h           # ヘルプ
```

### 出力例

```bash
# テキストの場合
$ clip
コピーしたテキストがここに出る

# 画像の場合 (デフォルトは /tmp 以下の一時ファイル)
$ clip
画像を保存しました: /tmp/tmp.aB3xYz.png

# -d で保存先を指定することも可
$ clip -d ~/pics
画像を保存しました: /tmp/tmp.aB3xYz.png  # ← -d ~/pics を付けると ~/pics/clip_TIMESTAMP.png

# ファイルリストの場合 → パスと取得コマンドを表示 (実行は Claude Code に委ねる)
$ clip
クリップボード: ファイル 2件

  C:\Users\user\Desktop\foo.txt
  → curl -fsSL 'http://my-windows:9999/file?path=C%3A%5CUsers%5Cuser%5CDesktop%5Cfoo.txt' -o 'foo.txt'

  C:\Users\user\Desktop\bar.png
  → curl -fsSL 'http://my-windows:9999/file?path=...' -o 'bar.png'

# quiet モード: curl コマンドだけ出力 (Claude Code やシェルへのパイプ向け)
$ clip -q
curl -fsSL 'http://my-windows:9999/file?path=C%3A%5C...' -o 'foo.txt'
curl -fsSL 'http://my-windows:9999/file?path=...' -o 'bar.png'

# quiet モード (スクリプトや Claude Code へのパイプ向け)
$ clip -q
/tmp/tmp.aB3xYz.png
```

## Claude Code / tmux との連携

Claude Code のキーバインドシステムはシェルコマンドを直接実行する機能を持たないため、
tmux 経由でペインに貼り付けるのが最もシンプルな方法。

### tmux: `<prefix> Ctrl-V` で貼り付け

`tmux.conf.snippet` の内容を `~/.tmux.conf` に追記する:

```bash
cat tmux.conf.snippet >> ~/.tmux.conf
tmux source ~/.tmux.conf
```

設定内容:

```
bind C-v run-shell "clip -q | tmux load-buffer - " \; paste-buffer -p
```

これで `<prefix> Ctrl-V` を押すと:
- **テキスト** → そのまま現在のペイン (Claude Code 入力欄) に流れる
- **画像** → `/tmp/tmp.XXX.png` のパスが入力欄に入る → そのまま Enter で Claude Code が画像を読む
- **ファイル一覧** → curl コマンド群が入力欄に入る → Claude Code が必要なものを実行

`<prefix> Alt-V` はファイルの場合のみ curl コマンドをその場で bash 実行し、カレントディレクトリにダウンロードします。テキスト・画像は通常の貼り付けにフォールバックします。

**セキュリティ:** `/file?path=` は今クリップボードにあるパスのみ許可します。任意のパスは 403 で拒否されます。

## ライセンス

MIT
