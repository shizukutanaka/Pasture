# 改善リサーチ ドキュメント (RESEARCH.md)

> 本書は Pasture を **10カテゴリー** に分け、各カテゴリーで **arXiv / GitHub から
> 約10件ずつ**関連情報を収集し、そこから **Pasture の改善点** を洗い出したものです。
> 既存の整理済みバックログ（IMP-1〜7 出荷済み、IMP-8〜17 提案）は
> [COMPETITIVE.md](COMPETITIVE.md) を参照。本書はその上流の **一次調査** にあたり、
> 新規候補 **IMP-18〜IMP-27** を末尾に集約します。
>
> 注: arXiv 番号は調査時点（2026-06）の検索結果に基づく。実装着手時は各論文を
> 再確認すること。出典URLは末尾「Sources」にまとめています。

---

## カテゴリー1 — ルーティング / モデル選択

**現状（Pasture）:** 決定論的・ハードウェア適応のルールルーター（`routing.rs`）。
ハード信号（コードフェンス, 推論/整形マーカー, 多問, 数式密度）+ スクリプト対応の
トークン長しきい値。学習型ルーターは持たない（ADR-002）。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | LLM Router: Rethinking Routing with Prefill Activations (2603.20895) | arXiv | 内部prefill活性で性能予測。Encoder-Target分離で公開重みエンコーダが閉源モデルの正誤を予測。 |
| 2 | LLMRouterBench (2601.07206) | arXiv | 400K件・33モデルの大規模ルーティング基準＋10ベースライン統一枠組。 |
| 3 | MMR-Bench: Multimodal LLM Routing (2601.17814) | arXiv | マルチモーダル入力のルーティング評価。将来の画像対応の指針。 |
| 4 | Zero-Shot LLM Routing via Universal Latent Space (2601.06220) | arXiv | モデルロックイン回避。学習なしで新モデルへ転移するルーター。 |
| 5 | ICL-Router: In-Context Learned Model Representations (2510.09719) | arXiv | モデル表現をICLで獲得。再学習不要でモデル追加。 |
| 6 | Trust by Design: Skill Profiles for Cost-Aware Routing (2602.02386) | arXiv | モデルごとの「スキルプロフィール」で透明・予算制約付き選択。 |
| 7 | lm-sys/RouteLLM | GitHub | 学習型ルーター4種（MF/BERT/causal）。Arena選好80k件で学習。 |
| 8 | vllm-project/semantic-router | GitHub | コスト/プライバシ/遅延/安全のシグナル合成ルーティング（v0.2 Athena）。 |
| 9 | aurelio-labs/semantic-router | GitHub | 埋め込み空間での超高速決定層。 |
| 10 | NVIDIA-AI-Blueprints/llm-router, Not-Diamond/awesome-ai-model-routing, ulab-uiuc/LLMRouter | GitHub | 本番Blueprint・キュレーション一覧・16+ルーター実装ライブラリ。 |

**改善点:**
- **(a) スキルプロフィール型ルーティング**（#6, #1）: 現状の「難しさ」一元判定を、
  タスク種別×モデル強みのプロフィールに拡張（コード→cloud、要約→local 等を
  設定可能テーブルで）。決定論を保ったまま精度向上。→ **IMP-25**。
- **(b) ゼロショット/プラガブルなモデル追加**（#4, #5）: モデル追加時に再学習が
  要らない Pasture の強みを、複数 local/cloud モデルの選択にも拡張（現状は二者択一）。
- **(c) ベンチで裏付け**（#2, #7）: 自前18ケース eval を LLMRouterBench/RouterBench
  形式で外部検証（→ IMP-17 と統合）。

---

## カテゴリー2 — カスケード / エスカレーション / 信頼度較正

**現状:** opt-in カスケード（`cascade.rs`）。ローカル平均logprob閾値（ADR-027）＋
テキストヒューリスティックのフォールバック。`calibrate --logprob` は*エスカレーション
率*の分位較正（ADR-028）で、*正誤*較正ではない。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | UCCI: Calibrated Uncertainty for Cost-Optimal Cascade Routing (2605.18796) | arXiv | トークンmargin不確実性→誤り確率を**isotonic回帰**で較正。NERでコスト31%減/ECE 0.12→0.03。 |
| 2 | Towards a Cascaded LLM Framework (Human-AI) (2506.11887) | arXiv | surrogateトークン確率で検証→人間含む階層エスカレーション。 |
| 3 | I-CALM: Confidence-Aware Abstention (2604.03904) | arXiv | 信頼度→棄権/被覆のリスク制御。言語的信頼度の有用性。 |
| 4 | Mean log-probability confidence (2605.02241) | arXiv | 平均logprobが訓練不要の強い局所信頼度（ADR-027の根拠）。 |
| 5 | FrugalGPT (2305.05176) | arXiv | カスケード＋スコアラの原典（ADR-011根拠）。 |
| 6 | Budget-Constrained Policy Learning for Cascades (2404.13082) | arXiv | 文脈依存カスケードを予算制約の方策学習で最適化。 |
| 7 | Dynamic Routing & Cascading Survey (2603.04445) | arXiv | 不確実性量化パラダイムの位置づけ。 |
| 8 | Pay for Hints, Not Answers (2601.22132) | arXiv | 答えでなく「ヒント」を買う費用効率推論。部分エスカレーション。 |
| 9 | Edge-Cloud-Expert Cascades for Telecom (2512.20012) | arXiv | logits直アクセスで認識的不確実性の較正が向上。 |
| 10 | UCCI HTML版 / 実装ノート | arXiv | isotonicの単調写像はstd Rustで実装可能。 |

**改善点:**
- **(a) UCCI流の正誤較正へ昇格**（#1, #9）: `calibrate --logprob` を「目標
  エスカレーション率」から「目標精度（P(誤り)>予算で昇格）」へ。18ケース eval +
  任意ユーザラベルで単調isotonic写像をstdのみで学習。→ **IMP-13 を具体化**。
- **(b) 棄権（abstain）モードの導入**（#3）: 機微で local も自信が低い場合、誤答より
  「分からない／クラウド不可」を明示する選択肢。安全側のUX。
- **(c) 部分エスカレーション**（#8, #2）: 全文ではなく要約/ヒントだけクラウド照会で
  コスト最小化（将来の研究的拡張）。

---

## カテゴリー3 — キャッシング（厳密 / セマンティック / プレフィックス・KV）

**現状:** opt-in の**厳密一致**キャッシュ（`cache.rs`、FIFO境界、機微は非キャッシュ）。
セマンティック/プレフィックス・キャッシュは無し。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | Semantic-Aware Eviction for Prefix Caches (2605.18825) | arXiv | 「全トークンが等価でない」前提の意味的退避方策。 |
| 2 | Don't Break the Cache: Prompt Caching for Agentic Tasks (2601.06007) | arXiv | マルチターンでプレフィックス再利用を壊さない要求整形。 |
| 3 | SemShareKV: KVCache Sharing via Token-Level LSH (2509.24832) | arXiv | 類似プロンプトのKV共有をLSHで。 |
| 4 | KVFlow: Prefix Caching for Multi-Agent Workflows (2507.07400) | arXiv | エージェント連携でのプレフィックス再利用。 |
| 5 | KVShare: Semantic-Aware KV Cache Sharing (2503.16525) | arXiv | 同一トークン再利用＋差分再計算。 |
| 6 | KV Cache Recycling for Low-Param LLMs (2512.11851) | arXiv | 低パラメータ機の文脈容量拡張。 |
| 7 | Prompt Cache: Modular Attention Reuse (2311.04934) | arXiv | モジュール式アテンション再利用の原典。 |
| 8 | Semantic Caching for LLM Embeddings (2603.03301) | arXiv | cos/L2類似、閾値~0.92、FP監視。 |
| 9 | Efficient Prompt Caching via Embedding Similarity (2402.01173) | arXiv | 埋め込み類似キャッシュの理論。 |
| 10 | GPT Semantic Cache (2411.05276) / zilliztech/GPTCache | arXiv+GitHub | API呼出を約60-69%削減。実装の定番。 |

**改善点:**
- **(a) ローカル埋め込みによるセマンティックキャッシュ**（#8-10）: local backendの
  `/v1/embeddings` を再利用、cos≥閾値でヒット、近傍距離をログしFP監視。opt-in・
  デフォルトはゼロ依存維持。機微は非キャッシュ継続。→ **IMP-12 を具体化**。
- **(b) プレフィックス保全リクエスト整形**（#1, #2, #7）: マルチターンで system/
  共通プレフィックスを安定化し、**プロバイダ側プロンプトキャッシュ**（OpenAI/
  Anthropic、読取$0.30 vs $3.00/M）を効かせる。コスト最大の梃子。→ **IMP-18**。
- **(c) キャッシュキー正規化**（GPTCache動機）: 空白/大小/末尾正規化で近似ヒット率
  向上、埋め込み不要のつなぎ。→ **IMP-11**。

---

## カテゴリー4 — プライバシー / PII / ローカルファースト

**現状:** std-only分類器（`privacy.rs`、7カテゴリ：keyword/email/ip/credit_card/
phone/api_key/jwt、ラベルのみ・値は非ログ）。機微は**強制local**、`--cloud`も上書き。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | PRISM (2511.22788) | arXiv | 機微分類ルーティングの根拠（ADR-007）。 |
| 2 | Casper: Prompt Sanitization (Web LLM) (2408.07004) | arXiv | WebGPUローカルLLMでPII特定（98.5%）＋警告。 |
| 3 | Privacy Preserving Prompt Engineering: Survey (2404.06001) | arXiv | 手法の俯瞰（匿名化/desensitize/DP）。 |
| 4 | Anti-adversarial Desensitizing Prompts (2505.01273) | arXiv | 機微語を[MASK]→置換生成。 |
| 5 | Resource-Constrained Sanitization (2411.11521) | arXiv | クライアント側SLMで送信前サニタイズ。 |
| 6 | Operationalizing Data Minimization (2510.03662) | arXiv | 効用維持で最小開示を探索するアルゴリズム。 |
| 7 | PromptObfus (masked LM desensitization) | arXiv | 機微語マスク化の具体手法。 |
| 8 | Hide and Seek (HaS) | arXiv | 端末でHide-Model匿名化→クラウド照会。 |
| 9 | microsoft/presidio | GitHub | 実運用PII検出・匿名化の定番OSS（パターン拡充の参考）。 |
| 10 | OWASP LLM Top 10 (機微情報漏えい) | 標準 | LLM06 等、漏えい対策の基準。 |

**改善点:**
- **(a) 任意のサニタイズ送信モード**（#2,#4-8）: 現状「機微→強制local」に加え、
  「機微語を局所マスク化してからcloud」を opt-in 提供（local非搭載でも機微タスクを
  処理可能に）。値は決して送らない方針との整合に注意。→ **IMP-19**。
- **(b) 検出パターンの継続強化**（#9, presidio準拠）: 住所/個人番号/IBAN/
  追加トークン形式など false-negative を継続監査（ADR-024の方針＝過検出寄り）。
- **(c) データ最小化指標の可視化**（#6）: cost log に「機微カテゴリ別の local 化件数」
  を集計し、漏えい防止の実効を stats で提示（値は非ログ継続）。

---

## カテゴリー5 — ローカル推論バックエンド

**現状:** Ollama（既定, NDJSONストリーム）＋ OpenAI互換（LM Studio/llama.cpp/vLLM/
LocalAI、SSE＋logprobs）。`PASTURE_LOCAL_BACKEND` で選択（ADR-020/021）。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | ggml-org/llama.cpp (llama-server) | GitHub | OpenAI互換 /v1、embeddings・top_logprobs対応。 |
| 2 | Ollama | GitHub/Tool | 既定。/v1互換、52M+ pulls・170k★。 |
| 3 | vLLM (v0.21, 2026-05) | GitHub/Tool | OpenAI/Anthropic/gRPC。本番サービング。 |
| 4 | LM Studio | Tool | /v1 chat/completions/embeddings/models をREST提供。 |
| 5 | LocalAI | GitHub | OpenAI互換のセルフホスト集約。 |
| 6 | HuggingFace TGI | GitHub | 高スループット推論サーバ。 |
| 7 | llamafile | GitHub | 単一ファイル配布のローカル推論。 |
| 8 | SGLang | GitHub | 高効率サービング（RadixAttention）。 |
| 9 | MLX (Apple) | GitHub | Apple Silicon最適化（5月更新で機能追加）。 |
| 10 | mostlygeek/llama-swap | GitHub | 複数モデルのオンデマンド切替プロキシ。 |

**改善点:**
- **(a) `/v1/models` の動的列挙**（#1,#4）: バックエンドの models を問い合わせて
  Pasture の `/v1/models` に反映（クライアントの接続時プローブ対策）。→ **IMP-8**。
- **(b) モデルスワップ連携**（#10）: 大小モデルのオンデマンド切替（llama-swap等）を
  想定したヘルスチェック/ウォームアップ（`doctor`/`up` 拡張）。
- **(c) バックエンド能力の自動検出**: logprobs/embeddings の対応可否を起動時に
  検出し、カスケード/セマンティックキャッシュの自動有効化判定に使う。

---

## カテゴリー6 — クラウド / マルチプロバイダ ゲートウェイ / コスト最適化

**現状:** feature-gated の OpenAI/Anthropic（BYOK, HTTPS, SSE）。クラウド失敗時は
local へフォールバックのみ。リトライ/複数プロバイダ・フェイルオーバは無し。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | BerriAI/litellm | GitHub | 100+プロバイダ、retry/fallback、ロードバランス、コスト追跡（40k★）。 |
| 2 | Portkey-AI/gateway | GitHub | 1600+ LLM、semantic cache、ガードレール、可観測性。 |
| 3 | OpenRouter | Tool | マーケットプレース型の単一API。 |
| 4 | Kong AI Gateway | Tool | 認証/レート制限/フィルタ統合のAPIゲートウェイ。 |
| 5 | Bayesian Orchestration of Multi-LLM Agents (2601.01522) | arXiv | 5モデル協調でコスト34%減。 |
| 6 | Cost-Aware Model Orchestration (2512.01099) | arXiv | データ駆動選択方策がブラックボックスLLM判断に優越。 |
| 7 | Budget-Aware Value Tree Search (2603.12634) | arXiv | 残予算比で探索→活用へ遷移。 |
| 8 | Fast Heterogeneous Serving (SLO制約) (2604.07472) | arXiv | メモリ/遅延/誤り/予算制約下の割当を1秒未満で。 |
| 9 | Skill Profiles for Cost-Aware Routing (2602.02386) | arXiv | 予算制約付き性能最大化の選択。 |
| 10 | FrugalGPT (2305.05176) | arXiv | 費用効率推論の原典。 |

**改善点:**
- **(a) 一時エラーのリトライ＋プロバイダ・フォールバック連鎖**（#1,#2）: 5xx/timeout
  で指数バックオフ再試行、順序付きプロバイダへ降格。最後の砦として現状の
  「cloud失敗→local」を維持。std-only。→ **IMP-9**。
- **(b) 予算アウェアな閾値**（#5-9）: 月次/セッション予算を設定し、消化率で
  しきい値を動的調整（残予算が減るほど local 寄り）。cost log を入力に。→ **IMP-26**。
- **(c) コスト追跡の精緻化**: プロバイダ別実価格表を設定可能にし、stats の spend を
  実勢に合わせる（現状 cloud は概算）。

---

## カテゴリー7 — プロキシ / サーバ実装（HTTP・ストリーミング・並行性・OpenAI互換）

**現状:** std-onlyのHTTP/1.1。境界付きワーカープール（2..=32, `proxy.rs`）。
エンドポイントは `/v1/chat/completions` と `/health` のみ。JSONパーサに深さ上限
（ADR-026）。`/v1/models`・`/v1/embeddings`・tool calling 無し。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | Rethinking Latency DoS: Attacking the Serving Framework (2602.07878) | arXiv | サービング枠組へのレイテンシDoS。境界/上限の重要性。 |
| 2 | Sarathi-Serve: chunked-prefill (2403.02310) | arXiv | スループット/遅延トレードオフのスケジューリング思想。 |
| 3 | Prefill-Decode Multiplexing (2504.14489) | arXiv | 高goodputの多重化。 |
| 4 | SSJF: Proxy Model Seq-Length Prediction (2404.08509) | arXiv | 出力長予測で短ジョブ優先スケジュール。 |
| 5 | BucketServe: Dynamic Batching (2507.17120) | arXiv | 長さ別バケットの動的バッチ。 |
| 6 | ConServe: GPU Harvesting Co-Serving (2410.01228) | arXiv | オンライン/オフライン同居。 |
| 7 | Efficient Serving for Agentic Workflows (2603.16104) | arXiv | エージェント負荷のサービング最適化。 |
| 8 | OpenAI API 互換仕様（chat/completions, models, embeddings） | 標準 | クライアント互換の最低要件。 |
| 9 | SSE / HTTP/1.1 chunked transfer | 標準 | ストリーミング実装の基礎。 |
| 10 | hyper / axum 等（参考、Pastureは非採用） | GitHub | 非同期実装の比較対象（ゼロ依存方針で不採用）。 |

**改善点:**
- **(a) API面の互換拡充**（#8）: `/v1/models`・`/v1/embeddings` を追加。多くの
  クライアントが接続時 `/v1/models` を叩き、無いと認識失敗。→ **IMP-8**。
- **(b) tool/function calling のパススルー＋ルーティング考慮**（#8）: `tools`/
  `tool_choice` を検出しハード信号化（cloudへ）＋忠実転送。→ **IMP-10**。
- **(c) レイテンシDoS硬化**（#1,#4,#5）: 本文サイズ上限・接続あたり時間上限・
  出力長見積りでの早期打切り。再帰深さ上限（ADR-026）を本文長/接続レートにも拡張。
  → **IMP-21**。

---

## カテゴリー8 — 評価 / ベンチマーク / 可観測性

**現状:** 18ケースの offline eval（`eval.rs`）＋しきい値スイープ。cost log（JSONL,
PII-free）＋ `stats`（cloud率・cacheヒット率・spend・logprob分布）。ライブ metrics
エンドポイント無し。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | RouterBench (2403.12031) | arXiv | 405k推論結果のルーティング基準（コスト×性能）。 |
| 2 | LLMRouterBench (2601.07206) | arXiv | 400k/33モデル/10ベースラインの統一枠組。 |
| 3 | VL-RouterBench (2512.23562) | arXiv | 視覚言語モデルのルーティング評価。 |
| 4 | MMR-Bench (2601.17814) | arXiv | マルチモーダルルーティング基準。 |
| 5 | OpenTelemetry GenAI semantic conventions | 標準 | LLM呼出の標準トレース属性。 |
| 6 | traceloop/openllmetry | GitHub | OTel準拠のLLM可観測性。 |
| 7 | langfuse/langfuse | GitHub | トレース/評価/コストのOSSダッシュボード。 |
| 8 | Helicone | GitHub/Tool | プロキシ型可観測性・コスト追跡。 |
| 9 | Prometheus text exposition format | 標準 | `/metrics` の出力形式。 |
| 10 | RouterBench OpenReview | レビュー | 評価設計の議論。 |

**改善点:**
- **(a) ライブ metrics エンドポイント**（#5,#6,#9）: `/metrics`（Prometheus）または
  `/v1/stats`（JSON）で cost log と同じ計数を即時公開。JSONL再解析不要。→ **IMP-16**。
- **(b) OpenTelemetry GenAI 準拠の任意エクスポート**（#5,#6）: OTel属性で
  span/メトリクス出力（opt-in、デフォルトはゼロ依存維持）。→ **IMP-23**。
- **(c) 外部ベンチ・ローダ**（#1,#2）: RouterBench/LLMRouterBench 形式の
  ラベル付き集合を読み込み、公開基準でルーティング品質を検証。→ **IMP-17**。

---

## カテゴリー9 — セキュリティ / デプロイ

**現状:** 既定 localhost バインド・認証/レート制限なし（CHANGELOG明記の将来課題）。
クラウド鍵は BYOK・非ログ。JSON再帰深さ上限のみ（ADR-026）。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | Prompt Control-Flow Integrity (PCFI) (2603.18433) | arXiv | 役割切替検出＋字句ヒューリスティックのゲートウェイ。全攻撃遮断/FP0%/0.04ms。 |
| 2 | Deterministic Security for Non-Deterministic AI (2602.10481) | arXiv | 文脈/プロンプト保護の決定論的防御。 |
| 3 | Generative Application Firewall (GAF) (2601.15824) | arXiv | ネットワーク〜意味層のポリシ強制。 |
| 4 | Encrypted Prompt (2503.23250) | arXiv | 不正アクションに対する権限付きプロンプト。 |
| 5 | Latency DoS on Serving (2602.07878) | arXiv | サービング枠組へのDoS（境界の必要性）。 |
| 6 | OWASP LLM Top 10 | 標準 | プロンプトインジェクション/漏えい等の基準。 |
| 7 | protectai/llm-guard | GitHub | 入出力スキャン（PII/injection）のOSS。 |
| 8 | protectai/rebuff | GitHub | プロンプトインジェクション検出。 |
| 9 | NVIDIA/NeMo-Guardrails | GitHub | 会話ガードレール枠組。 |
| 10 | Kong AI Gateway / API7 | Tool | 認証・レート制限・フィルタの実装参照。 |

**改善点:**
- **(a) 任意のベアラ認証＋トークンバケット・レート制限**（#3,#10）: 非localhost
  公開時に有効化（定数時間比較・fail-closed）。std-only・既定無効。→ **IMP-15**。
- **(b) 軽量プロンプトインジェクション・ガード**（#1,#2）: 公開時の入口で
  役割切替/既知パターンの字句ヒューリスティック（決定論・低オーバヘッド）。→ **IMP-20**。
- **(c) サプライチェーン**: 既に `cloud` feature の依存をピン留め（ADR-010）。
  cargo-deny/SBOM・署名検証を CI に追加（RELEASE_CHECKLIST と統合）。→ **IMP-27**。

---

## カテゴリー10 — UX / オンボーディング / CLI / i18n / トークナイゼーション

**現状:** `doctor`/`up`/`connect`/`models` 等の親切系コマンド、EN/JA i18n（キー
パリティ強制, ADR-017）、スクリプト対応トークン推定（ADR-022, CJK≈1tok/char）。

| # | 出典 | 種別 | 関連性 |
|---|------|------|--------|
| 1 | The Token Tax: Systematic Bias in Multilingual Tokenization (2509.05486) | arXiv | fertilityが系列長・コストを膨張。CJK不利。 |
| 2 | Beyond Fertility: STRR metric (2510.09947) | arXiv | fertilityを超える割当指標。推定精度の参考。 |
| 3 | Accelerating Excessively Tokenized Languages (2401.10660) | arXiv | 過分割言語の高速化。 |
| 4 | IndicSuperTokenizer (2511.03237) | arXiv | 言語別pre-tokenizeでfertility改善（手法の示唆）。 |
| 5 | i18next/i18next-cli | GitHub | 抽出/型安全/同期/lintを統合したCLI（キー管理の参考）。 |
| 6 | better-i18n / MCP連携 | GitHub | AIアシスタントから翻訳管理（将来のDX）。 |
| 7 | openclaw onboard | GitHub/Tool | 段階的オンボーディングCLIのUX参考。 |
| 8 | bradAGI/awesome-cli-coding-agents | GitHub | 端末ネイティブAIツールのUX潮流。 |
| 9 | llm-calculator tokenization benchmark | Tool | トークナイザ速度/効率の実測。 |
| 10 | tiktoken / HF tokenizers（参考、非採用） | GitHub | 厳密トークン数の比較対象（ゼロ依存方針で非採用）。 |

**改善点:**
- **(a) fertilityベースのトークン推定精緻化**（#1-4）: 現状の「CJK=1tok/char」を、
  スクリプト別係数＋句読点/数字の補正に拡張し、ルーティング長判定の系統誤差を低減。
  ゼロ依存・決定論維持。→ **IMP-22**。
- **(b) i18nキー管理のCLI化**（#5）: `i18n.rs` のキーパリティ検査を `pasture` 
  サブコマンド/CIに昇格（欠落キーを検出して提示）。
- **(c) オンボーディングの一層の自動化**（#7,#8）: `doctor`→自動修復提案の対話化、
  `connect` 対応アプリの拡充（最新クライアントの追従）。

---

## 新規候補の集約（IMP-18 〜 IMP-27）

> IMP-8〜17 は [COMPETITIVE.md](COMPETITIVE.md) に既出。本調査で新たに浮上した候補を
> 追加。すべて「**既定のゼロ依存ビルドを崩さない**（std-only もしくは opt-in）」設計を
> 前提とする。

| IMP | 概要 | カテゴリ | 根拠（arXiv/peer） | 既定ゼロ依存 |
|-----|------|----------|----------------------|:---:|
| **IMP-18** | プレフィックス保全リクエスト整形＋プロバイダ・プロンプトキャッシュ活用 | 3 | 2601.06007 / 2311.04934 / 2605.18825 | ✅ |
| **IMP-19** | 任意のローカルSLMサニタイズ送信モード（機微語マスク化→cloud） | 4 | Casper 2408.07004 / HaS / 2510.03662 | ✅(opt-in) |
| **IMP-20** | 公開時の軽量プロンプトインジェクション・ガード（字句/役割） | 9 | PCFI 2603.18433 / 2602.10481 | ✅ |
| **IMP-21** | レイテンシDoS硬化（本文サイズ/接続時間/出力長の上限） | 7,9 | 2602.07878 / 2404.08509 | ✅ |
| **IMP-22** | fertilityベースのトークン推定精緻化（スクリプト別係数） | 1,10 | 2509.05486 / 2510.09947 | ✅ |
| **IMP-23** | OpenTelemetry GenAI 準拠の任意メトリクス/トレース出力 | 8 | OTel GenAI / openllmetry | ✅(opt-in) |
| **IMP-24** | 出力長予測（軽量proxyモデル）でルーティング/コスト見積り | 1,7 | SSJF 2404.08509 | ⚠️(要検討) |
| **IMP-25** | スキルプロフィール型ルーティング（タスク×モデル強み表） | 1,6 | 2602.02386 / 2603.20895 | ✅ |
| **IMP-26** | 予算アウェアな動的しきい値（残予算でlocal寄りに） | 2,6 | 2601.01522 / 2603.12634 / FrugalGPT | ✅ |
| **IMP-27** | サプライチェーン強化（cargo-deny / SBOM / 署名検証をCIに） | 9 | OWASP / ADR-010 | ✅ |

### 優先度の所見
- **即効・低リスク（Tier 1）:** IMP-8/9/10/11（COMPETITIVE.md）＋ **IMP-22**（推定誤差是正）
  ＋ **IMP-21**（DoS硬化）。いずれも std-only。
- **研究的価値（Tier 2）:** **IMP-12/13**（セマンティックキャッシュ・正誤較正）、
  **IMP-18**（プレフィックスキャッシュ）、**IMP-25**（スキルプロフィール）。
- **公開運用向け（Tier 3）:** **IMP-15/16/20/23/26/27**。すべて opt-in で既定の
  単一・ゼロ依存・プライバシー優先バイナリを維持。

### 非目標（哲学維持のため不採用）
ベクタDB/Redis必須化、マルチテナント基盤、GPU学習が要る学習型ルーター、
プロンプト本文のログ化、非同期Webフレームワーク依存。これらは Pasture の
「ゼロ依存・単一ユーザ・ローカル/プライバシー優先」の核を損なうため採らない。

---

## Sources（主要URL）

**ルーティング:** arxiv.org/abs/2603.20895, /2601.07206, /2601.17814, /2601.06220,
/2510.09719, /2602.02386 ・ github.com/lm-sys/RouteLLM, /vllm-project/semantic-router,
/aurelio-labs/semantic-router, /NVIDIA-AI-Blueprints/llm-router, /Not-Diamond/awesome-ai-model-routing,
/ulab-uiuc/LLMRouter
**カスケード/較正:** arxiv.org/abs/2605.18796, /2506.11887, /2604.03904, /2605.02241,
/2305.05176, /2404.13082, /2603.04445, /2601.22132, /2512.20012
**キャッシュ:** arxiv.org/abs/2605.18825, /2601.06007, /2509.24832, /2507.07400,
/2503.16525, /2512.11851, /2311.04934, /2603.03301, /2402.01173, /2411.05276 ・ github.com/zilliztech/GPTCache
**プライバシー:** arxiv.org/abs/2511.22788, /2408.07004, /2404.06001, /2505.01273,
/2411.11521, /2510.03662 ・ github.com/microsoft/presidio ・ OWASP LLM Top 10
**ローカルBE:** github.com/ggml-org/llama.cpp, Ollama, vLLM, LM Studio, LocalAI, TGI,
llamafile, SGLang, MLX, mostlygeek/llama-swap
**クラウド/コスト:** github.com/BerriAI/litellm, /portkey-ai/gateway, OpenRouter, Kong ・
arxiv.org/abs/2601.01522, /2512.01099, /2603.12634, /2604.07472, /2602.02386, /2305.05176
**プロキシ/サーバ:** arxiv.org/abs/2602.07878, /2403.02310, /2504.14489, /2404.08509,
/2507.17120, /2410.01228, /2603.16104
**評価/可観測:** arxiv.org/abs/2403.12031, /2601.07206, /2512.23562, /2601.17814 ・
OpenTelemetry GenAI ・ github.com/traceloop/openllmetry, /langfuse/langfuse, Helicone
**セキュリティ:** arxiv.org/abs/2603.18433, /2602.10481, /2601.15824, /2503.23250,
/2602.07878 ・ github.com/protectai/llm-guard, /protectai/rebuff, /NVIDIA/NeMo-Guardrails ・ OWASP
**UX/i18n/トークン:** arxiv.org/abs/2509.05486, /2510.09947, /2401.10660, /2511.03237 ・
github.com/i18next/i18next-cli, /better-i18n, /bradAGI/awesome-cli-coding-agents
