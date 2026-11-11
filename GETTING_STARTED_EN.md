# Getting started with Pasture

Pasture is a tool for **running AI on your own computer**.
No complicated setup is needed. It works together with Ollama, a free tool for running models locally.

- Everyday questions are handled **on your machine — free and private**.
- Only difficult questions or code are switched to the **cloud** when needed (optional).
- Questions containing personal information (email addresses, phone numbers, card numbers) are **never sent to the cloud automatically**.

Follow this guide and you will have it running, even if this is your first time.

---

## Just three terms

- **Ollama**: free software for running AI models on your computer. Pasture builds on it.
- **Model**: the AI itself (for example, `llama3`). You download it with Ollama and use it.
- **Proxy / base_url**: Pasture runs as a "reception desk". Point your AI app's connection address (base_url) at Pasture, and the app works as before.

---

## Get running in 3 steps

### 1. Install Ollama

Install it from [https://ollama.com](https://ollama.com).
It usually starts automatically after installation. If it doesn't, run `ollama serve` in a terminal.

### 2. Download a model

Run this in a terminal (it takes a few minutes):

```sh
ollama pull llama3.2
```

### 3. Start it with one command

```sh
pasture up
```

`pasture up` starts Ollama if needed, downloads the model automatically if you don't have it yet, and then starts the proxy. When it starts, it also shows the connection address (base_url) to set in your app.
If something goes wrong, run `pasture doctor` to check your setup (how to fix it appears under `[!!]`).

> Getting Pasture itself: the easiest way is to download the prebuilt binary for your OS from the [Release page](https://github.com/shizukutanaka/Pasture/releases/tag/v0.28.0) — no build needed (local-only build).
> To build from source: `git clone https://github.com/shizukutanaka/pasture && cd pasture && cargo build --release` (Rust is required; the binary is created at `target/release/pasture`. For cloud use, build with `cargo build --release --features cloud` instead.)

---

## Try it out

### Ask one question right away

```sh
pasture chat "What is the highest mountain in Japan?"
```

The answer streams out little by little.

### Use it from an app (start the proxy)

```sh
pasture serve
```

It listens at `http://127.0.0.1:8645`.
Point your OpenAI-compatible app's **connection address (base_url)** there.

Example (environment variables that work with many tools):

```sh
export OPENAI_BASE_URL="http://127.0.0.1:8645/v1"
export OPENAI_API_KEY="dummy"   # a dummy value is fine for local use
```

Then just use your app as usual. Pasture automatically routes each request to "local / cloud".

To see setup instructions for your app:

```sh
pasture connect            # list of supported apps
pasture connect cursor     # e.g. setup for Cursor
pasture connect openwebui  # e.g. setup for Open WebUI
pasture connect lmstudio   # use LM Studio as the engine
pasture calibrate          # recommend a threshold based on your usage
```

Not sure which model to install:

```sh
pasture models             # models recommended for your machine's RAM
```

---

## If something goes wrong (reading `pasture doctor`)

- **`[!!] Ollama is not reachable`**
  Ollama is not running. After installing it, start it with `ollama serve`.
- **`[!!] local model 'llama3' is not installed`**
  The model hasn't been downloaded yet. Run `ollama pull llama3.2`.
- **`[!!] proxy port ... is already in use`**
  The port is taken. You can start on another port:
  `PASTURE_LISTEN_ADDR=127.0.0.1:8646 pasture serve`

If in doubt, just run:

```sh
pasture doctor
```

---

## If you also want to use the cloud (optional)

If local-only is enough for you, you can skip this section.

To use automatic switching to the cloud (OpenAI / Anthropic), build with cloud support and put your own API key in an environment variable (keys are never written to logs).

```sh
cargo build --release --features cloud
export PASTURE_OPENAI_API_KEY="sk-..."      # if you use OpenAI
# or
export PASTURE_ANTHROPIC_API_KEY="sk-ant-..."  # if you use Anthropic
export PASTURE_CLOUD_PROVIDER="openai"       # or "anthropic"
```

Check that it is set up:

```sh
pasture doctor
```

If it shows `[ok] cloud fallback ready`, you're done.

---

## FAQ

- **Does it cost money?**
  Pasture itself and Ollama are free. Local processing costs nothing.
  You only pay the providers' fees when you actually use the cloud.
- **Are my questions sent anywhere?**
  By default, everything is processed locally. A question is only sent when it is routed to the cloud.
  Questions containing personal information are automatically kept local and never sent to the cloud.
- **I want to see the breakdown of what I used**
  Run `pasture stats` to see local / cloud / cache counts and a rough cost estimate.

---

## Language

Japanese and English are supported. Switch with an environment variable:

```sh
PASTURE_LANG=ja pasture doctor   # Japanese
PASTURE_LANG=en pasture doctor   # English
```
(If not specified, it is selected automatically from your terminal's language setting.)

## Easy install (optional)

There is also a script that does the build and placement together:

```sh
sh install.sh          # macOS / Linux
# Windows: powershell -ExecutionPolicy Bypass -File install.ps1
```

---

If you're stuck, remember just one thing: `pasture doctor`.
