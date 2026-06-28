---
description: Windows クリップボードの内容を取得して会話に貼り付ける
keywords:
  - clipboard
  - windows
  - clipwire
  - クリップボード
  - 貼り付け
  - ペースト
triggers:
  - クリップボードを貼り付け
  - windows クリップボードから
  - クリップボードの内容を取得
---

# /clipwire

Windows クリップボードの内容を取得して会話に展開する。

## 引数

`$ARGUMENTS` — オプション。`-q`（quiet: パスのみ）/ `-d DIR`（画像保存先）

## 実行手順

### 1. clipwire get を実行

```bash
clipwire get $ARGUMENTS
```

`clipwire` が PATH に入っていない場合:

```bash
~/bin/clipwire get $ARGUMENTS
```

### 2. 種別に応じた処理

| X-Clip-Kind | 処理 |
|---|---|
| `text` / `url` / `html` / `rtf` | 内容をそのまま会話に展開する |
| `image` | 保存されたパスを Read ツールで読み込んで表示する |
| `files` / `vfiles` | curl コマンドを提示し、必要なら実行するか確認する |
| `audio` | 保存されたパスを報告する |
| `empty` | 「クリップボードは空です」と報告する |

### 3. 内容を会話に反映

取得した内容を次の作業に使えるよう整理して提示する。
