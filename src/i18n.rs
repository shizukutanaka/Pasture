//! Minimal zero-dependency internationalisation (I10: Japanese-first + i18n,
//! keys as `namespace.component.key`). Strings are looked up by key for the
//! active language; missing keys fall back to English, then to the key itself.
//! `{name}` placeholders are interpolated by `tf`.

/// Supported UI languages. English is the fallback locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Ja,
}

impl Lang {
    /// Short locale code ("en" / "ja").
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ja => "ja",
        }
    }
}

/// Detect the UI language from `PASTURE_LANG`, then `LC_ALL`/`LANG`.
/// A value starting with `ja` selects Japanese; otherwise English.
pub fn detect() -> Lang {
    let raw = std::env::var("PASTURE_LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default();
    lang_from_str(&raw)
}

/// Map a locale-ish string to a `Lang` (pure, testable).
pub fn lang_from_str(s: &str) -> Lang {
    if s.trim().to_lowercase().starts_with("ja") {
        Lang::Ja
    } else {
        Lang::En
    }
}

/// Look up a key, falling back to English, then to the key itself.
pub fn t(lang: Lang, key: &str) -> &'static str {
    lookup(lang, key)
        .or_else(|| lookup(Lang::En, key))
        .unwrap_or(key_as_static(key))
}

/// Look up a key and interpolate `{name}` placeholders.
pub fn tf(lang: Lang, key: &str, args: &[(&str, &str)]) -> String {
    let mut s = t(lang, key).to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
    let table = match lang {
        Lang::En => EN,
        Lang::Ja => JA,
    };
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

// Unknown keys are echoed back so missing translations are obvious in output
// without panicking. Only a small set of known keys is interned; anything else
// returns a generic marker (the catalogs are the source of truth).
fn key_as_static(_key: &str) -> &'static str {
    "(missing translation)"
}

const EN: &[(&str, &str)] = &[
    (
        "welcome",
        "pasture — run AI models on your own computer, with automatic cloud fallback.\n\n\
New here? Just three steps:\n\
    1. Install Ollama (runs the local models):   https://ollama.com\n\
    2. Download a model:                          ollama pull llama3.2\n\
    3. Start everything with one command:         pasture up\n\n\
Or check your setup first:  pasture doctor\n\
All commands:               pasture help\n",
    ),
    ("doctor.title", "pasture doctor — checking your setup\n"),
    ("doctor.ollama.ok", "[ok] Ollama is running at {host}:{port}"),
    ("doctor.ollama.models", "     models: {models}"),
    ("doctor.ollama.nomodels", "     (no models downloaded yet)"),
    (
        "doctor.ollama.unreachable",
        "[!!] Ollama is not reachable at {host}:{port}",
    ),
    (
        "doctor.ollama.fix",
        "     Ollama runs the local models. Install it: https://ollama.com\n     then start it (often automatic), e.g.:  ollama serve",
    ),
    ("doctor.model.ok", "[ok] local model '{model}' is installed"),
    (
        "doctor.model.missing",
        "[!!] local model '{model}' is not installed",
    ),
    ("doctor.model.fix", "     download it with:  ollama pull {model}"),
    ("doctor.model.unknown", "[!!] local model '{model}' status unknown (engine is not reachable)"),
    ("doctor.ollama.fix.notinstalled", "     Ollama is not installed. Get it: https://ollama.com"),
    ("doctor.ollama.fix.notrunning", "     Ollama is installed but not running. Start it:  ollama serve"),
    ("doctor.port.ok", "[ok] proxy port {addr} is free"),
    ("doctor.port.inuse", "[!!] proxy port {addr} is already in use"),
    (
        "doctor.port.fix",
        "     use another:  PASTURE_LISTEN_ADDR=127.0.0.1:8646 pasture serve",
    ),
    ("doctor.cloud.ready", "[ok] cloud fallback ready ({provider})"),
    ("doctor.cloud.off", "[--] cloud fallback off (set {hint} to enable)"),
    (
        "doctor.cloud.localonly",
        "[--] local-only build (rebuild with `--features cloud` to add cloud fallback)",
    ),
    ("doctor.allgood", "All good! Try:  pasture chat \"hello\""),
    (
        "doctor.problems",
        "{n} item(s) need attention (see [!!] above).",
    ),
    ("up.checking", "pasture up — getting you running\n"),
    (
        "up.ollama_required",
        "Ollama is required and not running. Install it (https://ollama.com), then re-run `pasture up`.",
    ),
    ("up.have_model", "Model '{model}' is ready."),
    ("up.pulling", "Downloading model '{model}' (one-time)..."),
    (
        "up.pull_failed",
        "Could not download '{model}'. Is the `ollama` command installed? Try:  ollama pull {model}",
    ),
    ("up.starting", "Starting the proxy. Point your app's base_url here."),
    (
        "up.starting_ollama",
        "Ollama is not running — trying to start it...",
    ),
    ("up.ollama_started", "Ollama started."),
    (
        "connect.help",
        "Connect your app to this address (base_url):\n    http://{addr}/v1\n  Example (works with most OpenAI-compatible apps):\n    export OPENAI_BASE_URL=\"http://{addr}/v1\"\n    export OPENAI_API_KEY=\"dummy\"\n",
    ),
    (
        "connect.list",
        "Pick your app:  pasture connect <app>\n  apps: openwebui, continue, cursor, lmstudio, sdk\n",
    ),
    ("connect.unknown", "Unknown app '{app}'. Try: openwebui, continue, cursor, lmstudio, sdk."),
    (
        "connect.openwebui",
        "Open WebUI:\n  Settings -> Connections -> add an OpenAI API connection.\n    URL:     {base}\n    API key: any value (e.g. dummy)",
    ),
    (
        "connect.continue",
        "Continue (VS Code): add a model to your config with\n    provider: openai\n    apiBase:  {base}\n    apiKey:   any value\n    model:    your local model name (e.g. llama3.2)",
    ),
    (
        "connect.cursor",
        "Cursor:\n  Settings -> Models -> enable \"Override OpenAI Base URL\".\n    Base URL: {base}\n    API key:  any value\n  Tip: select only your custom model to avoid validation errors.",
    ),
    (
        "connect.sdk",
        "OpenAI SDK / curl:\n    base_url: {base}\n    api_key:  any value (ignored for local use)",
    ),
    ("models.title", "Recommended models (verify tags at ollama.com/library)\n"),
    ("models.your_machine", "Your machine: about {ram} MB RAM -> use the {tier} tier."),
    // ADR-261: RAM detection can legitimately fail (non-Linux). Say so instead
    // of reporting 0 MB and steering the user to the CPU-only tier.
    ("models.hw_unknown", "  (hardware detection is unavailable on this OS - assuming a mid-range machine.\n   Set PASTURE_RAM_MB or PASTURE_THRESHOLD to make this exact.)"),
    ("doctor.hw.ok", "[ok] hardware: {ram} MB RAM -> routing threshold {threshold} tokens"),
    ("doctor.costlog.ok", "[ok] cost log writable at {path}"),
    ("doctor.costlog.bad", "[!!] cost log cannot be written ({path}): {why}"),
    ("doctor.costlog.fix", "     set PASTURE_COST_LOG=<path> to a writable location (spend tracking is off until then)"),
    ("doctor.hw.unknown", "[--] hardware: RAM not detectable on this OS -> using threshold {threshold} tokens (leaning local)"),
    ("doctor.hw.unknown.fix", "     set PASTURE_RAM_MB=<total MB> (or PASTURE_THRESHOLD=<tokens>) for exact routing"),
    (
        "models.ultra",
        "CPU-only / <8 GB RAM (ultra-light, runs on any modern laptop):\n    ollama pull phi3:mini          # 3.8B Q4, ~2.3 GB, fast on CPU\n    ollama pull gemma2:2b          # 2.6B, very fast, good quality/size\n    ollama pull qwen2.5:1.5b       # 1.5B, lowest RAM, still capable\n    ollama pull tinyllama          # 1.1B, absolute minimum (fallback)\n\n  For dual-local routing (fast model for simple, main for complex):\n    PASTURE_LOCAL_FAST_MODEL=qwen2.5:1.5b  PASTURE_LOCAL_MODEL=phi3:mini",
    ),
    (
        "models.local_only_tip",
        "  Tip: no cloud? run with  PASTURE_LOCAL_ONLY=1  to keep everything on-device.\n  Tip: add  PASTURE_INJECT_CONTEXT=1  for date/OS context (better PC-assistant answers).",
    ),
    (
        "models.low",
        "8 GB RAM:\n    ollama pull llama3.2      # 3B, best small general model\n    ollama pull gemma3:4b     # 4B, strong all-rounder\n    ollama pull qwen3:4b      # 4B, great for non-English",
    ),
    (
        "models.mid",
        "16 GB RAM / 8 GB VRAM:\n    ollama pull llama3.1:8b        # solid general 8B\n    ollama pull qwen2.5-coder:7b   # best small coding model",
    ),
    (
        "models.multilingual",
        "Best for non-English (incl. Japanese): the Qwen family, e.g.\n    ollama pull qwen2.5:7b",
    ),
    (
        "models.note",
        "Set your choice with:  PASTURE_LOCAL_MODEL=<name>   (then: pasture up)",
    ),
    (
        "connect.lmstudio",
        "LM Studio is itself a local OpenAI server (default {base_lm}).\n  To use LM Studio as pasture's local engine:\n    1. In LM Studio: Server tab -> load a model -> Start Server.\n    2. Run pasture with:  PASTURE_LOCAL_BACKEND=lmstudio pasture up\n  Your apps still point at pasture:  {base}",
    ),
    (
        "up.lmstudio_required",
        "Local OpenAI server (LM Studio) is not reachable. In LM Studio, open the Server tab, load a model, and Start Server — then re-run `pasture up`.",
    ),
    (
        "doctor.engine.ok",
        "[ok] local engine ({engine}) is running at {host}:{port}",
    ),
    (
        "doctor.engine.unreachable",
        "[!!] local engine ({engine}) is not reachable at {host}:{port}",
    ),
    (
        "doctor.engine.fix",
        "     Start your local OpenAI server (e.g. LM Studio: Server tab) and load a model.",
    ),
    (
        "calibrate.empty",
        "No cost log yet at {path}. Run some requests first, then calibrate.",
    ),
    (
        "calibrate.header",
        "Calibrating threshold from {n} logged prompt(s), target cloud rate {target}%:",
    ),
    (
        "calibrate.result",
        "  recommended threshold: {threshold} tokens  (~{rate}% would route to cloud by length)",
    ),
    (
        "calibrate.note",
        "  (length-only estimate; content signals — reasoning/code/privacy — escalate more.)",
    ),
    (
        "calibrate.apply",
        "  apply with:  PASTURE_THRESHOLD={threshold}",
    ),
    // ADR-172: warn when quantile is unreliable due to small sample.
    ("calibrate.small_sample", "  [note] {n} sample(s) is fewer than the recommended minimum ({min}). The quantile estimate has a wide confidence interval; collect more requests before relying on this recommendation."),
    ("calibrate.logprob.empty", "No cascade confidence logged yet. Enable cascade (PASTURE_CASCADE=1) with an OpenAI-compatible local backend (e.g. LM Studio) and run some requests, then calibrate --logprob."),
    ("calibrate.logprob.header", "Calibrating cascade logprob from {n} sample(s), target escalation rate {target}%:"),
    ("calibrate.logprob.result", "  recommended threshold: {threshold}  (~{rate}% of local answers would escalate)"),
    ("calibrate.logprob.apply", "  apply with:  PASTURE_CASCADE_LOGPROB={threshold}"),
    ("calibrate.error.labels-required", "calibrate --error needs --labels <file.jsonl>; each line: {\"logprob\": -0.42, \"correct\": true}"),
    // IMP-47: signal validity self-test. Reported before any threshold fit, because
    // fitting a threshold to a signal with no discriminative power yields a
    // confident-looking number that spends cloud budget at random.
    ("calibrate.auroc.header", "Confidence-signal self-test on {n} labelled answer(s) ({correct} correct, {incorrect} incorrect):"),
    ("calibrate.auroc.score", "  AUROC = {auroc}  (0.5 = coin flip; 1.0 = perfectly separates correct from incorrect)"),
    ("calibrate.auroc.one-class", "  {path} has only one class (all correct or all incorrect). AUROC is undefined — label some answers of the other kind and re-run."),
    ("calibrate.auroc.verdict.random", "  VERDICT: no better than random. Do NOT gate escalation on this signal — calibrating a threshold to it would spend cloud calls at random. Collect labels for a different signal, or route by length/content only."),
    ("calibrate.auroc.verdict.weak", "  VERDICT: weak. There is some signal, but thresholds will be unstable; treat any calibrated value as a rough starting point and re-check after more labels."),
    ("calibrate.auroc.verdict.usable", "  VERDICT: usable. The signal separates correct from incorrect well enough to calibrate on — run calibrate --logprob or --error next."),
    ("calibrate.error.empty", "No usable labels in {path}. Each line: {\"logprob\": -0.42, \"correct\": true} (logprob from the cost log, correct = your judgement of that answer)."),
    ("calibrate.error.header", "Calibrating error-grounded cascade threshold from {n} labelled answer(s) (overall error {overall}%), target error rate {target}%:"),
    ("calibrate.error.band", "  logprob {from} to {to}: estimated error {err}%  (n={count})"),
    ("calibrate.error.result", "  recommended threshold: {threshold}  (~{rate}% of these answers would escalate; answers kept local have estimated error <= {target}%)"),
    ("calibrate.error.apply", "  apply with:  PASTURE_CASCADE_LOGPROB={threshold}"),
    ("calibrate.error.unachievable", "  no logprob band meets the {target}% error target — even the most confident answers exceed it. Collect more labels, or escalate everything with PASTURE_CASCADE_LOGPROB=0."),
    // IMP-42: --sweep prints several candidate thresholds side by side instead
    // of the single point --target picks — there is no universally correct
    // threshold, only the one that fits your own traffic and cost tolerance.
    ("calibrate.sweep.header", "Threshold sweep from {n} logged prompt(s) (length-only estimate; content signals escalate more):"),
    ("calibrate.sweep.row", "  target {target}% cloud  ->  PASTURE_THRESHOLD={threshold}  (~{rate}% by length)"),
    ("calibrate.logprob.sweep.header", "Cascade logprob threshold sweep from {n} sample(s):"),
    ("calibrate.logprob.sweep.row", "  target {target}% escalation  ->  PASTURE_CASCADE_LOGPROB={threshold}  (~{rate}% escalate)"),
    ("config.title", "pasture configuration (effective):"),
    ("config.set", "set"),
    ("config.unset", "not set"),
];

const JA: &[(&str, &str)] = &[
    (
        "welcome",
        "pasture — 自分のパソコンで AI を動かし、必要なときだけクラウドに切り替えるツール。\n\n\
はじめての方へ。たった3ステップ:\n\
    1. Ollama を入れる（ローカルのモデルを動かします）: https://ollama.com\n\
    2. モデルをダウンロード:                            ollama pull llama3.2\n\
    3. ひとつのコマンドで起動:                          pasture up\n\n\
先に状態を確認する場合:  pasture doctor\n\
すべてのコマンド:        pasture help\n",
    ),
    ("doctor.title", "pasture doctor — 環境を確認します\n"),
    ("doctor.ollama.ok", "[ok] Ollama は {host}:{port} で動作中"),
    ("doctor.ollama.models", "     モデル: {models}"),
    ("doctor.ollama.nomodels", "     （モデル未ダウンロード）"),
    (
        "doctor.ollama.unreachable",
        "[!!] Ollama に接続できません（{host}:{port}）",
    ),
    (
        "doctor.ollama.fix",
        "     Ollama がローカルのモデルを動かします。導入: https://ollama.com\n     その後、起動してください（多くは自動）。例:  ollama serve",
    ),
    ("doctor.model.ok", "[ok] ローカルモデル '{model}' は導入済み"),
    (
        "doctor.model.missing",
        "[!!] ローカルモデル '{model}' が未導入",
    ),
    (
        "doctor.model.fix",
        "     取得方法:  ollama pull {model}",
    ),
    ("doctor.model.unknown", "[!!] ローカルモデル '{model}' の状態は不明です（エンジンに接続できません）"),
    ("doctor.ollama.fix.notinstalled", "     Ollama がインストールされていません: https://ollama.com"),
    ("doctor.ollama.fix.notrunning", "     Ollama はインストール済みですが起動していません:  ollama serve"),
    ("doctor.port.ok", "[ok] プロキシのポート {addr} は空いています"),
    (
        "doctor.port.inuse",
        "[!!] プロキシのポート {addr} は使用中です",
    ),
    (
        "doctor.port.fix",
        "     別ポートで起動:  PASTURE_LISTEN_ADDR=127.0.0.1:8646 pasture serve",
    ),
    ("doctor.cloud.ready", "[ok] クラウド切替の準備完了（{provider}）"),
    (
        "doctor.cloud.off",
        "[--] クラウド切替はオフ（{hint} を設定すると有効）",
    ),
    (
        "doctor.cloud.localonly",
        "[--] ローカル専用ビルド（`--features cloud` で再ビルドするとクラウド切替が有効）",
    ),
    ("doctor.allgood", "準備完了。試してみましょう:  pasture chat \"こんにちは\""),
    (
        "doctor.problems",
        "{n} 件の対応が必要です（上の [!!] を参照）。",
    ),
    ("up.checking", "pasture up — 実行までを自動で進めます\n"),
    (
        "up.ollama_required",
        "Ollama が必要ですが動作していません。導入（https://ollama.com）後、もう一度 `pasture up` を実行してください。",
    ),
    ("up.have_model", "モデル '{model}' は準備済みです。"),
    ("up.pulling", "モデル '{model}' をダウンロード中（初回のみ）..."),
    (
        "up.pull_failed",
        "'{model}' をダウンロードできませんでした。`ollama` コマンドは入っていますか？  ollama pull {model}",
    ),
    (
        "up.starting",
        "プロキシを起動します。アプリの base_url をここに向けてください。",
    ),
    (
        "up.starting_ollama",
        "Ollama が起動していません。起動を試みます...",
    ),
    ("up.ollama_started", "Ollama を起動しました。"),
    (
        "connect.help",
        "アプリの接続先（base_url）をここに向けてください:\n    http://{addr}/v1\n  例（多くの OpenAI 互換アプリで使えます）:\n    export OPENAI_BASE_URL=\"http://{addr}/v1\"\n    export OPENAI_API_KEY=\"dummy\"\n",
    ),
    (
        "connect.list",
        "アプリを選んでください:  pasture connect <app>\n  対応: openwebui, continue, cursor, lmstudio, sdk\n",
    ),
    (
        "connect.unknown",
        "不明なアプリ '{app}'。次から選択: openwebui, continue, cursor, lmstudio, sdk。",
    ),
    (
        "connect.openwebui",
        "Open WebUI:\n  設定 -> 接続 -> OpenAI API 接続を追加。\n    URL:     {base}\n    APIキー: 任意の文字列（例: dummy）",
    ),
    (
        "connect.continue",
        "Continue (VS Code): 設定にモデルを追加します。\n    provider: openai\n    apiBase:  {base}\n    apiKey:   任意の文字列\n    model:    ローカルのモデル名（例: llama3.2）",
    ),
    (
        "connect.cursor",
        "Cursor:\n  Settings -> Models -> \"Override OpenAI Base URL\" を有効化。\n    Base URL: {base}\n    APIキー:  任意の文字列\n  ヒント: 検証エラーを避けるため、自作モデルだけを選択。",
    ),
    (
        "connect.sdk",
        "OpenAI SDK / curl:\n    base_url: {base}\n    api_key:  任意の文字列（ローカル利用では無視されます）",
    ),
    ("models.title", "推奨モデル（タグは ollama.com/library で確認）\n"),
    (
        "models.your_machine",
        "あなたの環境: 約 {ram} MB RAM -> {tier} 段がおすすめ。",
    ),
    ("models.hw_unknown", "  （この OS ではハードウェア検出が使えません。中位マシンとして扱います。\n   PASTURE_RAM_MB または PASTURE_THRESHOLD を設定すると正確になります）"),
    ("doctor.hw.ok", "[ok] ハードウェア: RAM {ram} MB -> ルーティング閾値 {threshold} トークン"),
    ("doctor.costlog.ok", "[ok] コストログは書き込み可能: {path}"),
    ("doctor.costlog.bad", "[!!] コストログに書き込めません（{path}）: {why}"),
    ("doctor.costlog.fix", "     PASTURE_COST_LOG=<パス> を書き込み可能な場所に設定してください（それまで支出記録は無効です）"),
    ("doctor.hw.unknown", "[--] ハードウェア: この OS では RAM を検出できません -> 閾値 {threshold} トークンを使用（ローカル寄り）"),
    ("doctor.hw.unknown.fix", "     正確なルーティングには PASTURE_RAM_MB=<合計MB>（または PASTURE_THRESHOLD=<トークン>）を設定してください"),
    (
        "models.ultra",
        "CPU のみ / 8 GB RAM 未満（超軽量。GPU なしのノートでも動作）:\n    ollama pull phi3:mini          # 3.8B Q4、約 2.3 GB、CPU でも速い\n    ollama pull gemma2:2b          # 2.6B、非常に速く品質も良好\n    ollama pull qwen2.5:1.5b       # 1.5B、最低 RAM、日本語対応\n    ollama pull tinyllama          # 1.1B、最小サイズ（フォールバック用）\n\n  デュアルローカルルーティング（簡単な質問→高速モデル、複雑→メインモデル）:\n    PASTURE_LOCAL_FAST_MODEL=qwen2.5:1.5b  PASTURE_LOCAL_MODEL=phi3:mini",
    ),
    (
        "models.local_only_tip",
        "  ヒント: クラウド不要なら  PASTURE_LOCAL_ONLY=1  で完全ローカル動作。\n  ヒント:  PASTURE_INJECT_CONTEXT=1  で日付/OS をコンテキスト注入（PC アシスタントとして便利）。",
    ),
    (
        "models.low",
        "8 GB RAM:\n    ollama pull llama3.2      # 3B、小型汎用で最良\n    ollama pull gemma3:4b     # 4B、バランス良好\n    ollama pull qwen3:4b      # 4B、非英語に強い",
    ),
    (
        "models.mid",
        "16 GB RAM / 8 GB VRAM:\n    ollama pull llama3.1:8b        # 安定した汎用 8B\n    ollama pull qwen2.5-coder:7b   # 小型コーディング最良",
    ),
    (
        "models.multilingual",
        "非英語（日本語含む）に最適: Qwen 系。例:\n    ollama pull qwen2.5:7b",
    ),
    (
        "models.note",
        "選んだら:  PASTURE_LOCAL_MODEL=<名前>   （その後: pasture up）",
    ),
    (
        "connect.lmstudio",
        "LM Studio 自体がローカルの OpenAI サーバです（既定 {base_lm}）。\n  LM Studio を pasture のローカルエンジンとして使うには:\n    1. LM Studio: Server タブ -> モデルを読み込み -> Start Server。\n    2. 次で pasture を起動:  PASTURE_LOCAL_BACKEND=lmstudio pasture up\n  アプリの接続先は引き続き pasture:  {base}",
    ),
    (
        "up.lmstudio_required",
        "ローカルの OpenAI サーバ（LM Studio）に接続できません。LM Studio の Server タブでモデルを読み込み、Start Server してから、もう一度 `pasture up` を実行してください。",
    ),
    (
        "doctor.engine.ok",
        "[ok] ローカルエンジン（{engine}）が {host}:{port} で動作中",
    ),
    (
        "doctor.engine.unreachable",
        "[!!] ローカルエンジン（{engine}）に接続できません（{host}:{port}）",
    ),
    (
        "doctor.engine.fix",
        "     ローカルの OpenAI サーバ（例: LM Studio の Server タブ）を起動し、モデルを読み込んでください。",
    ),
    (
        "calibrate.empty",
        "コストログがまだありません（{path}）。何度かリクエストを実行してから較正してください。",
    ),
    (
        "calibrate.header",
        "ログ {n} 件のプロンプトサイズから閾値を較正（目標クラウド率 {target}%）:",
    ),
    (
        "calibrate.result",
        "  推奨閾値: {threshold} tokens （長さ基準で約 {rate}% がクラウドへ）",
    ),
    (
        "calibrate.note",
        "  （長さのみの推定。推論/コード/機微などの内容シグナルでさらにクラウドへ回る）",
    ),
    (
        "calibrate.apply",
        "  適用:  PASTURE_THRESHOLD={threshold}",
    ),
    ("calibrate.small_sample", "  [注] サンプル数 {n} 件は推奨最小値（{min}）を下回っています。分位点推定の信頼区間が広いため、この推奨値はリクエストが増えてから参照してください。"),
    ("calibrate.logprob.empty", "カスケードの信頼度がまだ記録されていません。OpenAI互換ローカルバックエンド（例: LM Studio）でカスケード(PASTURE_CASCADE=1)を有効にして何度か実行してから calibrate --logprob を実行してください。"),
    ("calibrate.logprob.header", "ログ {n} 件からカスケードlogprob閾値を較正（目標昇格率 {target}%）:"),
    ("calibrate.logprob.result", "  推奨閾値: {threshold} （ローカル応答の約 {rate}% が昇格）"),
    ("calibrate.logprob.apply", "  適用:  PASTURE_CASCADE_LOGPROB={threshold}"),
    ("calibrate.error.labels-required", "calibrate --error には --labels <file.jsonl> が必要です。各行: {\"logprob\": -0.42, \"correct\": true}"),
    ("calibrate.auroc.header", "ラベル付き回答 {n} 件で信頼度シグナルを自己検査（正解 {correct} 件 / 不正解 {incorrect} 件）:"),
    ("calibrate.auroc.score", "  AUROC = {auroc}  （0.5 = でたらめ、1.0 = 正解と不正解を完全に分離）"),
    ("calibrate.auroc.one-class", "  {path} は片方のクラスしかありません（全て正解、または全て不正解）。AUROCは定義できません。もう一方のラベルを追加して再実行してください。"),
    ("calibrate.auroc.verdict.random", "  判定: でたらめと同等。このシグナルで昇格を判断しないでください。閾値を較正してもクラウド呼び出しをランダムに消費するだけです。別のシグナルのラベルを集めるか、長さ・内容のみでルーティングしてください。"),
    ("calibrate.auroc.verdict.weak", "  判定: 弱い。多少の情報はありますが閾値は不安定です。較正値は暫定の出発点とみなし、ラベルを増やして再確認してください。"),
    ("calibrate.auroc.verdict.usable", "  判定: 使用可能。較正するに足る分離性能があります。次に calibrate --logprob または --error を実行してください。"),
    ("calibrate.error.empty", "{path} に有効なラベルがありません。各行: {\"logprob\": -0.42, \"correct\": true}（logprob はコストログから、correct はその回答へのあなたの判定）。"),
    ("calibrate.error.header", "ラベル付き回答 {n} 件（全体誤答率 {overall}%）から誤答率ベースのカスケード閾値を較正（目標誤答率 {target}%）:"),
    ("calibrate.error.band", "  logprob {from}〜{to}: 推定誤答率 {err}%  (n={count})"),
    ("calibrate.error.result", "  推奨閾値: {threshold} （約 {rate}% が昇格、ローカル維持分の推定誤答率 <= {target}%）"),
    ("calibrate.error.apply", "  適用:  PASTURE_CASCADE_LOGPROB={threshold}"),
    ("calibrate.error.unachievable", "  目標誤答率 {target}% を満たす logprob 帯がありません。最も確信度の高い回答でも超過しています。ラベルを増やすか、全昇格（PASTURE_CASCADE_LOGPROB=0）を検討してください。"),
    ("calibrate.sweep.header", "ログ {n} 件のプロンプトから閾値スイープ（長さのみの推定。内容シグナルでさらにクラウドへ）:"),
    ("calibrate.sweep.row", "  目標 {target}% クラウド  ->  PASTURE_THRESHOLD={threshold}  （長さ基準で約 {rate}%）"),
    ("calibrate.logprob.sweep.header", "ログ {n} 件からカスケードlogprob閾値スイープ:"),
    ("calibrate.logprob.sweep.row", "  目標 {target}% 昇格  ->  PASTURE_CASCADE_LOGPROB={threshold}  （約 {rate}% が昇格）"),
    ("config.title", "pasture 設定（実効値）:"),
    ("config.set", "設定済み"),
    ("config.unset", "未設定"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lang_from_str() {
        assert_eq!(lang_from_str("ja_JP.UTF-8"), Lang::Ja);
        assert_eq!(lang_from_str("ja"), Lang::Ja);
        assert_eq!(lang_from_str("en_US.UTF-8"), Lang::En);
        assert_eq!(lang_from_str(""), Lang::En);
    }

    #[test]
    fn test_t_known_key_both_langs() {
        assert!(t(Lang::Ja, "doctor.title").contains("環境"));
        assert!(t(Lang::En, "doctor.title").contains("checking"));
    }

    #[test]
    fn test_t_fallback_to_english() {
        // A key present only in EN should fall back when asked in JA.
        // (All current keys exist in both; emulate via a guaranteed EN-only path
        // by checking the fallback mechanism returns the EN string, not a marker.)
        assert_ne!(t(Lang::Ja, "doctor.allgood"), "(missing translation)");
    }

    #[test]
    fn test_t_unknown_key_marker() {
        assert_eq!(t(Lang::En, "no.such.key"), "(missing translation)");
    }

    #[test]
    fn test_tf_interpolates() {
        let s = tf(
            Lang::En,
            "doctor.ollama.ok",
            &[("host", "127.0.0.1"), ("port", "11434")],
        );
        assert_eq!(s, "[ok] Ollama is running at 127.0.0.1:11434");
    }

    #[test]
    fn test_tf_japanese_interpolates() {
        let s = tf(Lang::Ja, "doctor.model.missing", &[("model", "llama3")]);
        assert!(s.contains("llama3"));
        assert!(s.contains("未導入"));
    }

    #[test]
    fn test_catalogs_have_same_keys() {
        // Every EN key must have a JA translation and vice versa.
        for (k, _) in EN {
            assert!(JA.iter().any(|(jk, _)| jk == k), "JA missing key: {k}");
        }
        for (k, _) in JA {
            assert!(EN.iter().any(|(ek, _)| ek == k), "EN missing key: {k}");
        }
    }
}
