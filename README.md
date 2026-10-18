<p align="center">
  <img src="src-tauri/icons/source/icon.svg" width="96" alt="Retranslator logo">
</p>

<h1 align="center">Retranslator</h1>

<p align="center">
  <a href="https://github.com/neodym121/retranslator/releases/latest"><img src="https://img.shields.io/github/v/release/neodym121/retranslator?color=eeeeee&labelColor=101010" alt="Release"></a>
  <a href="https://github.com/neodym121/retranslator/actions/workflows/build.yml"><img src="https://img.shields.io/github/actions/workflow/status/neodym121/retranslator/build.yml?label=build&labelColor=101010" alt="Build"></a>
  <img src="https://img.shields.io/badge/platform-Windows-eeeeee?labelColor=101010" alt="Windows">
  <img src="https://img.shields.io/badge/Tauri-2-eeeeee?logo=tauri&labelColor=101010" alt="Tauri 2">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/neodym121/retranslator?color=eeeeee&labelColor=101010" alt="MIT"></a>
</p>

<p align="center"><b>English</b> · <a href="README.ru.md">Русский</a></p>

A local Anthropic-compatible gateway for Claude Desktop's third-party inference mode. Claude Desktop sees
Claude-style model ids (`claude-sonnet-5.5`), and each request goes to the provider you pick under its real id
(`deepseek-v4.1-flash`, `gpt-…`, `gemini-…` and so on).

## Features

- Any OpenAI-compatible provider: presets for OpenAI, Google Gemini, OpenRouter and PolzaAI, or your own base URL.
- Full Anthropic ⇄ OpenAI conversion: text, images, PDFs, tools, thinking, streaming, errors, token counting.
- Model rules: local id → provider + real id, with display name, tier, context size and the 1M variant.
- Listens on `127.0.0.1` only. API keys live in Windows Credential Manager.
- Request log, tray icon, Russian and English interface.

## Download

[Latest release](https://github.com/neodym121/retranslator/releases/latest): the `.msi` installer or the portable
`.exe` (one file, no install). Windows 10/11 with WebView2.

## Connect Claude Desktop

1. Help → Troubleshooting → Enable Developer Mode.
2. Developer → Configure Third-Party Inference → Inference provider: **Gateway**.
3. Copy the Gateway base URL, API key and auth scheme `bearer` from the Connection tab in Retranslator.
4. Apply, then fully restart Claude Desktop.

## Build from source

```bash
npm ci
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri dev
```

Release builds come from GitHub Actions on a `v*` tag.
