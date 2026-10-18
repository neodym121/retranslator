// Retranslator UI. All state comes from the backend (get_state + events), language included; the page
// only keeps what is on screen: the open tab, form drafts and lists of models fetched for a provider.

type Tier = "sonnet" | "opus" | "haiku";
type Lang = "ru" | "en";

interface ServerStatus {
  running: boolean;
  port: number;
  error: string | null;
}
interface Provider {
  id: string;
  name: string;
  preset: string;
  base_url: string;
  key_hint: string | null;
}
interface Rule {
  id: string;
  local_id: string;
  provider_id: string;
  upstream_model: string;
  enabled: boolean;
  display_name: string;
  tier: Tier | null;
  family_default: boolean;
  max_input_tokens: number | null;
  supports_1m: boolean;
  max_output_tokens: number | null;
  created_at: string;
  warning?: string | null;
}
interface Preset {
  key: string;
  name: string;
  base_url: string;
}
interface UiState {
  server: ServerStatus;
  base_url: string;
  local_key: string;
  port: number;
  autostart: boolean;
  language: Lang;
  providers: Provider[];
  rules: Rule[];
  presets: Preset[];
  requests_total: number;
  version: string;
  config_dir: string;
}
interface LogEntry {
  id: number;
  time: string;
  method: string;
  path: string;
  local_model: string;
  upstream_model: string;
  provider: string;
  stream: boolean;
  status: number;
  duration_ms: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  error: string | null;
}
interface ModelList {
  models: string[];
  status: number;
  latency_ms: number;
}
interface ConnectionCheck {
  ok: boolean;
  message: string;
  status: number | null;
  latency_ms: number | null;
  models: string[] | null;
}
interface IdCheck {
  error: string | null;
  warning: string | null;
}

interface TauriGlobal {
  core: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
  event: { listen<T>(event: string, cb: (e: { payload: T }) => void): Promise<() => void> };
  window: { getCurrentWindow(): { minimize(): Promise<void>; toggleMaximize(): Promise<void>; close(): Promise<void> } };
}
const tauri = (window as unknown as { __TAURI__: TauriGlobal }).__TAURI__;

function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return tauri.core.invoke<T>(cmd, args);
}

const MAX_LOGS = 1000;

// ── Language ────────────────────────────────────────────

let lang: Lang = "ru";

/** The text in the current interface language. */
function t(ru: string, en: string): string {
  return lang === "en" ? en : ru;
}

function locale(): string {
  return lang === "en" ? "en-GB" : "ru-RU";
}

// ── DOM helpers ─────────────────────────────────────────

type Child = Node | string | number | null | undefined | false;
type Props = Record<string, unknown>;

function h<K extends keyof HTMLElementTagNameMap>(tag: K, props: Props = {}, ...children: Child[]): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") el.className = String(value);
    else if (key.startsWith("on") && typeof value === "function") {
      el.addEventListener(key.slice(2).toLowerCase(), value as EventListener);
    } else if (key === "value" || key === "checked") {
      (el as unknown as Record<string, unknown>)[key] = value;
    } else if (value === true) el.setAttribute(key, "");
    else el.setAttribute(key, String(value));
  }
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : String(c));
  }
  return el;
}

function $(id: string): HTMLElement {
  return document.getElementById(id)!;
}

function pulse(kind: "" | "live" | "ok" | "warn" = ""): HTMLElement {
  return h("span", { class: `pulse ${kind}` });
}

const ICONS = {
  edit: '<path d="M10.6 2.9l2.5 2.5-7.4 7.4-3.2.7.7-3.2z"/>',
  delete: '<path d="M2.8 4.4h10.4M6.3 4.4V2.7h3.4v1.7M4.2 4.4l.6 8.9h6.4l.6-8.9"/>',
  close: '<path d="M4 4l8 8M12 4l-8 8"/>',
};

function iconButton(name: keyof typeof ICONS, label: string, onclick: () => void): HTMLElement {
  const btn = h("button", { type: "button", class: "icon-btn", title: label, "aria-label": label, onclick });
  btn.innerHTML = `<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`;
  return btn;
}

function sectionHead(title: string, lead: string, ...actions: Child[]): HTMLElement {
  return h(
    "div",
    { class: "section-head" },
    h("div", {}, h("h1", {}, title), lead ? h("p", { class: "lead" }, lead) : null),
    h("div", { class: "row-actions" }, ...actions),
  );
}

function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}

function toast(text: string, kind: "ok" | "warn" = "ok"): void {
  const el = h("div", { class: "toast" }, pulse(kind), text);
  $("toasts").append(el);
  setTimeout(() => el.remove(), kind === "warn" ? 6000 : 2200);
}

async function copy(text: string, what: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = h("textarea", {}, text);
    document.body.append(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
  toast(t(`${what}: скопировано`, `${what}: copied`));
}

function fmtInt(n: number): string {
  return n.toLocaleString(locale());
}

function fmtContext(n: number | null): string {
  if (!n) return "—";
  if (n >= 1_000_000) return `${+(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1000) return `${Math.round(n / 1000)}K`;
  return String(n);
}

function fmtDuration(ms: number): string {
  if (ms < 1000) return t(`${ms} мс`, `${ms} ms`);
  if (ms < 60_000) return t(`${(ms / 1000).toFixed(1)} с`, `${(ms / 1000).toFixed(1)} s`);
  const min = Math.floor(ms / 60_000);
  const sec = Math.round((ms % 60_000) / 1000);
  return t(`${min} мин ${sec} с`, `${min} min ${sec} s`);
}

function fmtTime(iso: string): string {
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toLocaleTimeString(locale(), { hour12: false });
}

/** Server state in the instrument voice, the same in both languages. */
function serverWord(s: ServerStatus): string {
  return s.running ? "Live" : s.error ? "Error" : "Stopped";
}

// ── State ───────────────────────────────────────────────

let state: UiState;
let logs: LogEntry[] = [];
let tab = "connect";
let keyVisible = false;
const modelCache = new Map<string, string[]>();
const checks = new Map<string, ConnectionCheck | "pending">();

async function refresh(): Promise<void> {
  apply(await call<UiState>("get_state"));
}

function apply(next: UiState): void {
  state = next;
  lang = next.language;
  render();
}

async function act(fn: () => Promise<UiState>, done?: () => string): Promise<boolean> {
  try {
    apply(await fn());
    if (done) toast(done());
    return true;
  } catch (e) {
    toast(errorText(e), "warn");
    return false;
  }
}

function render(): void {
  document.documentElement.lang = lang;
  renderNav();
  renderServerBox();
  const views: Record<string, () => void> = {
    connect: renderConnect,
    providers: renderProviders,
    models: renderModels,
    logs: renderLogs,
    server: renderServer,
  };
  views[tab]?.();
}

function setTab(next: string): void {
  tab = next;
  for (const v of document.querySelectorAll<HTMLElement>(".view")) v.classList.toggle("active", v.dataset.view === tab);
  render();
}

function presetOf(key: string): Preset {
  return state.presets.find((p) => p.key === key) ?? state.presets[state.presets.length - 1];
}

function presetLabel(p: Preset): string {
  return p.key === "custom" ? t("Свой", "Custom") : p.name;
}

async function toggleServer(): Promise<void> {
  await act(() => call<UiState>(state.server.running ? "stop_server" : "start_server"));
}

// ── Sidebar ─────────────────────────────────────────────

function renderNav(): void {
  const labels: Record<string, string> = {
    connect: t("Подключение", "Connection"),
    providers: t("Провайдеры", "Providers"),
    models: t("Модели", "Models"),
    logs: t("Логи", "Logs"),
    server: t("Сервер", "Server"),
  };
  $("win-min").title = t("Свернуть", "Minimize");
  $("win-max").title = t("Развернуть", "Maximize");
  $("win-close").title = t("Закрыть: сервер останется в трее", "Close: the server stays in the tray");
  for (const b of document.querySelectorAll<HTMLElement>(".tab")) {
    b.textContent = labels[b.dataset.tab!] ?? "";
    b.classList.toggle("active", b.dataset.tab === tab);
  }
}

function renderServerBox(): void {
  const s = state.server;
  $("server-label").replaceChildren(pulse(s.running ? "live" : s.error ? "warn" : ""), serverWord(s));
  const langBtn = $("lang-toggle");
  langBtn.textContent = lang.toUpperCase();
  langBtn.title = t("Язык интерфейса: русский. Нажмите для English", "Interface language: English. Click for Russian");
  const btn = $("server-toggle") as HTMLButtonElement;
  btn.textContent = s.running ? t("Остановить", "Stop") : t("Запустить", "Start");
  btn.className = s.running ? "btn btn-ghost" : "btn btn-light";
}

async function toggleLanguage(): Promise<void> {
  await act(() => call<UiState>("set_language", { language: lang === "ru" ? "en" : "ru" }));
}

// ── Connect ─────────────────────────────────────────────

function renderConnect(): void {
  const s = state.server;
  const key = keyVisible ? state.local_key : `${state.local_key.slice(0, 8)}${"•".repeat(12)}`;
  const activeRules = state.rules.filter((r) => r.enabled && state.providers.some((p) => p.id === r.provider_id));
  const copyLabel = t("Копировать", "Copy");

  const valueRow = (label: string, value: string, ...actions: Child[]) =>
    h(
      "div",
      { class: "field-row" },
      h("div", { class: "kv" }, h("span", { class: "mono-label" }, label), h("div", { class: "value", title: value }, value)),
      h("div", { class: "actions" }, ...actions),
    );

  const card = h(
    "div",
    { class: "card-light" },
    h("span", { class: "eyebrow" }, "Developer → Configure Third-Party Inference"),
    valueRow("Gateway base URL", state.base_url, h("button", { class: "btn btn-dark", onclick: () => copy(state.base_url, "Base URL") }, copyLabel)),
    valueRow(
      "Gateway API key",
      key,
      h(
        "button",
        {
          class: "btn btn-ghost",
          onclick: () => {
            keyVisible = !keyVisible;
            renderConnect();
          },
        },
        keyVisible ? t("Скрыть", "Hide") : t("Показать", "Show"),
      ),
      h("button", { class: "btn btn-ghost", onclick: regenerateKey }, t("Перевыпустить", "Regenerate")),
      h("button", { class: "btn btn-dark", onclick: () => copy(state.local_key, "API key") }, copyLabel),
    ),
    valueRow("Gateway auth scheme", "bearer", h("button", { class: "btn btn-dark", onclick: () => copy("bearer", "Auth scheme") }, copyLabel)),
  );

  const metric = (label: string, value: Child, signal = false) =>
    h(
      "div",
      { class: "metric" },
      h("span", { class: "mono-label" }, label),
      h("div", { class: `metric-value${signal ? " signal" : ""}` }, value),
    );

  const kbd = (text: string) => h("span", { class: "kbd" }, text);
  const steps: Child[][] = [
    [t("В Claude Desktop: ", "In Claude Desktop: "), kbd("Help → Troubleshooting → Enable Developer Mode"), "."],
    [t("Откройте ", "Open "), kbd("Developer → Configure Third-Party Inference"), "."],
    [
      "Inference provider — ",
      h("b", {}, "Gateway"),
      t(". Вставьте Gateway base URL, API key и auth scheme ", ". Paste the Gateway base URL, API key and auth scheme "),
      kbd("bearer"),
      t(" из карточки.", " from the card."),
    ],
    [t("Нажмите ", "Click "), h("b", {}, "Apply"), "."],
    [t("Полностью перезапустите Claude Desktop: выйдите через трей и откройте заново.", "Fully restart Claude Desktop: quit it from the tray and open it again.")],
  ];

  $("view-connect").replaceChildren(
    sectionHead(t("Подключение", "Connection"), ""),
    card,
    h(
      "div",
      { class: "metrics" },
      metric(t("Сервер", "Server"), serverWord(s), !s.running && !!s.error),
      metric(t("Порт", "Port"), h("span", { class: "mono" }, String(state.port))),
      metric(t("Модели", "Models"), String(activeRules.length)),
      metric(t("Запросы", "Requests"), fmtInt(state.requests_total)),
    ),
    h(
      "div",
      { class: "block-gap" },
      h("div", { class: "block-title" }, h("span", { class: "eyebrow" }, t("Инструкция", "Setup"))),
      h(
        "ol",
        { class: "steps" },
        ...steps.map((body, i) => h("li", {}, h("span", { class: "num" }, String(i + 1).padStart(2, "0")), h("span", {}, ...body))),
      ),
    ),
  );
}

async function regenerateKey(): Promise<void> {
  const ok = await confirmDialog(
    t("Перевыпустить ключ?", "Regenerate the key?"),
    t(
      "Старый ключ перестанет работать сразу. В Claude Desktop нужно будет вставить новый и перезапустить его.",
      "The old key stops working right away. Paste the new one into Claude Desktop and restart it.",
    ),
    t("Перевыпустить", "Regenerate"),
  );
  if (ok) await act(() => call<UiState>("regenerate_key"), () => t("Новый ключ создан", "New key created"));
}

// ── Providers ───────────────────────────────────────────

function renderProviders(): void {
  const add = h("button", { class: "btn btn-light", onclick: () => openProvider() }, t("Добавить провайдера", "Add provider"));
  const head = sectionHead(
    t("Провайдеры", "Providers"),
    t(
      "Куда уходят запросы. API-ключи лежат в системном хранилище Windows (Credential Manager).",
      "Where requests go. API keys live in the Windows system store (Credential Manager).",
    ),
    add,
  );
  if (state.providers.length === 0) {
    $("view-providers").replaceChildren(
      head,
      h(
        "div",
        { class: "empty" },
        h("p", {}, t("Провайдеров пока нет. Пресеты: OpenAI, Google Gemini, OpenRouter, PolzaAI или свой.", "No providers yet. Presets: OpenAI, Google Gemini, OpenRouter, PolzaAI or a custom one.")),
      ),
    );
    return;
  }
  $("view-providers").replaceChildren(head, h("div", { class: "grid-2" }, ...state.providers.map(providerCard)));
}

function providerCard(p: Provider): HTMLElement {
  const rules = state.rules.filter((r) => r.provider_id === p.id).length;
  const check = checks.get(p.id);
  const result =
    check === "pending"
      ? h("span", { class: "check-result muted" }, t("Проверка…", "Checking…"))
      : check
        ? h(
            "span",
            { class: `check-result ${check.ok ? "good" : "signal"}`, title: check.message },
            check.ok ? `OK${check.latency_ms !== null ? ` · ${check.latency_ms} ${t("мс", "ms")}` : ""}` : t("Ошибка", "Error"),
          )
        : null;
  return h(
    "div",
    { class: "card provider-card" },
    h("div", { class: "top" }, h("span", { class: "name" }, p.name), result),
    h("div", { class: "url" }, p.base_url),
    h(
      "div",
      { class: "meta" },
      p.key_hint ? `${t("Ключ", "Key")} ${p.key_hint}` : h("span", { class: "signal" }, t("Ключ не задан", "No key")),
      ` · ${t("правил", "rules")}: ${rules}`,
    ),
    check && check !== "pending" && !check.ok ? h("div", { class: "hint signal" }, check.message) : null,
    h(
      "div",
      { class: "foot" },
      h("button", { class: "btn btn-ghost", onclick: () => testProvider(p) }, t("Проверить", "Test")),
      h(
        "div",
        { class: "row-actions" },
        iconButton("edit", t("Изменить", "Edit"), () => openProvider(p)),
        iconButton("delete", t("Удалить", "Delete"), () => deleteProvider(p)),
      ),
    ),
  );
}

async function testProvider(p: Provider): Promise<void> {
  checks.set(p.id, "pending");
  renderProviders();
  try {
    const res = await call<ConnectionCheck>("test_provider", {
      draft: { id: p.id, preset: p.preset, base_url: p.base_url, api_key: "" },
    });
    if (res.models) modelCache.set(p.id, res.models);
    checks.set(p.id, res);
  } catch (e) {
    checks.set(p.id, { ok: false, message: errorText(e), status: null, latency_ms: null, models: null });
  }
  if (tab === "providers") renderProviders();
}

async function deleteProvider(p: Provider): Promise<void> {
  const rules = state.rules.filter((r) => r.provider_id === p.id).length;
  const ok = await confirmDialog(
    t(`Удалить «${p.name}»?`, `Delete "${p.name}"?`),
    rules
      ? t(`Вместе с ним удалятся правила маппинга (${rules}) и сохранённый ключ.`, `Its mapping rules (${rules}) and the stored key go with it.`)
      : t("Сохранённый ключ тоже удалится.", "The stored key is deleted too."),
    t("Удалить", "Delete"),
  );
  if (ok) await act(() => call<UiState>("delete_provider", { id: p.id }), () => t("Провайдер удалён", "Provider deleted"));
}

/** Same rule as the backend: the base carries the version (`…/v1`), the path is appended. */
function endpointPreview(base: string): string {
  let b = base.trim().replace(/\/+$/, "");
  for (const suffix of ["/chat/completions", "/models"]) if (b.endsWith(suffix)) b = b.slice(0, -suffix.length);
  b = b.replace(/\/+$/, "");
  return b ? `${b}/chat/completions` : "";
}

function endpointText(endpoint: string): string {
  return endpoint ? t(`Запросы пойдут на ${endpoint}`, `Requests go to ${endpoint}`) : t("Укажите адрес API провайдера", "Enter the provider's API address");
}

function segmented<T extends string>(options: { value: T; label: string }[], current: T, onPick: (v: T) => void): HTMLElement {
  return h(
    "div",
    { class: "segmented" },
    ...options.map((o) => h("button", { type: "button", class: o.value === current ? "on" : "", onclick: () => onPick(o.value) }, o.label)),
  );
}

// Native <select> and <datalist> popups ignore the app's styles, so both are drawn here: a select
// button, or a text field with suggestions filtered by what is typed.
interface MenuItem {
  value: string;
  label: string;
}
interface DropdownOptions {
  value: string;
  items: () => MenuItem[];
  onPick: (value: string) => void;
  /** Text field mode: free input, the list only suggests. */
  input?: { placeholder: string; mono?: boolean; onInput: (value: string) => void };
}

const MENU_LIMIT = 300;
const CHEVRON = '<svg viewBox="0 0 10 10" width="10" height="10" aria-hidden="true"><path d="M2 3.8l3 3 3-3"/></svg>';

function dropdown(o: DropdownOptions): HTMLElement {
  let value = o.value;
  let shown: MenuItem[] = [];
  let active = -1;
  const wrap = h("div", { class: "dropdown" });
  const menu = h("div", { class: `menu${o.input?.mono ? " mono" : ""}`, role: "listbox" });
  const labelOf = (v: string) => o.items().find((i) => i.value === v)?.label ?? v;

  const caption = h("span", { class: "select-label" }, labelOf(value));
  const chevron = h("span", { class: "chevron" });
  chevron.innerHTML = CHEVRON;
  const control: HTMLInputElement | HTMLButtonElement = o.input
    ? h("input", {
        type: "text",
        class: o.input.mono ? "mono-input" : "",
        value,
        placeholder: o.input.placeholder,
        autocomplete: "off",
        spellcheck: "false",
        oninput: () => {
          value = control.value;
          o.input!.onInput(value);
          open();
        },
        onfocus: () => open(),
        onblur: () => close(),
      })
    : h("button", { type: "button", class: "select", onclick: () => (menu.isConnected ? close() : open()), onblur: () => close() }, caption, chevron);

  const highlight = (i: number) => {
    active = i;
    menu.querySelectorAll(".menu-item").forEach((el, n) => el.classList.toggle("active", n === i));
    menu.children[i]?.scrollIntoView({ block: "nearest" });
  };
  const pick = (v: string) => {
    value = v;
    if (o.input) control.value = v;
    else caption.textContent = labelOf(v);
    o.onPick(v);
    close();
  };
  const onScroll = (e: Event) => {
    if (e.target !== menu) close();
  };
  function close() {
    if (!menu.isConnected) return;
    menu.remove();
    active = -1;
    document.removeEventListener("scroll", onScroll, true);
  }
  function open() {
    const q = o.input ? control.value.trim().toLowerCase() : "";
    shown = o.items().filter((i) => !q || i.value.toLowerCase().includes(q) || i.label.toLowerCase().includes(q)).slice(0, MENU_LIMIT);
    if (shown.length === 0 || (shown.length === 1 && shown[0].value === control.value)) return close();
    menu.replaceChildren(
      ...shown.map((it) =>
        h(
          "div",
          {
            class: `menu-item${it.value === value ? " on" : ""}`,
            role: "option",
            // pointerdown + preventDefault: the control keeps focus, so its blur doesn't close the menu first
            onpointerdown: (e: Event) => {
              e.preventDefault();
              pick(it.value);
            },
          },
          it.label,
        ),
      ),
    );
    // Fixed position: escapes the dialog's overflow and stays in its top layer.
    const r = wrap.getBoundingClientRect();
    const below = innerHeight - r.bottom - 12;
    const up = below < 150 && r.top > below;
    menu.style.left = `${r.left}px`;
    menu.style.width = `${r.width}px`;
    menu.style.maxHeight = `${Math.min(260, up ? r.top - 12 : below)}px`;
    menu.style.top = up ? "" : `${r.bottom + 4}px`;
    menu.style.bottom = up ? `${innerHeight - r.top + 4}px` : "";
    if (!menu.isConnected) {
      wrap.append(menu);
      document.addEventListener("scroll", onScroll, true);
    }
    highlight(shown.findIndex((i) => i.value === value));
  }

  (control as HTMLElement).addEventListener("keydown", (e: KeyboardEvent) => {
    if (!menu.isConnected) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        open();
      }
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const step = e.key === "ArrowDown" ? 1 : -1;
      highlight((active + step + shown.length) % shown.length);
    } else if (e.key === "Enter" && active >= 0) {
      e.preventDefault();
      pick(shown[active].value);
    } else if (e.key === "Escape") {
      e.preventDefault(); // close the list, not the dialog
      close();
    }
  });
  wrap.append(control);
  return wrap;
}

// A div, not a label: a label would forward clicks on its caption to the first button inside.
function field(label: string, ...children: Child[]): HTMLElement {
  return h("div", { class: "field" }, h("span", { class: "mono-label" }, label), ...children);
}

function openProvider(existing?: Provider): void {
  const d = {
    id: existing?.id ?? "",
    preset: existing?.preset ?? "openai",
    name: existing?.preset === "custom" ? existing.name : "",
    base_url: existing?.preset === "custom" ? existing.base_url : "",
    api_key: "",
    clear_key: false,
  };
  let status: Child = null;
  let fetched: string[] | null = null;

  const isCustom = () => d.preset === "custom";
  const draft = () => ({ id: d.id, preset: d.preset, base_url: isCustom() ? d.base_url : "", api_key: d.api_key });

  const body = h("div", { class: "modal-body" });
  const errorLine = h("div", { class: "modal-error" });

  const draw = () => {
    const preset = presetOf(d.preset);
    const savedKey = existing?.key_hint && !d.clear_key;
    const presetEndpoint = h("span", { class: "hint" }, endpointText(endpointPreview(preset.base_url)));
    body.replaceChildren(
      h(
        "div",
        { class: "form" },
        existing
          ? isCustom()
            ? null
            : presetEndpoint
          : field(
              t("Пресет", "Preset"),
              segmented(
                state.presets.map((p) => ({ value: p.key, label: presetLabel(p) })),
                d.preset,
                (key) => {
                  d.preset = key;
                  status = null;
                  draw();
                },
              ),
              isCustom() ? null : presetEndpoint,
            ),
        ...(isCustom()
          ? [
              field(
                t("Название", "Name"),
                h("input", {
                  type: "text",
                  value: d.name,
                  placeholder: t("Например, Ollama", "For example, Ollama"),
                  oninput: (e: Event) => (d.name = (e.target as HTMLInputElement).value),
                }),
              ),
              field(
                t("Base URL (OpenAI-совместимый)", "Base URL (OpenAI-compatible)"),
                h("input", {
                  type: "text",
                  class: "mono-input",
                  value: d.base_url,
                  placeholder: "https://api.example.com/v1",
                  oninput: (e: Event) => {
                    d.base_url = (e.target as HTMLInputElement).value;
                    const hint = body.querySelector<HTMLElement>("[data-endpoint]");
                    if (hint) hint.textContent = endpointText(endpointPreview(d.base_url));
                  },
                }),
                h("span", { class: "hint", "data-endpoint": "" }, endpointText(endpointPreview(d.base_url))),
              ),
            ]
          : []),
        field(
          "API key",
          h("input", {
            type: "password",
            class: "mono-input",
            value: d.api_key,
            autocomplete: "off",
            placeholder: savedKey
              ? t(`Сохранён ${existing!.key_hint} · пусто — не менять`, `Stored ${existing!.key_hint} · empty keeps it`)
              : t("Ключ провайдера", "Provider key"),
            oninput: (e: Event) => (d.api_key = (e.target as HTMLInputElement).value),
          }),
          existing?.key_hint
            ? h(
                "label",
                { class: "check hint" },
                h("input", {
                  type: "checkbox",
                  checked: d.clear_key,
                  onchange: (e: Event) => {
                    d.clear_key = (e.target as HTMLInputElement).checked;
                    draw();
                  },
                }),
                t("Удалить сохранённый ключ", "Delete the stored key"),
              )
            : null,
        ),
        status,
      ),
    );
  };

  // One check: a passed one also brings the model list for the rule suggestions.
  const test = async () => {
    status = h("span", { class: "hint" }, t("Проверяю соединение…", "Checking the connection…"));
    draw();
    try {
      const res = await call<ConnectionCheck>("test_provider", { draft: draft() });
      if (res.models) {
        fetched = res.models;
        if (d.id) modelCache.set(d.id, res.models);
      }
      status = h("span", { class: `hint ${res.ok ? "good" : "signal"}` }, res.message, res.latency_ms ? ` · ${res.latency_ms} ${t("мс", "ms")}` : "");
    } catch (e) {
      status = h("span", { class: "hint signal" }, errorText(e));
    }
    draw();
  };

  const save = async () => {
    errorLine.textContent = "";
    const before = new Set(state.providers.map((p) => p.id));
    try {
      const next = await call<UiState>("save_provider", { provider: { ...d } });
      if (fetched) {
        const id = d.id || next.providers.find((p) => !before.has(p.id))?.id;
        if (id) modelCache.set(id, fetched);
      }
      closeModal();
      apply(next);
      toast(existing ? t("Провайдер сохранён", "Provider saved") : t("Провайдер добавлен", "Provider added"));
    } catch (e) {
      errorLine.textContent = errorText(e);
    }
  };

  draw();
  openModal(
    existing ? `${t("Провайдер", "Provider")} · ${existing.name}` : t("Новый провайдер", "New provider"),
    body,
    errorLine,
    h(
      "div",
      { class: "modal-foot" },
      h("button", { class: "btn btn-ghost", onclick: test }, t("Проверить", "Test")),
      h("span", { class: "spacer" }),
      h("button", { class: "btn btn-text", onclick: closeModal }, t("Отмена", "Cancel")),
      h("button", { class: "btn btn-light", onclick: save }, t("Сохранить", "Save")),
    ),
  );
}

// ── Models ──────────────────────────────────────────────

function renderModels(): void {
  const noProviders = state.providers.length === 0;
  const add = h("button", { class: "btn btn-light", disabled: noProviders, onclick: () => openRule() }, t("Добавить правило", "Add rule"));
  const head = sectionHead(t("Модели", "Models"), "", add);
  if (state.rules.length === 0) {
    $("view-models").replaceChildren(
      head,
      h(
        "div",
        { class: "empty" },
        h(
          "p",
          {},
          noProviders
            ? t("Сначала добавьте провайдера, потом правило: например, claude-sonnet-5.5 → gpt-5-mini.", "Add a provider first, then a rule: for example claude-sonnet-5.5 → gpt-5-mini.")
            : t("Правил пока нет. Например: claude-sonnet-5.5 → gpt-5-mini.", "No rules yet. For example: claude-sonnet-5.5 → gpt-5-mini."),
        ),
        noProviders ? h("button", { class: "btn btn-ghost", onclick: () => setTab("providers") }, t("К провайдерам", "Go to providers")) : null,
      ),
    );
    return;
  }
  const rows = state.rules.map((r) => {
    const provider = state.providers.find((p) => p.id === r.provider_id);
    return h(
      "tr",
      { class: r.enabled ? "" : "disabled" },
      h(
        "td",
        { class: "cell-switch" },
        h("input", {
          type: "checkbox",
          class: "switch",
          checked: r.enabled,
          title: r.enabled ? t("Выключить", "Turn off") : t("Включить", "Turn on"),
          onchange: (e: Event) => {
            const on = (e.target as HTMLInputElement).checked;
            act(() => call<UiState>("set_rule_enabled", { id: r.id, enabled: on })).then((ok) => {
              if (!ok) renderModels();
            });
          },
        }),
      ),
      h(
        "td",
        {},
        h("div", { class: "mono-strong" }, r.local_id),
        r.display_name ? h("div", { class: "cell-sub" }, r.display_name) : null,
        r.warning ? h("div", { class: "cell-warn" }, r.warning) : null,
      ),
      h("td", { class: "cell-arrow" }, "→"),
      h("td", {}, h("div", { class: "mono-strong" }, r.upstream_model)),
      h("td", {}, provider?.name ?? h("span", { class: "signal" }, t("удалён", "deleted"))),
      h(
        "td",
        {},
        h(
          "div",
          { class: "chips" },
          r.tier ? h("span", { class: "tag", title: r.family_default ? t("По умолчанию для tier", "Default for the tier") : undefined }, r.family_default ? `${r.tier} ★` : r.tier) : null,
          r.supports_1m ? h("span", { class: "tag", title: t("supports_1m: Claude Desktop покажет вариант с суффиксом [1m]", "supports_1m: Claude Desktop shows a variant with the [1m] suffix") }, "1M") : null,
        ),
      ),
      h(
        "td",
        { class: "cell-actions" },
        h(
          "div",
          { class: "row-actions" },
          iconButton("edit", t("Изменить", "Edit"), () => openRule(r)),
          iconButton("delete", t("Удалить", "Delete"), () => deleteRule(r)),
        ),
      ),
    );
  });
  const col = (w: string) => h("col", w ? { width: w } : {});
  $("view-models").replaceChildren(
    head,
    h(
      "div",
      { class: "table-wrap" },
      h(
        "table",
        {},
        h("colgroup", {}, col("48"), col(""), col("20"), col(""), col("16%"), col("100"), col("68")),
        h(
          "thead",
          {},
          h(
            "tr",
            {},
            h("th", {}, t("Вкл", "On")),
            h("th", {}, t("Локальный id", "Local id")),
            h("th", { class: "cell-arrow" }, ""),
            h("th", {}, t("Реальный id", "Real id")),
            h("th", {}, t("Провайдер", "Provider")),
            h("th", {}, "Tier"),
            h("th", {}, ""),
          ),
        ),
        h("tbody", {}, ...rows),
      ),
    ),
  );
}

async function deleteRule(r: Rule): Promise<void> {
  const ok = await confirmDialog(
    t(`Удалить правило «${r.local_id}»?`, `Delete the rule "${r.local_id}"?`),
    t("Claude Desktop перестанет видеть эту модель после обновления списка.", "Claude Desktop stops seeing this model once it refreshes the list."),
    t("Удалить", "Delete"),
  );
  if (ok) await act(() => call<UiState>("delete_rule", { id: r.id }), () => t("Правило удалено", "Rule deleted"));
}

function guessTier(localId: string): Tier | null {
  const id = localId.toLowerCase();
  for (const tier of ["sonnet", "opus", "haiku"] as Tier[]) if (id.includes(tier)) return tier;
  return null;
}

function openRule(existing?: Rule): void {
  const d: Rule = existing
    ? { ...existing }
    : {
        id: "",
        local_id: "",
        provider_id: state.providers[0]?.id ?? "",
        upstream_model: "",
        enabled: true,
        display_name: "",
        tier: null,
        family_default: false,
        max_input_tokens: null,
        supports_1m: false,
        max_output_tokens: null,
        created_at: "",
      };
  let tierTouched = !!existing;
  let idCheck: IdCheck = { error: null, warning: existing?.warning ?? null };
  let modelsError: string | null = null;
  let advancedOpen = false;
  let checkTimer = 0;

  const body = h("div", { class: "modal-body" });
  const errorLine = h("div", { class: "modal-error" });

  // Quiet when the id is fine: only problems get a line.
  const idHint = (): HTMLElement | null => {
    const problem = idCheck.error ?? idCheck.warning;
    return problem ? h("span", { class: "hint signal" }, problem) : null;
  };

  const checkId = () => {
    clearTimeout(checkTimer);
    checkTimer = window.setTimeout(async () => {
      idCheck = await call<IdCheck>("check_local_id", { id: d.local_id });
      body.querySelector<HTMLElement>("[data-id-hint]")?.replaceChildren(idHint() ?? "");
    }, 120);
  };

  const num = (v: string): number | null => {
    const n = parseInt(v.replace(/\s/g, ""), 10);
    return Number.isFinite(n) && n > 0 ? n : null;
  };

  const modelsHint = (): HTMLElement | null =>
    modelsError
      ? h(
          "span",
          { class: "hint signal" },
          t("Список моделей не загрузился: ", "Could not load the model list: "),
          modelsError,
          " ",
          h("button", { type: "button", class: "btn btn-text", onclick: () => void loadModels() }, t("Повторить", "Retry")),
        )
      : null;

  // Suggestions for the real id load on their own, once per provider.
  const loadModels = async () => {
    const p = state.providers.find((x) => x.id === d.provider_id);
    if (!p) return;
    try {
      const res = await call<ModelList>("fetch_models", { draft: { id: p.id, preset: p.preset, base_url: p.base_url, api_key: "" } });
      modelCache.set(p.id, res.models);
      modelsError = null;
    } catch (e) {
      modelsError = errorText(e);
    }
    if (d.provider_id !== p.id) return;
    // A list that arrives while the field has focus shows up at once.
    const real = body.querySelector<HTMLInputElement>("[data-real-id] input");
    if (real && document.activeElement === real) real.dispatchEvent(new Event("input"));
    body.querySelector<HTMLElement>("[data-models-hint]")?.replaceChildren(modelsHint() ?? "");
  };

  const advancedSummary = () =>
    [
      d.max_input_tokens ? `${t("контекст", "context")} ${fmtContext(d.max_input_tokens)}` : "",
      d.max_output_tokens ? `${t("ответ", "output")} ≤ ${fmtInt(d.max_output_tokens)}` : "",
      d.supports_1m ? t("вариант 1M", "1M variant") : "",
    ]
      .filter(Boolean)
      .join(" · ");
  const updateSummary = () => {
    const el = body.querySelector<HTMLElement>("[data-advanced-summary]");
    if (el) el.textContent = advancedSummary();
  };

  const tierControl = (): HTMLElement =>
    h(
      "div",
      { class: "chips" },
      segmented<string>(
        [
          { value: "", label: t("Нет", "None") },
          { value: "sonnet", label: "sonnet" },
          { value: "opus", label: "opus" },
          { value: "haiku", label: "haiku" },
        ],
        d.tier ?? "",
        (v) => {
          tierTouched = true;
          d.tier = (v || null) as Tier | null;
          if (!d.tier) d.family_default = false;
          body.querySelector<HTMLElement>("[data-tier]")?.replaceChildren(tierControl());
        },
      ),
      d.tier
        ? h(
            "label",
            { class: "check" },
            h("input", {
              type: "checkbox",
              checked: d.family_default,
              onchange: (e: Event) => (d.family_default = (e.target as HTMLInputElement).checked),
            }),
            t(`По умолчанию для ${d.tier}`, `Default for ${d.tier}`),
          )
        : null,
    );

  const draw = () => {
    body.replaceChildren(
      h(
        "div",
        { class: "form" },
        field(
          t("Локальный id", "Local id"),
          h("input", {
            type: "text",
            class: "mono-input",
            value: d.local_id,
            placeholder: t("claude-sonnet-5.5 — так модель увидит Claude Desktop", "claude-sonnet-5.5 — the id Claude Desktop sees"),
            oninput: (e: Event) => {
              d.local_id = (e.target as HTMLInputElement).value;
              if (!tierTouched) {
                d.tier = guessTier(d.local_id);
                body.querySelector<HTMLElement>("[data-tier]")?.replaceChildren(tierControl());
              }
              checkId();
            },
          }),
          h("div", { class: "slot", "data-id-hint": "" }, idHint()),
          existing
            ? null
            : h(
                "div",
                { class: "chips" },
                ...["claude-sonnet-5.5", "claude-opus-5.5", "claude-haiku-5.5"].map((s) =>
                  h(
                    "button",
                    {
                      type: "button",
                      class: "chip",
                      onclick: () => {
                        d.local_id = s;
                        if (!tierTouched) d.tier = guessTier(s);
                        checkId();
                        draw();
                      },
                    },
                    s,
                  ),
                ),
              ),
        ),
        h(
          "div",
          { class: "form-row" },
          field(
            t("Провайдер", "Provider"),
            dropdown({
              value: d.provider_id,
              items: () => state.providers.map((p) => ({ value: p.id, label: p.name })),
              onPick: (id) => {
                if (id === d.provider_id) return;
                d.provider_id = id;
                modelsError = null;
                draw();
                if (!modelCache.has(d.provider_id)) void loadModels();
              },
            }),
          ),
          field(
            t("Реальный id", "Real id"),
            h(
              "div",
              { "data-real-id": "" },
              dropdown({
                value: d.upstream_model,
                items: () => (modelCache.get(d.provider_id) ?? []).map((m) => ({ value: m, label: m })),
                onPick: (m) => (d.upstream_model = m),
                input: { placeholder: "gpt-5-mini", mono: true, onInput: (m) => (d.upstream_model = m) },
              }),
            ),
          ),
        ),
        h("div", { class: "slot", "data-models-hint": "" }, modelsHint()),
        field(
          t("Отображаемое имя", "Display name"),
          h("input", {
            type: "text",
            value: d.display_name,
            placeholder: t("Пусто — показывать id", "Empty: show the id"),
            oninput: (e: Event) => (d.display_name = (e.target as HTMLInputElement).value),
          }),
        ),
        field("Tier", h("div", { "data-tier": "" }, tierControl())),
        h(
          "details",
          {
            class: "advanced",
            open: advancedOpen,
            ontoggle: (e: Event) => (advancedOpen = (e.target as HTMLDetailsElement).open),
          },
          h(
            "summary",
            {},
            h("span", { class: "mono-label" }, t("Дополнительно", "Advanced")),
            h("span", { class: "hint", "data-advanced-summary": "" }, advancedSummary()),
          ),
          h(
            "div",
            { class: "form" },
            h(
              "div",
              { class: "form-row" },
              field(
                t("Контекст, токенов", "Context, tokens"),
                h("input", {
                  type: "number",
                  min: "1",
                  value: d.max_input_tokens ?? "",
                  placeholder: "max_input_tokens",
                  oninput: (e: Event) => {
                    d.max_input_tokens = num((e.target as HTMLInputElement).value);
                    updateSummary();
                  },
                }),
                h("span", { class: "hint" }, t("По нему Claude решает, когда сжимать диалог.", "Claude uses it to decide when to compact the chat.")),
              ),
              field(
                t("Лимит ответа", "Output limit"),
                h("input", {
                  type: "number",
                  min: "1",
                  value: d.max_output_tokens ?? "",
                  placeholder: t("как просит клиент", "as the client asks"),
                  oninput: (e: Event) => {
                    d.max_output_tokens = num((e.target as HTMLInputElement).value);
                    updateSummary();
                  },
                }),
                h("span", { class: "hint" }, t("Claude просит до 64K, многие модели дают меньше.", "Claude asks for up to 64K, many models allow less.")),
              ),
            ),
            h(
              "label",
              { class: "check" },
              h("input", {
                type: "checkbox",
                checked: d.supports_1m,
                onchange: (e: Event) => {
                  d.supports_1m = (e.target as HTMLInputElement).checked;
                  updateSummary();
                },
              }),
              t("Вариант с контекстом 1M (supports_1m)", "1M-context variant (supports_1m)"),
            ),
          ),
        ),
      ),
    );
  };

  const save = async () => {
    errorLine.textContent = "";
    try {
      const { warning: _w, ...rule } = d;
      const next = await call<UiState>("save_rule", { rule });
      closeModal();
      apply(next);
      const saved = next.rules.find((r) => r.local_id.toLowerCase() === d.local_id.trim().toLowerCase());
      if (saved?.warning) toast(saved.warning, "warn");
      else toast(existing ? t("Правило сохранено", "Rule saved") : t("Правило добавлено", "Rule added"));
    } catch (e) {
      errorLine.textContent = errorText(e);
    }
  };

  draw();
  if (d.local_id) checkId();
  if (!modelCache.has(d.provider_id)) void loadModels();
  openModal(
    existing ? `${t("Правило", "Rule")} · ${existing.local_id}` : t("Новое правило", "New rule"),
    body,
    errorLine,
    h(
      "div",
      { class: "modal-foot" },
      h("span", { class: "spacer" }),
      h("button", { class: "btn btn-text", onclick: closeModal }, t("Отмена", "Cancel")),
      h("button", { class: "btn btn-light", onclick: save }, t("Сохранить", "Save")),
    ),
  );
}

// ── Logs ────────────────────────────────────────────────

function statusTag(status: number): HTMLElement {
  const kind = status >= 200 && status < 300 ? "good" : status === 499 ? "" : "signal";
  return h("span", { class: `tag ${kind}`, title: status === 499 ? t("Клиент закрыл соединение", "The client closed the connection") : undefined }, String(status));
}

function renderLogs(): void {
  const clear = h(
    "button",
    {
      class: "btn btn-ghost",
      disabled: logs.length === 0,
      onclick: async () => {
        await call("clear_logs");
        logs = [];
        renderLogs();
      },
    },
    t("Очистить", "Clear"),
  );
  const head = sectionHead(t("Логи", "Logs"), "", clear);
  if (logs.length === 0) {
    $("view-logs").replaceChildren(
      head,
      h(
        "div",
        { class: "empty" },
        h("p", {}, state.server.running ? t("Запросов пока не было.", "No requests yet.") : t("Сервер остановлен, запросов нет.", "The server is stopped, no requests.")),
      ),
    );
    return;
  }
  const rows = logs.map((l) => {
    const isMessages = l.path === "/v1/messages" && l.local_model;
    const tokens = l.input_tokens || l.output_tokens || l.cache_read_tokens;
    return h(
      "tr",
      {},
      h("td", { class: "num-cell muted" }, fmtTime(l.time)),
      h(
        "td",
        {},
        isMessages
          ? h("div", { class: "mono-strong" }, l.local_model)
          : h("div", { class: "mono-strong" }, `${l.method} ${l.path}`),
        isMessages && l.upstream_model ? h("div", { class: "cell-sub mono" }, `→ ${l.upstream_model}`) : null,
        l.error ? h("div", { class: "cell-warn", title: l.error }, l.error) : null,
      ),
      h("td", {}, l.provider || h("span", { class: "muted" }, "—")),
      h("td", {}, statusTag(l.status), h("div", { class: "cell-sub num-cell" }, fmtDuration(l.duration_ms))),
      h(
        "td",
        { class: "num-cell" },
        tokens ? `${fmtInt(l.input_tokens)} → ${fmtInt(l.output_tokens)}` : h("span", { class: "muted" }, "—"),
        l.cache_read_tokens || l.cache_write_tokens
          ? h("div", { class: "cell-sub" }, `${t("кэш", "cache")} ${fmtInt(l.cache_read_tokens)}${l.cache_write_tokens ? ` / +${fmtInt(l.cache_write_tokens)}` : ""}`)
          : null,
      ),
    );
  });
  const col = (w: string) => h("col", w ? { width: w } : {});
  $("view-logs").replaceChildren(
    head,
    h(
      "div",
      { class: "table-wrap" },
      h(
        "table",
        {},
        h("colgroup", {}, col("84"), col(""), col("18%"), col("92"), col("124")),
        h(
          "thead",
          {},
          h(
            "tr",
            {},
            h("th", {}, t("Время", "Time")),
            h("th", {}, t("Модель", "Model")),
            h("th", {}, t("Провайдер", "Provider")),
            h("th", {}, t("Статус", "Status")),
            h("th", { title: t("Токены: вход → выход", "Tokens: in → out") }, t("Токены", "Tokens")),
          ),
        ),
        h("tbody", {}, ...rows),
      ),
    ),
  );
}

// ── Server ──────────────────────────────────────────────

function renderServer(): void {
  if (document.activeElement?.classList.contains("port-input")) return; // don't clobber typing
  const s = state.server;
  let port = String(state.port);
  const applyPort = () => {
    const n = parseInt(port, 10);
    if (!Number.isInteger(n) || n < 1024 || n > 65535) {
      toast(t("Порт должен быть от 1024 до 65535", "The port must be between 1024 and 65535"), "warn");
      return;
    }
    (document.activeElement as HTMLElement | null)?.blur();
    act(() => call<UiState>("set_port", { port: n }), () => t(`Порт ${n} сохранён`, `Port ${n} saved`));
  };
  const portInput = h("input", {
    type: "number",
    class: "port-input",
    min: "1024",
    max: "65535",
    value: port,
    oninput: (e: Event) => (port = (e.target as HTMLInputElement).value),
    onkeydown: (e: KeyboardEvent) => {
      if (e.key === "Enter") applyPort();
    },
  });
  const setting = (label: string, desc: Child, control: Child = null) =>
    h("div", { class: "setting" }, h("span", { class: "mono-label" }, label), h("div", { class: "desc" }, desc), h("div", {}, control));

  $("view-server").replaceChildren(
    sectionHead(t("Сервер", "Server"), ""),
    h(
      "div",
      { class: "settings" },
      setting(
        t("Состояние", "State"),
        s.running
          ? h("span", {}, t("Работает на ", "Running on "), h("span", { class: "mono" }, `http://127.0.0.1:${s.port}`))
          : s.error
            ? h("span", { class: "signal" }, s.error)
            : "Stopped",
        h("button", { class: s.running ? "btn btn-ghost" : "btn btn-light", onclick: toggleServer }, s.running ? t("Остановить", "Stop") : t("Запустить", "Start")),
      ),
      setting(
        t("Порт", "Port"),
        t("От 1024 до 65535. После смены порта обновите Gateway base URL в Claude Desktop.", "From 1024 to 65535. After changing it, update the Gateway base URL in Claude Desktop."),
        h("div", { class: "input-with-button" }, portInput, h("button", { class: "btn btn-dark", onclick: applyPort }, t("Применить", "Apply"))),
      ),
      setting(
        t("Автозапуск", "Autostart"),
        t("Запускать сервер вместе с приложением.", "Start the server together with the app."),
        h("input", {
          type: "checkbox",
          class: "switch",
          checked: state.autostart,
          onchange: (e: Event) => act(() => call<UiState>("set_autostart", { enabled: (e.target as HTMLInputElement).checked })),
        }),
      ),
      setting(
        t("Трей", "Tray"),
        t(
          "Закрытие окна полностью выгружает интерфейс из памяти. Сервер продолжает работать в трее; клик по иконке откроет окно снова. Выход — через меню трея.",
          "Closing the window unloads the interface from memory. The server keeps running in the tray; clicking the icon opens the window again. Quit from the tray menu.",
        ),
      ),
      setting(
        t("Данные", "Data"),
        h(
          "span",
          {},
          t("Настройки: ", "Settings: "),
          h("span", { class: "mono" }, state.config_dir),
          t(". Ключи хранятся в Windows Credential Manager под именем Retranslator.", ". Keys are stored in Windows Credential Manager under the name Retranslator."),
        ),
      ),
      setting(t("Версия", "Version"), h("span", { class: "mono" }, state.version)),
    ),
  );
}

// ── Modal ───────────────────────────────────────────────

function openModal(title: string, ...parts: Node[]): void {
  const dlg = $("modal") as HTMLDialogElement;
  dlg.replaceChildren(
    h("div", { class: "modal-head" }, h("h2", {}, title), iconButton("close", t("Закрыть", "Close"), closeModal)),
    ...parts,
  );
  if (!dlg.open) dlg.showModal();
  dlg.querySelector<HTMLInputElement>("input[type=text]")?.focus();
}

function closeModal(): void {
  const dlg = $("modal") as HTMLDialogElement;
  if (dlg.open) dlg.close();
  dlg.replaceChildren();
}

function confirmDialog(title: string, text: string, okLabel: string): Promise<boolean> {
  return new Promise((resolve) => {
    const dlg = $("modal") as HTMLDialogElement;
    let answered = false;
    const finish = (v: boolean) => {
      if (answered) return;
      answered = true;
      closeModal();
      resolve(v);
    };
    openModal(
      title,
      h("div", { class: "modal-body" }, h("p", { class: "lead" }, text)),
      h(
        "div",
        { class: "modal-foot" },
        h("span", { class: "spacer" }),
        h("button", { class: "btn btn-text", onclick: () => finish(false) }, t("Отмена", "Cancel")),
        h("button", { class: "btn btn-light", onclick: () => finish(true) }, okLabel),
      ),
    );
    dlg.addEventListener("close", () => finish(false), { once: true });
  });
}

// ── Boot ────────────────────────────────────────────────

let refreshTimer = 0;
function refreshSoon(): void {
  clearTimeout(refreshTimer);
  refreshTimer = window.setTimeout(() => void refresh(), 150);
}

async function boot(): Promise<void> {
  for (const b of document.querySelectorAll<HTMLElement>(".tab")) b.addEventListener("click", () => setTab(b.dataset.tab!));
  $("server-toggle").addEventListener("click", () => void toggleServer());
  $("lang-toggle").addEventListener("click", () => void toggleLanguage());
  const win = tauri.window.getCurrentWindow();
  $("win-min").addEventListener("click", () => void win.minimize());
  $("win-max").addEventListener("click", () => void win.toggleMaximize());
  $("win-close").addEventListener("click", () => void win.close());

  await tauri.event.listen("state-changed", refreshSoon);
  await tauri.event.listen<LogEntry>("log-entry", (e) => {
    logs.unshift(e.payload);
    if (logs.length > MAX_LOGS) logs.length = MAX_LOGS;
    if (tab === "logs") renderLogs();
    if (tab === "connect") refreshSoon();
  });

  state = await call<UiState>("get_state");
  lang = state.language;
  logs = await call<LogEntry[]>("get_logs");
  setTab("connect");
}

boot().catch((e) => toast(`${t("Не удалось загрузить состояние", "Could not load the state")}: ${errorText(e)}`, "warn"));
