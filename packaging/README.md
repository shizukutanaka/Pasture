# 配布ストアへの提出準備（packaging/）

winget と Homebrew へ「出す」と決めたらすぐ提出できるよう、下ごしらえを置く場所です。
**このフォルダがあるだけでは何も公開されません。** 提出（外部リポジトリへの PR や
自前 tap の公開）は所有者の判断で行います。

## 実態（2026-11-18 実測）

- Pasture は Rust 製の単一バイナリです（`pasture` コマンド1本）。
- GitHub Release **v0.28.0** が実在し、資産3件が付いています。
  - `pasture-v0.28.0-windows-x86_64.zip`
  - `pasture-v0.28.0-macos-aarch64.tar.gz`
  - `pasture-v0.28.0-linux-x86_64.tar.gz`
  - 各アーカイブの中身は「同名フォルダ＋その中のバイナリ」という形です。
- 下のマニフェスト類に入っている SHA-256 は、2026-11-15 に実際の配布ファイルを
  ダウンロードして測った値です（推測値ではありません）。
  ただし Release への `SHA256SUMS` ファイルの添付自体は未実施で、所有者判断待ちの
  別件です（winget / Homebrew の提出に添付は必須ではありません）。

| 資産 | SHA-256 |
|---|---|
| windows-x86_64.zip | `89d38b2f78427f82c0ff5028b8bcbb8801686984037a0395be452d5b8ed11fc9` |
| macos-aarch64.tar.gz | `f22641e744c3f6f55a9a6c149b894d097944b4734566f5a254e3934d69233c63` |
| linux-x86_64.tar.gz | `edb51219f0b12852a6c2b6c61bb6bdb3910890dc337e7220c3284bf07b4a69a8` |

ダウンロード URL はいずれも
`https://github.com/shizukutanaka/Pasture/releases/download/v0.28.0/<資産名>` です。

## winget（Windows）

- 下ごしらえ: `packaging/winget/manifests/s/shizukutanaka/Pasture/0.28.0/` の
  YAML 3点。提出先リポジトリ microsoft/winget-pkgs の中の置き場所と、まったく同じ
  階層にしてあります。
- 提出手順:
  1. microsoft/winget-pkgs を自分のアカウントへフォークする。
  2. フォーク側の同じ階層（`manifests/s/shizukutanaka/Pasture/0.28.0/`）へ
     YAML 3点をそのままコピーする。
  3. Windows 実機で検証する:
     - `winget validate --manifest manifests/s/shizukutanaka/Pasture/0.28.0`
     - `winget install --manifest manifests/s/shizukutanaka/Pasture/0.28.0`
       で実際にインストールでき、`pasture version` が動くこと。
  4. winget-pkgs へ PR を出す。あとは先方の自動検証とレビューを待つ。
- 所有者側で必要なもの: 「提出する」という判断と、GitHub アカウントでの PR 作成だけです。
  費用はかかりません。

## Homebrew（macOS / Linux）

選択肢が2つあります。

### A. 自前 tap（すぐ使える形。今回の下ごしらえはこちら向け）

- `packaging/homebrew/pasture.rb` は、Release の出来合いバイナリを入れる Formula です。
- 手順:
  1. `homebrew-tap` という名前の公開リポジトリを自分のアカウントに作る。
  2. `pasture.rb` をそのリポジトリの `Formula/` フォルダへ置く。
  3. 利用者は `brew install shizukutanaka/tap/pasture` で入れられます。
- 所有者側で必要なもの: 「tap を公開する」という判断と、リポジトリ作成だけです。

### B. homebrew-core（公式入り。ハードルが高い）

- 公式入りには「広く使われていること」の審査があり、現時点では現実的な近道では
  ありません。狙う場合は、出来合いバイナリではなく**ソースからビルドする Formula**
  が必要です。そのときに足りない材料は1つだけ:
  - ソース tarball（`https://github.com/shizukutanaka/Pasture/archive/refs/tags/v0.28.0.tar.gz`）
    の SHA-256 実測値。GitHub が生成する tarball は手元の `git archive` とは
    バイト列が一致しないため、必ず実物をダウンロードして測ること。
- 骨組み（ハッシュは提出時に実測して入れる）:

```ruby
class Pasture < Formula
  desc "OpenAI-compatible local/cloud LLM routing proxy"
  homepage "https://github.com/shizukutanaka/Pasture"
  url "https://github.com/shizukutanaka/Pasture/archive/refs/tags/v0.28.0.tar.gz"
  sha256 "<提出時に実ダウンロードで実測する>"
  license "MIT"

  depends_on "rust" => :build
  depends_on "openssl@3" # cloud 機能（TLS）に必要

  def install
    system "cargo", "install", *std_cargo_args, "--features", "cloud"
  end

  test do
    assert_match "pasture #{version}", shell_output("#{bin}/pasture version")
  end
end
```

## 提出前の確認リスト

- [ ] 資産を再ダウンロードし、上の表のハッシュと一致することを確認する
      （提出直前の鮮度確認。Release 資産が差し替わっていたらマニフェストも直す）。
- [ ] winget: Windows 実機で `winget validate` と `--manifest` での実インストール確認。
- [ ] Homebrew: Mac 実機で `brew install`（tap 経由）と `brew test pasture` の確認。
- [ ] Linux バイナリが OpenSSL を動的リンクしていないか `ldd` で確認する。
      リリース用ビルドは Linux では静的リンク指定が無いため、もし `libssl.so.3` を
      要求するなら tap の Formula に `depends_on "openssl@3"` を足すこと
      （macOS / Windows の資産は静的リンク済み）。

## 版を上げたときの更新手順

新しい Release（例: v0.29.0）を出したら:

1. 新しい資産3件をダウンロードし、SHA-256 を測る。
2. winget: `0.28.0` フォルダを新しい版数で複製し、版数・URL・ハッシュを差し替えて
   winget-pkgs へ新しい PR を出す。
3. Homebrew: `pasture.rb` の `version`・URL・ハッシュを差し替える。

## 今回見送ったこと（申し送り）

- README 本体への winget / brew のインストール導線の追加は、実際に提出・公開する
  ことが決まってから行う（公開されていない導線を看板に書かないため）。
