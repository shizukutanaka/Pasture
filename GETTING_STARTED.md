# はじめての pasture（やさしいガイド）

pasture（かぼす）は、**自分のパソコンで AI を動かす**ためのツールです。
むずかしい設定はいりません。Ollama という無料ソフトと組み合わせて使います。

- ふだんの質問は、**手元のパソコンで無料・非公開**に処理します。
- 難しい質問やコードのときだけ、必要に応じて**クラウド**に切り替えられます（任意）。
- 個人情報（メール・電話番号・カード番号など）が含まれる質問は、**自動でクラウドに送りません**。

このガイドのとおりに進めれば、はじめての方でも動かせます。

---

## まず用語だけ（3つ）

- **Ollama（オラマ）**: AI モデルをパソコンで動かす無料ソフト。pasture はこれを土台に使います。
- **モデル**: AI の本体（例: `llama3`）。Ollama で「ダウンロード」して使います。
- **プロキシ / base_url**: pasture は「受け付け窓口」として動きます。お使いの AI アプリの接続先（base_url）を pasture に向けるだけで、アプリ側はそのまま使えます。

---

## 3ステップで動かす

### 1. Ollama を入れる

一番かんたんなのは、pasture に任せる方法です。ターミナルで次を実行します:

```
pasture engine
```

公式の配布元から Ollama を取得し、正しいものか（チェックサム）を確かめてから、起動まで行います。取得するものとサイズは、ダウンロードの前に表示されます。

手動で入れる場合は [https://ollama.com](https://ollama.com) からインストールします。
インストール後はたいてい自動で動き始めます（動かない場合はターミナルで `ollama serve`）。

### 2. モデルをダウンロードする

ターミナルで次を実行します（数分かかります）。

```sh
ollama pull llama3.2
```

### 3. ひとつのコマンドで起動する

```sh
pasture up
```

`pasture up` は、（必要なら）Ollama を起動し、モデルが未取得なら自動でダウンロードし、そのままプロキシを起動します。起動時に、アプリに設定する接続先（base_url）も表示します。
うまくいかないときは `pasture doctor` で状態を確認してください（`[!!]` の下に直し方が出ます）。

> pasture 本体の入手: [Release ページ](https://github.com/shizukutanaka/Pasture/releases/tag/v0.28.0)から OS 別のバイナリをダウンロードするのが一番かんたんです（ビルド不要・クラウド振り分け入り）。
> ソースからビルドする場合: `git clone https://github.com/shizukutanaka/pasture && cd pasture && cargo build --release`（Rust が必要です。`target/release/pasture` ができます。クラウド併用には `cargo build --release --features cloud`）

---

## 使ってみる

### その場で1回だけ質問する

```sh
pasture chat "日本一高い山は？"
```

答えが少しずつ流れて表示されます。

### アプリから使う（プロキシとして起動）

```sh
pasture serve
```

起動すると `http://127.0.0.1:8645` で待ち受けます。
お使いの OpenAI 対応アプリの**接続先（base_url）**をここに向けてください。

例（多くのツールで使える環境変数）:

```sh
export OPENAI_BASE_URL="http://127.0.0.1:8645/v1"
export OPENAI_API_KEY="dummy"   # ローカル利用ではダミーで構いません
```

あとはアプリをいつもどおり使うだけ。pasture が「手元 / クラウド」を自動で振り分けます。

お使いのアプリ別の設定は次で確認できます:

```sh
pasture connect            # 対応アプリ一覧
pasture connect cursor     # 例: Cursor の設定手順
pasture connect openwebui  # 例: Open WebUI の設定手順
pasture connect lmstudio   # LM Studio をエンジンとして使う方法
pasture calibrate          # 自分の使い方に合わせて閾値を推奨
```

どのモデルを入れるか迷ったら:

```sh
pasture models             # お使いの RAM に合った推奨モデル
```

---

## 困ったとき（pasture doctor の見方）

- **`[!!] Ollama is not reachable`**
  Ollama が動いていません。インストール後、`ollama serve` で起動してください。
- **`[!!] local model 'llama3' is not installed`**
  モデルが未ダウンロードです。`ollama pull llama3.2` を実行してください。
- **`[!!] proxy port ... is already in use`**
  ポートが使用中です。別のポートで起動できます。
  `PASTURE_LISTEN_ADDR=127.0.0.1:8646 pasture serve`

迷ったら、いつでも次を実行してください。

```sh
pasture doctor
```

---

## クラウドも使いたい場合（任意）

手元だけで十分なら、この章は読み飛ばして大丈夫です。

クラウド（OpenAI / Anthropic）への自動切り替えは、配布バイナリに最初から入っています。
自分の API キーを環境変数に入れるだけです（キーはログに記録されません）。
ソースからビルドした場合は、先に `cargo build --release --features cloud` でビルドしてください。

```sh
export PASTURE_OPENAI_API_KEY="sk-..."      # OpenAI を使う場合
# または
export PASTURE_ANTHROPIC_API_KEY="sk-ant-..."  # Anthropic を使う場合
export PASTURE_CLOUD_PROVIDER="openai"       # または "anthropic"
```

設定できたか確認:

```sh
pasture doctor
```

`[ok] cloud fallback ready` と出れば準備完了です。

---

## よくある質問

- **お金はかかりますか？**
  pasture 本体と Ollama は無料です。手元（local）での処理に料金は発生しません。
  クラウドを使ったときだけ、各社の料金がかかります。
- **自分の質問は外部に送られますか？**
  既定では手元で処理します。クラウドに切り替えたときだけ送信されます。
  個人情報を含む質問は自動的に手元に固定され、クラウドへは送りません。
- **使った内訳を見たい**
  `pasture stats` で、手元 / クラウド / キャッシュの件数や費用の概算を確認できます。

---

## 表示言語

日本語と英語に対応しています。環境変数で切り替えられます。

```sh
PASTURE_LANG=ja pasture doctor   # 日本語
PASTURE_LANG=en pasture doctor   # English
```
（未指定時はお使いの端末の言語設定を見て自動選択します）

## かんたんインストール（任意）

ビルドと配置をまとめて行うスクリプトもあります。

```sh
sh install.sh          # macOS / Linux
# Windows: powershell -ExecutionPolicy Bypass -File install.ps1
```

## 新しい版への更新

配布バイナリを使っている場合は、更新もコマンドひとつです。

```sh
pasture update          # 最新版をインストール
pasture update --check  # 新しい版があるかだけ確認
```

確認は、このコマンドを実行したときだけ行われます（裏で勝手に通信することはありません）。ダウンロードしたファイルは、リリースに付属するチェックサムと照合してから入れ替えるので、照合に失敗した場合は今のバイナリがそのまま残ります。ソースからビルドした場合は `pasture update` がその旨を表示します。最新のコードで再ビルドするか、上のインストールスクリプトを再実行してください。

---

困ったら `pasture doctor`。それだけ覚えておけば大丈夫です。
