<p align="center">
  <img src="src-tauri/icons/source/icon.svg" width="96" alt="Логотип Retranslator">
</p>

<h1 align="center">Retranslator</h1>

<p align="center">
  <a href="https://github.com/neodym121/retranslator/releases/latest"><img src="https://img.shields.io/github/v/release/neodym121/retranslator?color=eeeeee&labelColor=101010" alt="Релиз"></a>
  <img src="https://img.shields.io/badge/platform-Windows-eeeeee?labelColor=101010" alt="Windows">
  <img src="https://img.shields.io/badge/Tauri-2-eeeeee?logo=tauri&labelColor=101010" alt="Tauri 2">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/neodym121/retranslator?color=eeeeee&labelColor=101010" alt="MIT"></a>
</p>

<p align="center"><a href="README.md">English</a> · <b>Русский</b></p>

Локальный Anthropic-совместимый гейтвей для режима Third-party inference в Claude Desktop. Claude Desktop видит
модели с Claude-именами (`claude-sonnet-5.5`), а запрос уходит выбранному провайдеру под реальным id
(`deepseek-v4.1-flash`, `gpt-…`, `gemini-…` и т. д.).

## Возможности

- Любой OpenAI-совместимый провайдер: пресеты OpenAI, Google Gemini, OpenRouter, PolzaAI или свой base URL.
- Полная конвертация Anthropic ⇄ OpenAI: текст, изображения, PDF, инструменты, thinking, стриминг, ошибки, подсчёт токенов.
- Правила моделей: локальный id → провайдер и реальный id, плюс отображаемое имя, tier, размер контекста и вариант 1M.
- Слушает только `127.0.0.1`. API-ключи хранятся в Windows Credential Manager.
- Лог запросов, иконка в трее, интерфейс на русском и английском.

## Скачать

[Последний релиз](https://github.com/neodym121/retranslator/releases/latest): установщик `.msi` или портативный `.exe`
(один файл, без установки). Windows 10/11 с WebView2.

## Подключение Claude Desktop

1. Help → Troubleshooting → Enable Developer Mode.
2. Developer → Configure Third-Party Inference → Inference provider: **Gateway**.
3. Скопируйте Gateway base URL, API key и auth scheme `bearer` со вкладки «Подключение» в Retranslator.
4. Apply, затем полностью перезапустите Claude Desktop.

## Сборка из исходников

```bash
npm ci
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri dev
```

Релизные сборки делает GitHub Actions по тегу `v*`.
