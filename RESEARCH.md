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
| **IMP-21** | レイテンシDoS硬化（本文サイズ/接続時間/出力長の上限）— **本文サイズ上限(413)実装済(ADR-033)**、接続時間/出力長上限は残 | 7,9 | 2602.07878 / 2404.08509 | ✅ |
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

## Round 2 — 深掘りと実装ロードマップ

> ループ第2巡。第1巡の網羅調査を受け、**最優先5項目を設計レベルに落とし込む**。
> 各項目に「設計 / env / ファイル接点 / テスト / ゼロ依存維持」を付す。新規出典は
> 下の Sources にも追記。

### R2-1. IMP-13 — UCCI流の正誤較正（isotonic, std-only）

**追加根拠:** PAVA（Pool Adjacent Violators）は **O(n)・パラメトリック仮定なし**で
単調写像を学習でき、データが小さい時はPlattより過学習しやすい点に注意（→少数
ラベルでは保守的に）。PDAS変種はwarm-start可能でオンライン更新向き。

**設計:**
- `calibrate.rs` に `fit_isotonic(samples: &[(f64 /*logprob*/, bool /*correct*/)]) -> Vec<(f64,f64)>`
  を追加。PAVAでlogprob昇順→P(correct)の単調増加ブロックを生成（std::のみ、約40行）。
- 推論時: `p_error(logprob)` を区分線形補間で評価。`should_escalate = p_error > budget`。
- ラベル源: 既存18ケース eval（`eval.rs`）＋任意のユーザ提供 `pasture-labels.jsonl`。
- 出力は**助言**（現行 `calibrate` と同じ）: `PASTURE_CASCADE_LOGPROB` の代わりに
  `PASTURE_CASCADE_ERR_BUDGET=0.1` を推奨値として提示。

**env:** `PASTURE_CASCADE_ERR_BUDGET`（新）。既存 `PASTURE_CASCADE_LOGPROB` は後方互換で残置。
**ファイル接点:** `calibrate.rs`（PAVA＋補間）, `cascade.rs`（閾値判定の分岐）, `cli.rs`（`calibrate --error-budget`）。
**テスト:** 単調性（出力が非減少）、既知点で正答率一致、ラベル0件で安全フォールバック（現行率較正へ）。
**ゼロ依存:** ✅ オフライン・std数値のみ。

### R2-2. IMP-12 — ローカル埋め込みのセマンティックキャッシュ（opt-in）

**追加根拠:** 本番知見は **閾値0.90–0.95、0.92が定番**、FP上限は埋め込み性能で
**3–5%**。閾値は**実トラフィックで曲線を引いて**決める（合成不可）。vCache
(2502.03771) は「検証付き」キャッシュでFPを抑制、ドメイン特化埋め込み
(2504.02268) でヒット率改善。

**設計:**
- `cache.rs` に第2層 `SemanticCache`（opt-in）。キー＝local backendの `/v1/embeddings`
  で得たベクトル。cos類似 ≥ `threshold` でヒット。境界付き（FIFO）でベクトルを保持。
- **検証ステップ**（vCache準拠）: ヒット時に近傍距離を `cost log` に記録し、`stats` で
  FP代理指標（near-miss分布）を提示→ユーザが閾値を調整可能に。
- 機微プロンプトは**埋め込みもキャッシュもしない**（`privacy.rs` の既存規則を踏襲）。
- デフォルト無効＝既定ビルドはゼロ依存のまま（埋め込みはlocal HTTP、新crate不要）。

**env:** `PASTURE_SEMANTIC_CACHE=<n>`（容量, 0=無効）, `PASTURE_SEMANTIC_THRESHOLD=0.92`。
**ファイル接点:** `cache.rs`（cos類似・境界保持）, `backend.rs`（embeddings呼出）, `cost.rs`（near-miss記録）, `proxy.rs`（ルックアップ順序: exact→semantic→backend）。
**テスト:** cos計算の正確性、閾値境界（0.92で言い換えヒット/別意図ミス）、機微非キャッシュ、容量退避。
**ゼロ依存:** ✅（opt-in; localのみ; ベクタDB非使用）。

### R2-3. IMP-18 — プレフィックス保全＋プロバイダ・プロンプトキャッシュ活用

**追加根拠:** Anthropic は `cache_control:{type:"ephemeral"}`、参照順は
**tools→system→messages**、**読取=入力の0.1x / 書込=1.25x（1h TTLは2x）**、
**breakpoint最大4**。OpenAIは自動プレフィックスキャッシュ。安定プレフィックスを
壊さない要求整形が最大のコスト梃子。

**設計:**
- cloud送信時、`cloud.rs` の Anthropic shaping に `cache_control` を付与（system＋
  長い共通前置にbreakpoint, 最大4）。OpenAIは順序維持で自動キャッシュに委ねる。
- マルチターンで system/tools を**安定順序**に正規化（揮発的メタを末尾へ）。
- `stats` に「cache_creation/cache_read トークン」を集計（Anthropicレスポンスの
  `cache_creation` を解析）。

**env:** `PASTURE_PROVIDER_PROMPT_CACHE=1`（既定オン推奨, cloud feature時）。
**ファイル接点:** `cloud.rs`（リクエスト整形＋レスポンス使用量解析）, `cost.rs`（キャッシュ系トークン）, `proxy.rs`（メッセージ順序正規化）。
**テスト:** ペイロード整形のゴールデンテスト（breakpoint位置）、使用量解析、非cloudビルドで無効。
**ゼロ依存:** ✅（cloud featureゲート内のみ; 既定ビルド不変）。

### R2-4. IMP-20 — 軽量プロンプトインジェクション・ガード（公開時のみ, opt-in）

**追加根拠:** 字句/正規表現層は **~0.1ms**、PCFIは ALLOW/SANITIZE/BLOCK を
バックエンド到達前に判定し全攻撃遮断/FP0%/0.04ms。ただし**regex単独はFP 8–15%**
なので「第一層」に留め、強制ではなく**警告/ラベル**運用が無難。ゼロショット
埋め込みドリフト(2601.12359)や混合判定(2603.25176)は重い→既定は字句のみ。

**設計:**
- 非localhostバインド時のみ有効化（IMP-15と連動）。`proxy.rs` 入口に
  `classify_injection(text) -> Allow|Flag` の決定論的字句判定（"ignore previous",
  "respond as system", role切替, 既知エンコード/homoglyph）。
- 既定は**Flagをログ＋ヘッダ表示**（BLOCKは opt-in）。FP過多を避け、cost logに
  カテゴリのみ記録（本文は非ログ, I5踏襲）。

**env:** `PASTURE_INJECTION_GUARD=off|flag|block`（既定off; 公開時flag推奨）。
**ファイル接点:** 新規 `src/guard.rs`（純パターン＋テスト）, `proxy.rs`（入口フック）, `i18n.rs`（警告文言）。
**テスト:** 既知攻撃の検出、正常文のFP測定、off時ゼロオーバヘッド。
**ゼロ依存:** ✅（std正規表現相当を手書きパターンで; 新crate不要）。

### R2-5. 即効クイックウィン（IMP-8/9/10）の接点詳細

- **IMP-8 `/v1/models`・`/v1/embeddings`:** `proxy.rs:319` 付近のルート分岐に2本追加。
  `/v1/models` は config のlocal/cloudモデルIDを OpenAI list形で返す。`/v1/embeddings`
  は local backend にパススルー（`backend.rs`）。テスト: 形状・404解消。
- **IMP-9 リトライ＋フォールバック:** `cloud.rs` 送信を `retry(max=3, backoff=2^n, jitter)`
  でラップ。5xx/timeout/接続失敗のみ再試行。最終失敗で現行の local フォールバック。
  env `PASTURE_CLOUD_RETRY=3`。テスト: 一過性5xx→成功、恒久失敗→local。
- **IMP-10 tool calling:** `proxy.rs` のリクエスト解析で `tools`/`tool_choice` 検出→
  `routing.rs` の `add_hard_signal` 呼出（cloud寄せ）＋フィールド忠実転送。
  テスト: tools有り→cloud、フィールド保全。

### Round 2 まとめ
- **着手順の推奨:** IMP-8 → IMP-10 → IMP-9（クイックウィン, std-only, 1–2日規模）
  → IMP-22（推定誤差是正）→ IMP-13/12（研究的価値）→ IMP-18（コスト梃子）
  → IMP-15/20/16（公開運用）。
- すべて **既定のゼロ依存・単一バイナリ・プライバシー優先**を不変に保つ opt-in 設計。
- 閾値・予算は**ユーザの実ログで較正**する方針（合成データに依存しない）で一貫。

---

## Round 3 — 残り候補の設計レベル深掘り

> ループ第3巡。Round 2 で扱わなかった候補（IMP-19 / 26 / 23 / 27）を設計化し、
> IMP-21/22/24/25 を短く締める。新規出典は下の Sources に追記。

### R3-1. IMP-19 — 任意の可逆擬名化（reversible pseudonymization）送信モード

**追加根拠:** MemPrivacy は端末側で機微スパンを**型付きプレースホルダ**
（`<Email_1>`, `<Health_Info_1>`）に置換、原値↔プレースホルダの対応は**ローカル保存**し
セッション跨ぎで安定。PII Shield 方式は detect→redact→**restore**で、LLM が前後を
書換え/翻訳/要約してもプレースホルダ位置で原値を復元できる。Anonymous-by-Construction
(2603.17217) は型整合な代理値で流暢性を保つ。

**設計（Pastureの第3経路）:** 現状は「機微→**強制local**」が既定。これを保ちつつ、
local非搭載 or 明示opt-in時のみ次を提供：
1. `privacy.rs` の検出スパンを**型付きプレースホルダ**へ置換（既存7カテゴリを再利用）。
2. 原値マップは**プロセス内/ローカルのみ**（cost logにもディスクにも原値は書かない, I5）。
3. cloud応答受信後、プレースホルダを原値へ**復元**してクライアントへ返す。
4. クラウドには**原値が一切出ない**ことを不変条件としてテストで保証。

**env:** `PASTURE_PII_MODE=local|pseudonymize`（既定 `local`）。
**ファイル接点:** `privacy.rs`（span→placeholder, restore）, `proxy.rs`（送信前置換・応答後復元）, `cascade.rs`（擬名化時のみcloud許可）。
**テスト:** 往復で原値復元、cloudペイロードに原値非出現（負例検査）、マップ非永続、既定モードは挙動不変。
**ゼロ依存:** ✅（std文字列処理のみ）。**注意:** 復元は文字列マッチ依存→プレースホルダ命名を衝突しにくい形に。

### R3-2. IMP-26 — 予算アウェアな動的しきい値（cost-velocity breaker）

**追加根拠:** 2026の定石は **(1) TPM/TPD・日次/月次の累積spend上限、(2) 予算「率」に対する
ブレーカ**（絶対ドルでなく**ワークロード予算比**で発火＝価格改定に強い）、**(3) スパイク検知**
（単発が平均コストの50–100×ならパターン無しでも発火）、**(4) フォールバックで安価モデルへ**。

**設計:** Pasture は単一ユーザなので階層予算は不要。cost log（PII-free）を入力に：
- `PASTURE_BUDGET_DAILY_USD` を設定すると、当日の累積spendに対し**消化率**を算出。
- 消化率が上がるほど**ルーティング閾値を引上げ**（=より local 寄り）。線形 or 区分で。
- **スパイク検知:** 推定プロンプトが平均の N×（既定50×）超なら、その1件を local へ寄せる
  （文脈溢れの暴発を防ぐ）。
- 上限到達時は `block|local-only|warn` を選択（既定 `local-only`：cloudを止めても動作継続）。

**env:** `PASTURE_BUDGET_DAILY_USD`, `PASTURE_BUDGET_ACTION=local-only|warn|block`, `PASTURE_SPIKE_FACTOR=50`。
**ファイル接点:** `cost.rs`（当日集計・平均算出）, `routing.rs`（消化率→閾値補正・スパイク判定）, `cli.rs`（`stats` に予算消化率表示）。
**テスト:** 消化率0/50/100%での閾値変化、スパイク単発の local 化、上限到達時アクション、予算未設定で挙動不変。
**ゼロ依存:** ✅（cost logのみ・オフライン）。**ADR-015/023 と整合**（観測ログを較正に使う既存方針の延長）。

### R3-3. IMP-23 — OpenTelemetry GenAI 準拠の任意メトリクス出力

**追加根拠:** GenAI semconv の **client spans/metrics は2026初頭にstable化**。
標準メトリクス: `gen_ai.client.token.usage` / `operation.duration` /
`time_to_first_chunk` / `time_per_output_chunk`。属性: `gen_ai.request.model` /
`gen_ai.usage.input_tokens` / `output_tokens` / `gen_ai.response.finish_reasons`。
`input_tokens` は**cachedトークンを含む**。PII: **プロンプト本文をspanに載せない**。

**設計（ゼロ依存維持の肝）:** OTel SDK は依存になるため**採らない**。代わりに：
- **(既定)** IMP-16 の `/metrics`（Prometheus text）を **`gen_ai.*` 命名**で出力。
  既存 cost log の計数（route, model, tokens, spend, logprob分布）を写像。
- **(opt-in)** 手書きの **OTLP/HTTP(JSON) エクスポータ**を std HTTP で実装し、
  collector へ push（新crate不要）。属性は上記stable集合のみ、**本文は載せない**（I5）。

**env:** `PASTURE_OTEL_ENDPOINT`（未設定なら無効）, `PASTURE_METRICS=prometheus|otlp|off`。
**ファイル接点:** `cost.rs`（集計の単一ソース）, 新規 `src/metrics.rs`（Prometheus/OTLP整形）, `proxy.rs`（`/metrics` ルート, IMP-16と統合）。
**テスト:** 命名・型の準拠（gen_ai.*）、本文非出力、off時ゼロオーバヘッド、OTLP JSON整形のゴールデン。
**ゼロ依存:** ✅（既定Prometheusはstd; OTLPもstd HTTP・opt-in）。

### R3-4. IMP-27 — サプライチェーン強化（CI/リリース）

**追加根拠:** 2026のRust定石: **cargo audit + cargo deny を CI に**（脆弱性＋ライセンス方針）、
**cargo-auditable** で依存を実バイナリに埋込、**SBOM**（CycloneDX/SPDX, Syft）を毎ビルド発行、
**cosign キーレス署名**（Fulcio/Rekor）＋ **SLSA provenance**、`Cargo.lock` コミット
（**Pastureは実施済**）、Cargo.lockによる**再現ビルド**。

**設計:** 既定ビルドはゼロ依存ゆえ依存監査面は最小だが、`cloud` feature の TLS スタックが対象。
- `.github/workflows/ci.yml` に `cargo deny check` と `cargo audit` を追加（zero-dep と cloud の両 feature）。
- リリース job で `cargo auditable build`＋SBOM(CycloneDX) 添付、`cosign` キーレス署名（SLSA L1→L2）。
- ADR-010（依存ピン留め）・RELEASE_CHECKLIST と統合し、release-approval を人手ゲートのまま維持。

**ファイル接点:** `.github/workflows/ci.yml` / `release.yml`, `deny.toml`（新）, `RELEASE_CHECKLIST.md`。
**テスト/検証:** CIで deny/audit が赤を出すこと、SBOM 添付、署名検証手順を SECURITY.md に明記。
**ゼロ依存:** ✅（成果物には影響せず, CIのみ）。
> 注: 本リポの取込時、GitHub App権限の制約で `.github/workflows/*.yml` は push 不可だったため、
> 本項は**メンテナが手動適用**する前提（workflows権限付与 or 手動コミット）。

### R3-5. 残り候補の要約クローズ

- **IMP-21 レイテンシDoS硬化:** `proxy.rs` に本文サイズ上限（既定~1MB）、接続あたり処理時間上限、
  Content-Length検証、接続レート制限を追加。ADR-026（再帰深さ128）を本文長/レートへ拡張。env
  `PASTURE_MAX_BODY_BYTES` / `PASTURE_REQ_TIMEOUT_MS`。std-only。
- **IMP-22 fertility推定:** `routing.rs:estimate_tokens` を「スクリプト別係数表＋句読点/数字補正」へ。
  CJK=1.0、ラテン≈0.25/char、記号/空白別係数。係数は env でも上書き可。決定論維持・テスト追加。
- **IMP-24 出力長予測:** 軽量proxyモデルは依存増＝**現時点では非採用（deferred）**。代替として
  プロンプト長・タスク種別からの**ヒューリスティック見積り**のみ cost 予測に使用。
- **IMP-25 スキルプロフィール:** `config` に `[skills]` 表（例 `code = cloud`, `summarize = local`,
  `translate.ja = local`）を追加し、ハード信号より前段で参照。決定論・設定駆動。`routing.rs` に
  プロフィール照合を1段挿入。

### Round 3 まとめ（全候補の状態）
| IMP | 状態 | 既定ゼロ依存 |
|-----|------|:---:|
| 8/9/10/11 | 設計＋接点明記（Round2） | ✅ |
| 12/13/18/20 | 設計化（Round2） | ✅ |
| 19/23/26 | 設計化（Round3） | ✅ |
| 27 | CI設計（手動適用前提） | ✅ |
| 21/22/25 | 要約設計（Round3） | ✅ |
| 24 | 非採用（deferred） | ⚠️ |

全項目が **opt-in もしくは std-only** で、既定の単一・ゼロ依存・プライバシー優先バイナリを不変に保つ。
閾値・予算・較正は一貫して**ユーザの実 cost log**から導く（合成データ非依存）。

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

**Round 2 追加出典:**
- *isotonic/PAVA較正:* arxiv.org/abs/2006.05527（PAVA高速化）, /1508.02452（primal-dual active-set）
- *プロバイダ・プロンプトキャッシュ:* platform.claude.com/docs（cache_control ephemeral, 0.1x read）,
  openrouter.ai/docs（prompt caching）, docs.litellm.ai/docs/completion/prompt_caching
- *セマンティックキャッシュ閾値:* arxiv.org/abs/2502.03771（vCache 検証付き）, /2504.02268（ドメイン特化埋め込み）,
  portkey.ai/blog/semantic-caching-thresholds（0.92/FP3-5%）
- *インジェクション軽量防御:* arxiv.org/abs/2603.18433（PCFI）, /2605.06669（security-usability-latency）,
  /2601.12359（zero-shot embedding drift）, /2506.06384（pretrained+heuristic）, /2603.25176（LLM-as-Judge+MoM）

**Round 3 追加出典:**
- *可逆擬名化:* arxiv.org/abs/2603.17217（Anonymous-by-Construction）, MemPrivacy（edge-cloud reversible pseudonymization, 2026-05）,
  PII Shield（detect-redact-restore proxy）, dev.to/mukundakatta/llm-pii-redact
- *予算/コスト制御:* docs.litellm.ai/docs/proxy/users（budgets/rate limits）, portkey.ai/blog/rate-limiting-for-llm-applications,
  virtido.com/blog/ai-gateway-patterns-production-guide（cost-velocity breaker, spike detection）
- *OpenTelemetry GenAI:* opentelemetry.io/docs/specs/semconv/gen-ai/（spans/metrics stable）, gen-ai-metrics, gen-ai-spans
- *Rustサプライチェーン:* github.com/rust-secure-code/cargo-auditable, embarkstudios/cargo-deny, sigstore/cosign（keyless）,
  Syft SBOM（CycloneDX/SPDX）, SLSA provenance
