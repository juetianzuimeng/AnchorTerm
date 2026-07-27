/**
 * AnchorTerm frontend — PR4: menubar + dialogs, no sidebar, multi-tab SessionView.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openPath, openUrl } from "@tauri-apps/plugin-opener";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type AuthType = "password" | "public_key";
type SessionStateName =
  | "idle"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "disconnected"
  | "failed";
type InputMode = "shell" | "raw";
type PropsMode =
  | "create"
  | "edit-profile"
  | "reconnect"
  | "edit-runtime";

interface HostProfile {
  id: string;
  name: string;
  host: string;
  port: number;
  username: string;
  auth_type: AuthType;
  private_key_path?: string | null;
  has_saved_password: boolean;
  reconnect_enabled: boolean;
}

interface SessionSnapshot {
  session_id: string;
  state: SessionStateName;
  host?: string | null;
  username?: string | null;
  message?: string | null;
  cwd?: string | null;
  attempt?: number | null;
}

interface DataEvent {
  session_id?: string;
  sessionId?: string;
  data_b64?: string;
  dataB64?: string;
}
interface CwdEvent {
  session_id?: string;
  sessionId?: string;
  cwd: string;
}
interface ErrorEvent {
  session_id?: string;
  sessionId?: string;
  message: string;
}

interface PendingInputBuffer {
  text: string;
  cursor: number;
}

interface CompleteResult {
  line: string;
  cursor: number;
  candidates: string[];
  token_start: number;
  token_end: number;
}

interface CompleteUiState {
  candidates: string[];
  index: number;
  tokenStart: number;
  tokenEnd: number;
  /**
   * Line used as the cycle base (after common-prefix / last cycle apply).
   * Replacement is always `baseLine[0..tokenStart) + candidate + baseLine[tokenEnd..)`.
   */
  baseLine: string;
  /**
   * Exact draft text after the last completion apply/cycle.
   * If the user types further, this diverges and we must re-query instead of
   * cycling the stale list (see tgservice-info → tgservice-all… bug).
   */
  appliedLine: string;
  busy: boolean;
}

/** Last form values used to connect a tab (no password stored). */
interface ConnectFormSnapshot {
  host: string;
  port: number;
  username: string;
  authType: AuthType;
  privateKeyPath?: string | null;
  profileId?: string | null;
  profileName?: string;
}

const MAX_TABS = 16;

// ---------------------------------------------------------------------------
// Utils
// ---------------------------------------------------------------------------

const $ = <T extends HTMLElement>(id: string) =>
  document.getElementById(id) as T;

function opsLog(
  category: string,
  message: string,
  detail?: Record<string, unknown> | string | null,
) {
  let d: string | null = null;
  if (detail != null) {
    d = typeof detail === "string" ? detail : JSON.stringify(detail);
  }
  invoke("ops_log", { category, message, detail: d }).catch(() => {});
}

function previewText(s: string, max = 160): string {
  const one = s
    .replace(/\r/g, "\\r")
    .replace(/\n/g, "\\n")
    .replace(/\t/g, "\\t");
  return one.length > max ? one.slice(0, max) + "…" : one;
}

function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

// ---------------------------------------------------------------------------
// Draft command history (per host+user, survives tab close / app restart)
// ---------------------------------------------------------------------------

const CMD_HISTORY_STORE_KEY = "anchorterm.cmdHistory.v1";
const CMD_HISTORY_MAX = 500;
/** Max distinct host+user buckets kept in localStorage. */
const CMD_HISTORY_BUCKETS_MAX = 40;

function cmdHistoryKey(username: string, host: string): string {
  return `${username.trim()}@${host.trim().toLowerCase()}`;
}

function loadAllCmdHistories(): Record<string, string[]> {
  try {
    const raw = localStorage.getItem(CMD_HISTORY_STORE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return {};
    }
    const out: Record<string, string[]> = {};
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof k === "string" && Array.isArray(v)) {
        out[k] = v.filter((x): x is string => typeof x === "string");
      }
    }
    return out;
  } catch {
    return {};
  }
}

function saveAllCmdHistories(map: Record<string, string[]>) {
  try {
    localStorage.setItem(CMD_HISTORY_STORE_KEY, JSON.stringify(map));
  } catch (e) {
    opsLog("ERR", "cmd_history_persist_failed", { error: String(e) });
  }
}

function loadCmdHistory(username: string, host: string): string[] {
  if (!username.trim() || !host.trim()) return [];
  const list = loadAllCmdHistories()[cmdHistoryKey(username, host)];
  return Array.isArray(list) ? list.slice() : [];
}

function persistCmdHistory(username: string, host: string, list: string[]) {
  if (!username.trim() || !host.trim()) return;
  const key = cmdHistoryKey(username, host);
  const all = loadAllCmdHistories();
  // Drop then re-insert so this key becomes most-recently-used (insertion order).
  delete all[key];
  all[key] = list.slice(-CMD_HISTORY_MAX);
  const keys = Object.keys(all);
  if (keys.length > CMD_HISTORY_BUCKETS_MAX) {
    const drop = keys.length - CMD_HISTORY_BUCKETS_MAX;
    for (let i = 0; i < drop; i++) {
      delete all[keys[i]];
    }
  }
  saveAllCmdHistories(all);
}

function base64ToBytes(b64: string): Uint8Array {
  const binary = atob(b64);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

function escapeHtml(s: string) {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function newSessionId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (c) => {
    const r = (Math.random() * 16) | 0;
    const v = c === "x" ? r : (r & 0x3) | 0x8;
    return v.toString(16);
  });
}

function payloadSessionId(p: {
  session_id?: string;
  sessionId?: string;
}): string | undefined {
  return p.sessionId ?? p.session_id;
}

function isControlKeyPayload(data: string): boolean {
  if (!data) return false;
  if (data.length === 1) {
    const c = data.charCodeAt(0);
    if (c < 32 && c !== 9) return true;
    if (c === 127) return true;
  }
  if (data.startsWith("\x1b")) return true;
  return false;
}

function showToast(msg: string, ms = 2800) {
  let el = document.getElementById("app-toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "app-toast";
    el.className = "toast hidden";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.remove("hidden");
  window.setTimeout(() => el?.classList.add("hidden"), ms);
}

/**
 * Modal secret input. Never logs the value.
 * Resolves to string (may be empty if user OK with blank) or null if cancelled.
 */
function promptSecret(opts: {
  title: string;
  label: string;
  hint: string;
  allowEmpty?: boolean;
}): Promise<string | null> {
  return new Promise((resolve) => {
    const dlg = $("dlg-secret") as HTMLDialogElement;
    const input = $("dlg-secret-input") as HTMLInputElement;
    const title = $("dlg-secret-title");
    const label = $("dlg-secret-label");
    const hint = $("dlg-secret-hint");
    title.textContent = opts.title;
    label.textContent = opts.label;
    hint.textContent = opts.hint;
    input.value = "";

    const cleanup = () => {
      okBtn.removeEventListener("click", onOk);
      cancelBtn.removeEventListener("click", onCancel);
      xBtn.removeEventListener("click", onCancel);
      input.removeEventListener("keydown", onKey);
      if (dlg.open) dlg.close();
      input.value = "";
    };
    const onOk = () => {
      const v = input.value;
      if (!opts.allowEmpty && !v) {
        showToast("请输入内容");
        return;
      }
      cleanup();
      resolve(v);
    };
    const onCancel = () => {
      cleanup();
      resolve(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        onOk();
      } else if (e.key === "Escape") {
        e.preventDefault();
        onCancel();
      }
    };

    const okBtn = $("dlg-secret-ok");
    const cancelBtn = $("dlg-secret-cancel");
    const xBtn = $("dlg-secret-x");
    okBtn.addEventListener("click", onOk);
    cancelBtn.addEventListener("click", onCancel);
    xBtn.addEventListener("click", onCancel);
    input.addEventListener("keydown", onKey);

    if (!dlg.open) dlg.showModal();
    input.focus();
  });
}

/** Ensure public-key auth has a passphrase (prompt if form empty). Cancel → null. */
async function ensurePassphrase(
  current: string | null | undefined,
): Promise<string | null | undefined> {
  if (current && current.length > 0) return current;
  opsLog("UI", "prompt_passphrase_before_connect");
  const entered = await promptSecret({
    title: "私钥口令",
    label: "私钥口令 / passphrase",
    hint: "该私钥可能已加密。口令仅用于本次连接，不会写入配置或日志。无口令可留空后确定。",
    allowEmpty: true,
  });
  if (entered === null) return undefined; // cancelled
  return entered;
}

/** Ensure password auth has a password (prompt if form empty). Cancel → null. */
async function ensurePassword(
  current: string | null | undefined,
): Promise<string | null | undefined> {
  if (current && current.length > 0) return current;
  opsLog("UI", "prompt_password_before_connect");
  const entered = await promptSecret({
    title: "登录密码",
    label: "密码",
    hint: "未填写密码且可能无已保存凭据。密码仅用于本次连接，不会写入日志。若已保存到凭据库可留空后确定。",
    allowEmpty: true,
  });
  if (entered === null) return undefined;
  return entered;
}

// ---------------------------------------------------------------------------
// Form / dialogs (connection properties)
// ---------------------------------------------------------------------------

let selectedProfileId: string | null = null;
let propsMode: PropsMode = "create";
/** When reconnect / edit-runtime: target session id. */
let propsTargetSessionId: string | null = null;
let propsConnecting = false;
let mgrSelectedId: string | null = null;

function setPropsError(msg: string | null) {
  const el = $("dlg-props-error");
  if (!msg) {
    el.hidden = true;
    el.textContent = "";
    return;
  }
  el.hidden = false;
  el.textContent = msg;
}

function syncAuthFields() {
  const type = ($("auth-type") as HTMLSelectElement).value as AuthType;
  $("auth-password-fields").classList.toggle("hidden", type !== "password");
  $("auth-key-fields").classList.toggle("hidden", type !== "public_key");
}

function readForm() {
  const authType = ($("auth-type") as HTMLSelectElement).value as AuthType;
  const host = ($("host") as HTMLInputElement).value.trim();
  const port = Number(($("port") as HTMLInputElement).value) || 22;
  const username = ($("username") as HTMLInputElement).value.trim();
  const profileName = ($("profile-name") as HTMLInputElement).value.trim();
  return { authType, host, port, username, profileName };
}

function clearFormSecrets() {
  ($("password") as HTMLInputElement).value = "";
  ($("passphrase") as HTMLInputElement).value = "";
}

function fillFormFromProfile(p: HostProfile) {
  selectedProfileId = p.id;
  ($("profile-name") as HTMLInputElement).value = p.name;
  ($("host") as HTMLInputElement).value = p.host;
  ($("port") as HTMLInputElement).value = String(p.port);
  ($("username") as HTMLInputElement).value = p.username;
  ($("auth-type") as HTMLSelectElement).value = p.auth_type;
  ($("private-key-path") as HTMLInputElement).value = p.private_key_path || "";
  ($("save-password") as HTMLInputElement).checked = p.has_saved_password;
  clearFormSecrets();
  syncAuthFields();
}

function fillFormFromSnapshot(s: ConnectFormSnapshot, title?: string) {
  selectedProfileId = s.profileId ?? null;
  ($("profile-name") as HTMLInputElement).value =
    title || s.profileName || "";
  ($("host") as HTMLInputElement).value = s.host;
  ($("port") as HTMLInputElement).value = String(s.port);
  ($("username") as HTMLInputElement).value = s.username;
  ($("auth-type") as HTMLSelectElement).value = s.authType;
  ($("private-key-path") as HTMLInputElement).value =
    s.privateKeyPath || "";
  ($("save-password") as HTMLInputElement).checked = false;
  clearFormSecrets();
  syncAuthFields();
}

function clearForm() {
  selectedProfileId = null;
  ($("profile-name") as HTMLInputElement).value = "";
  ($("host") as HTMLInputElement).value = "";
  ($("port") as HTMLInputElement).value = "22";
  ($("username") as HTMLInputElement).value = "";
  ($("auth-type") as HTMLSelectElement).value = "password";
  ($("private-key-path") as HTMLInputElement).value = "";
  ($("save-password") as HTMLInputElement).checked = false;
  clearFormSecrets();
  syncAuthFields();
}

function setFormReadonly(ro: boolean) {
  for (const id of [
    "profile-name",
    "host",
    "port",
    "username",
    "auth-type",
    "password",
    "save-password",
    "private-key-path",
    "passphrase",
  ]) {
    const el = document.getElementById(id) as
      | HTMLInputElement
      | HTMLSelectElement
      | null;
    if (el) el.disabled = ro;
  }
}

/**
 * Build auth payload. For secrets missing from the form, prompts the user
 * **before** connect (never persists passphrase to profiles.json).
 * Returns null if validation fails or user cancels a required prompt.
 */
async function buildAuth(
  authType: AuthType,
): Promise<Record<string, unknown> | null> {
  if (authType === "password") {
    const save_password = ($("save-password") as HTMLInputElement).checked;
    let password = ($("password") as HTMLInputElement).value || null;
    const prompted = await ensurePassword(password);
    if (prompted === undefined) {
      setPropsError("已取消连接");
      return null;
    }
    password = prompted || null;
    return {
      type: "password",
      password,
      save_password,
    };
  }
  const private_key_path = (
    $("private-key-path") as HTMLInputElement
  ).value.trim();
  if (!private_key_path) {
    setPropsError("请填写私钥路径");
    return null;
  }
  let passphrase = ($("passphrase") as HTMLInputElement).value || null;
  const prompted = await ensurePassphrase(passphrase);
  if (prompted === undefined) {
    setPropsError("已取消连接");
    return null;
  }
  passphrase = prompted || null;
  // Reflect into form so a retry without re-open keeps the value for this dialog session.
  ($("passphrase") as HTMLInputElement).value = passphrase || "";
  return {
    type: "public_key",
    private_key_path,
    passphrase,
  };
}

async function loadProfiles(): Promise<HostProfile[]> {
  const profiles = await invoke<HostProfile[]>("list_profiles");
  (window as unknown as { __profiles: HostProfile[] }).__profiles = profiles;
  return profiles;
}

function getProfiles(): HostProfile[] {
  return (
    (window as unknown as { __profiles?: HostProfile[] }).__profiles || []
  );
}

async function saveProfileFromForm(
  id: string | null,
): Promise<HostProfile | null> {
  const { authType, host, port, username, profileName } = readForm();
  if (!host || !username) {
    setPropsError("保存需要主机与用户名");
    return null;
  }
  const name = profileName || `${username}@${host}`;
  try {
    const profile = await invoke<HostProfile>("save_profile", {
      req: {
        id,
        name,
        host,
        port,
        username,
        auth_type: authType,
        private_key_path:
          authType === "public_key"
            ? ($("private-key-path") as HTMLInputElement).value.trim() || null
            : null,
        save_password:
          authType === "password" &&
          ($("save-password") as HTMLInputElement).checked,
        password:
          authType === "password"
            ? ($("password") as HTMLInputElement).value || null
            : null,
      },
    });
    selectedProfileId = profile.id;
    await loadProfiles();
    return profile;
  } catch (e) {
    setPropsError(String(e));
    return null;
  }
}

// ---------------------------------------------------------------------------
// SessionView
// ---------------------------------------------------------------------------

class SessionView {
  readonly sessionId: string;
  title: string;
  profileId: string | null;
  host: string;
  username: string;
  state: SessionStateName = "idle";
  cwd: string | null = null;
  message: string | null = null;
  lastForm: ConnectFormSnapshot | null = null;

  term: Terminal;
  fitAddon: FitAddon;
  draft: PendingInputBuffer = { text: "", cursor: 0 };
  inputMode: InputMode = "shell";
  pendingDraft: string | null = null;
  completeUi: CompleteUiState | null = null;

  /**
   * Shell draft history for this tab's current host+user.
   * Backed by localStorage so close-tab / re-open same endpoint keeps history.
   * Oldest → newest. Browsing with ↑/↓ when complete popup is closed.
   */
  private cmdHistory: string[] = [];
  /** `null` = editing live draft; otherwise index into `cmdHistory`. */
  private histIndex: number | null = null;
  /** Snapshot of the in-progress draft when the user first presses ↑. */
  private histLiveDraft = "";

  rootEl: HTMLElement;
  termHost: HTMLElement;
  draftInput: HTMLInputElement;
  btnSend: HTMLButtonElement;
  btnMode: HTMLButtonElement;
  completePopup: HTMLUListElement;
  overlay: HTMLElement;
  overlayText: HTMLElement;
  errorEl: HTMLElement;

  private disposed = false;

  constructor(opts: {
    sessionId: string;
    title: string;
    host: string;
    username: string;
    profileId?: string | null;
  }) {
    this.sessionId = opts.sessionId;
    this.title = opts.title;
    this.host = opts.host;
    this.username = opts.username;
    this.profileId = opts.profileId ?? null;
    this.cmdHistory = loadCmdHistory(this.username, this.host);

    this.rootEl = document.createElement("div");
    this.rootEl.className = "session-view";
    this.rootEl.dataset.sessionId = this.sessionId;

    this.termHost = document.createElement("div");
    this.termHost.className = "term-host shell-mode";

    this.errorEl = document.createElement("p");
    this.errorEl.className = "error session-error";
    this.errorEl.hidden = true;

    const draftWrap = document.createElement("div");
    draftWrap.className = "draft-wrap";

    this.completePopup = document.createElement("ul");
    this.completePopup.className = "complete-popup hidden";
    this.completePopup.setAttribute("role", "listbox");

    const draftBar = document.createElement("div");
    draftBar.className = "draft-bar";

    const label = document.createElement("label");
    label.className = "draft-label";
    label.textContent = "草稿";

    this.draftInput = document.createElement("input");
    this.draftInput.type = "text";
    this.draftInput.className = "draft-input";
    this.draftInput.placeholder =
      "命令在此输入 · ↑↓ 历史 · Tab 补全 · Enter 发送（断线保留）";
    this.draftInput.autocomplete = "off";
    this.draftInput.spellcheck = false;

    this.btnSend = document.createElement("button");
    this.btnSend.type = "button";
    this.btnSend.className = "btn-draft-send";
    this.btnSend.textContent = "发送";
    this.btnSend.disabled = true;

    this.btnMode = document.createElement("button");
    this.btnMode.type = "button";
    this.btnMode.className = "btn-input-mode";
    this.btnMode.textContent = "Shell 模式";
    this.btnMode.title = "点击切换 Shell 模式 / TUI 直通";

    const modeControls = document.createElement("div");
    modeControls.className = "mode-controls";
    const modeHelp = document.createElement("button");
    modeHelp.type = "button";
    modeHelp.className = "mode-help";
    modeHelp.setAttribute("aria-label", "输入模式说明");
    modeHelp.textContent = "?";
    modeHelp.tabIndex = 0;
    const modeTip = document.createElement("div");
    modeTip.className = "mode-help-tip";
    modeTip.setAttribute("role", "tooltip");
    modeTip.innerHTML = [
      "<strong>输入模式说明</strong>",
      "<p><b>Shell 模式</b>（默认）</p>",
      "<ul>",
      "<li>在底部「草稿」框输入命令，Enter 发送</li>",
      "<li>支持 Tab 补全、↑↓ 历史、断线保留草稿</li>",
      "<li>终端区主要显示输出；普通按键进草稿框，不直接进 SSH</li>",
      "</ul>",
      "<p><b>TUI 直通</b></p>",
      "<ul>",
      "<li>按键直接发给远端终端（适合 vim / htop / less 等）</li>",
      "<li>不使用草稿的补全与本地历史（由远端程序自己处理）</li>",
      "<li>交互式全屏程序请用此模式</li>",
      "</ul>",
      "<p class=\"mode-help-tip-foot\">点击「Shell 模式 / TUI 直通」按钮可切换。</p>",
    ].join("");
    modeControls.append(this.btnMode, modeHelp, modeTip);
    // Keep tip open while hovering the help control itself.
    modeHelp.addEventListener("click", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      modeControls.classList.toggle("tip-pinned");
    });
    modeHelp.addEventListener("blur", () => {
      modeControls.classList.remove("tip-pinned");
    });

    draftBar.append(label, this.draftInput, this.btnSend, modeControls);
    draftWrap.append(this.completePopup, draftBar);

    this.overlay = document.createElement("div");
    this.overlay.className = "overlay hidden";
    const card = document.createElement("div");
    card.className = "overlay-card";
    const spin = document.createElement("div");
    spin.className = "spinner";
    this.overlayText = document.createElement("p");
    this.overlayText.textContent = "连接中…";
    card.append(spin, this.overlayText);
    this.overlay.append(card);

    this.rootEl.append(this.termHost, this.errorEl, draftWrap, this.overlay);

    this.term = new Terminal({
      cursorBlink: true,
      fontSize: 14,
      fontFamily: 'Consolas, "Cascadia Mono", "Courier New", monospace',
      convertEol: true,
      theme: {
        background: "#0a0e14",
        foreground: "#e7ecf3",
        cursor: "#3d8bfd",
        selectionBackground: "rgba(61,139,253,0.35)",
      },
      allowProposedApi: true,
    });
    this.fitAddon = new FitAddon();
    this.term.loadAddon(this.fitAddon);
    this.term.open(this.termHost);
    this.term.options.disableStdin = true;

    this.wireTerminal();
    this.wireDraft();
  }

  isLive(): boolean {
    return this.state === "connected";
  }

  isBusy(): boolean {
    return this.state === "connecting" || this.state === "reconnecting";
  }

  canReconnectSameTab(): boolean {
    return (
      this.state === "idle" ||
      this.state === "failed" ||
      this.state === "disconnected"
    );
  }

  mount(parent: HTMLElement) {
    parent.appendChild(this.rootEl);
  }

  setActive(active: boolean) {
    this.rootEl.classList.toggle("active", active);
  }

  fitAndResize() {
    if (this.disposed) return;
    this.fitAddon.fit();
    if (this.isLive()) {
      invoke("resize", {
        sessionId: this.sessionId,
        cols: this.term.cols,
        rows: this.term.rows,
      }).catch(() => {});
    }
  }

  /** Wait until layout is visible then fit; return terminal cols/rows for connect. */
  async fitThenDims(): Promise<{ cols: number; rows: number }> {
    await new Promise<void>((resolve) => {
      requestAnimationFrame(() => {
        requestAnimationFrame(() => {
          if (!this.disposed) this.fitAddon.fit();
          resolve();
        });
      });
    });
    return {
      cols: Math.max(this.term.cols || 80, 20),
      rows: Math.max(this.term.rows || 24, 5),
    };
  }

  applyInputMode() {
    if (this.inputMode === "shell") {
      this.btnMode.textContent = "Shell 模式";
      this.btnMode.classList.remove("raw");
      this.btnMode.title = "当前：Shell 模式 · 点击切换为 TUI 直通";
      this.termHost.classList.add("shell-mode");
      this.term.options.disableStdin = true;
    } else {
      this.btnMode.textContent = "TUI 直通";
      this.btnMode.classList.add("raw");
      this.btnMode.title = "当前：TUI 直通 · 点击切换为 Shell 模式";
      this.termHost.classList.remove("shell-mode");
      this.term.options.disableStdin = false;
      if (this.isLive()) this.term.focus();
    }
  }

  toggleInputMode() {
    this.inputMode = this.inputMode === "shell" ? "raw" : "shell";
    opsLog("UI", `input_mode_toggle mode=${this.inputMode}`, {
      sid: this.sessionId.slice(0, 8),
    });
    this.applyInputMode();
    if (this.inputMode === "shell") this.draftInput.focus();
  }

  clearScreenLocal() {
    this.term.clear();
    opsLog("UI", "clear_screen_local", { sid: this.sessionId.slice(0, 8) });
  }

  clearDraft() {
    this.draft = { text: "", cursor: 0 };
    this.draftInput.value = "";
    this.histIndex = null;
    this.histLiveDraft = "";
    this.hideCompletePopup();
  }

  /**
   * Reload history when host/user changes (same-tab reconnect or new endpoint).
   * Call after assigning `this.host` / `this.username`.
   */
  rebindCmdHistory() {
    this.cmdHistory = loadCmdHistory(this.username, this.host);
    this.histIndex = null;
    this.histLiveDraft = "";
    opsLog("CMD", "history_rebind", {
      key: cmdHistoryKey(this.username, this.host),
      n: this.cmdHistory.length,
      sid: this.sessionId.slice(0, 8),
    });
  }

  setError(msg: string | null) {
    if (!msg) {
      this.errorEl.hidden = true;
      this.errorEl.textContent = "";
      return;
    }
    this.errorEl.hidden = false;
    this.errorEl.textContent = msg;
  }

  applyState(
    state: SessionStateName,
    message?: string | null,
    cwd?: string | null,
  ) {
    this.state = state;
    if (message !== undefined) this.message = message ?? null;
    if (cwd !== undefined) this.cwd = cwd ?? null;

    if (state === "connecting" || state === "reconnecting") {
      this.overlay.classList.remove("hidden");
      this.overlay.classList.toggle("reconnecting", state === "reconnecting");
      this.overlayText.textContent =
        message ||
        (state === "reconnecting" ? "重连中…" : "连接中…");
    } else {
      this.overlay.classList.add("hidden");
      this.overlay.classList.remove("reconnecting");
    }

    this.btnSend.disabled = !this.isLive();

    if (this.isLive() && this.pendingDraft !== null) {
      const line = this.pendingDraft;
      this.pendingDraft = null;
      void this.flushPendingLine(line);
    }

    if (state === "connected") {
      this.applyInputMode();
      if (this.inputMode === "shell") this.draftInput.focus();
    }

    renderTabBar();
    if (activeSessionId === this.sessionId) {
      syncGlobalStatusBar();
    }
  }

  writeToTerm(data: string | Uint8Array) {
    if (typeof data === "string") {
      this.term.write(data);
    } else {
      this.term.write(new TextDecoder("utf-8", { fatal: false }).decode(data));
    }
  }

  private wireTerminal() {
    this.term.onData((data) => {
      if (this.inputMode === "shell" && !isControlKeyPayload(data)) {
        this.draftInput.focus();
        return;
      }
      if (!this.isLive()) return;
      const bytes = new TextEncoder().encode(data);
      invoke("write_bytes", {
        sessionId: this.sessionId,
        dataB64: bytesToBase64(bytes),
      }).catch((e) => {
        opsLog("ERR", "term_key write failed", { error: String(e) });
        this.setError(String(e));
      });
    });
  }

  private wireDraft() {
    this.btnMode.addEventListener("click", (ev) => {
      ev.preventDefault();
      this.toggleInputMode();
    });
    this.btnSend.addEventListener("click", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      void this.sendDraftLine();
    });
    this.draftInput.addEventListener("keydown", (e) => {
      if (e.key === "Tab") {
        e.preventDefault();
        void this.runComplete();
        return;
      }
      if (this.completeUi && !this.completeUi.busy) {
        if (e.key === "Escape") {
          e.preventDefault();
          this.hideCompletePopup();
          return;
        }
        if (e.key === "ArrowDown") {
          e.preventDefault();
          this.cycleCandidate(1);
          return;
        }
        if (e.key === "ArrowUp") {
          e.preventDefault();
          this.cycleCandidate(-1);
          return;
        }
      }
      // Command history (only when completion popup is not active).
      if (e.key === "ArrowUp" && !e.altKey && !e.ctrlKey && !e.metaKey) {
        e.preventDefault();
        this.historyStep(-1);
        return;
      }
      if (e.key === "ArrowDown" && !e.altKey && !e.ctrlKey && !e.metaKey) {
        e.preventDefault();
        this.historyStep(1);
        return;
      }
      if (e.key === "Enter") {
        e.preventDefault();
        void this.sendDraftLine();
      }
    });
    // Typing while browsing history leaves browse mode so further ↑ starts
    // from the latest entry again (bash-like).
    this.draftInput.addEventListener("input", () => {
      if (this.histIndex !== null) {
        this.histIndex = null;
        this.histLiveDraft = "";
      }
      this.syncDraftFromInput();
      // User continued typing after a multi-candidate complete (e.g. filled
      // "tgservice-" then typed "info"). Drop the stale popup so the next Tab
      // re-queries with the new prefix instead of cycling old candidates.
      if (
        this.completeUi &&
        !this.completeUi.busy &&
        this.draft.text !== this.completeUi.appliedLine
      ) {
        opsLog("CMD", "complete_invalidate_on_edit", {
          applied: previewText(this.completeUi.appliedLine, 120),
          now: previewText(this.draft.text, 120),
          sid: this.sessionId.slice(0, 8),
        });
        this.hideCompletePopup();
      }
    });
  }

  /** Push a successfully-submitted (or queued) command into host+user history. */
  private pushHistory(line: string) {
    const logical = line.replace(/[\r\n]+$/g, "").trimEnd();
    if (!logical.trim()) return;
    const last = this.cmdHistory[this.cmdHistory.length - 1];
    if (last === logical) {
      this.histIndex = null;
      this.histLiveDraft = "";
      return;
    }
    this.cmdHistory.push(logical);
    if (this.cmdHistory.length > CMD_HISTORY_MAX) {
      this.cmdHistory.splice(0, this.cmdHistory.length - CMD_HISTORY_MAX);
    }
    this.histIndex = null;
    this.histLiveDraft = "";
    persistCmdHistory(this.username, this.host, this.cmdHistory);
  }

  /**
   * Browse local draft history. `delta` -1 = older (↑), +1 = newer (↓).
   * Does not talk to the remote shell's HISTFILE.
   */
  private historyStep(delta: number) {
    this.hideCompletePopup();
    if (this.cmdHistory.length === 0) return;

    if (delta < 0) {
      // Older
      if (this.histIndex === null) {
        this.syncDraftFromInput();
        this.histLiveDraft = this.draft.text;
        this.histIndex = this.cmdHistory.length - 1;
      } else if (this.histIndex > 0) {
        this.histIndex -= 1;
      } else {
        return; // already at oldest
      }
    } else {
      // Newer
      if (this.histIndex === null) return;
      if (this.histIndex < this.cmdHistory.length - 1) {
        this.histIndex += 1;
      } else {
        // Past newest → restore in-progress draft
        this.histIndex = null;
        this.draft.text = this.histLiveDraft;
        this.draft.cursor = this.histLiveDraft.length;
        this.histLiveDraft = "";
        this.restoreDraftToInput();
        return;
      }
    }

    const line = this.cmdHistory[this.histIndex!];
    this.draft.text = line;
    this.draft.cursor = line.length;
    this.restoreDraftToInput();
  }

  private syncDraftFromInput() {
    this.draft.text = this.draftInput.value;
    this.draft.cursor =
      this.draftInput.selectionStart ?? this.draft.text.length;
  }

  private restoreDraftToInput() {
    this.draftInput.value = this.draft.text;
    const pos = Math.min(this.draft.cursor, this.draft.text.length);
    this.draftInput.setSelectionRange(pos, pos);
  }

  private hideCompletePopup() {
    this.completeUi = null;
    this.completePopup.classList.add("hidden");
    this.completePopup.innerHTML = "";
  }

  private cycleCandidate(delta: number) {
    if (!this.completeUi || this.completeUi.candidates.length === 0) return;
    const n = this.completeUi.candidates.length;
    this.completeUi.index = (this.completeUi.index + delta + n) % n;
    this.renderCompletePopup();
    const name = this.completeUi.candidates[this.completeUi.index];
    const base = this.completeUi.baseLine;
    const start = this.completeUi.tokenStart;
    const end = this.completeUi.tokenEnd;
    const before = this.draftInput.value;
    const next = base.slice(0, start) + name + base.slice(end);
    const suffixLeft = base.slice(end);
    this.draft.text = next;
    this.draft.cursor = start + name.length;
    this.completeUi.appliedLine = next;
    // Keep token span in sync with the newly applied candidate for next cycle.
    this.completeUi.tokenEnd = start + name.length;
    this.completeUi.baseLine = next;
    this.restoreDraftToInput();
    opsLog("CMD", "complete_cycle", {
      index: this.completeUi.index,
      n,
      candidate: previewText(name, 80),
      base: previewText(base, 120),
      tokenRange: [start, end],
      suffixAfterToken: previewText(suffixLeft, 40),
      before: previewText(before, 120),
      after: previewText(next, 120),
      sid: this.sessionId.slice(0, 8),
    });
  }

  private renderCompletePopup() {
    if (!this.completeUi) return;
    this.completePopup.innerHTML = "";
    this.completeUi.candidates.forEach((c, i) => {
      const li = document.createElement("li");
      li.textContent = c;
      if (i === this.completeUi!.index) li.classList.add("active");
      li.addEventListener("mousedown", (ev) => {
        ev.preventDefault();
        this.completeUi!.index = i;
        this.cycleCandidate(0);
        this.hideCompletePopup();
      });
      this.completePopup.appendChild(li);
    });
    this.completePopup.classList.remove("hidden");
  }

  private async runComplete() {
    if (!this.isLive()) {
      this.setError("未连接，无法补全");
      return;
    }
    this.syncDraftFromInput();
    const line = this.draft.text;
    const cursor = this.draftInput.selectionStart ?? line.length;
    // Only cycle the open list when the user has NOT edited the draft since
    // the last apply. Otherwise re-query with the refined prefix (bash-like).
    if (
      this.completeUi &&
      this.completeUi.candidates.length > 1 &&
      !this.completeUi.busy
    ) {
      if (line === this.completeUi.appliedLine) {
        opsLog("CMD", "complete_tab_cycle_existing", {
          n: this.completeUi.candidates.length,
          index: this.completeUi.index,
          sid: this.sessionId.slice(0, 8),
        });
        this.cycleCandidate(1);
        return;
      }
      opsLog("CMD", "complete_requery_after_edit", {
        applied: previewText(this.completeUi.appliedLine, 120),
        now: previewText(line, 120),
        sid: this.sessionId.slice(0, 8),
      });
      this.hideCompletePopup();
    }
    if (this.completeUi?.busy) return;
    opsLog("CMD", "complete_request", {
      line: previewText(line, 160),
      cursor,
      sid: this.sessionId.slice(0, 8),
    });
    this.completeUi = {
      candidates: [],
      index: 0,
      tokenStart: 0,
      tokenEnd: cursor,
      baseLine: line,
      appliedLine: line,
      busy: true,
    };
    try {
      const result = await invoke<CompleteResult>("complete_draft", {
        sessionId: this.sessionId,
        line,
        cursor,
      });
      this.setError(null);
      const r = result as CompleteResult & {
        tokenStart?: number;
        tokenEnd?: number;
        token_start?: number;
        token_end?: number;
      };
      const tokenStart = r.tokenStart ?? r.token_start ?? 0;
      const tokenEnd = r.tokenEnd ?? r.token_end ?? cursor;
      const n = result.candidates?.length ?? 0;
      opsLog("CMD", "complete_result", {
        before: previewText(line, 160),
        after: previewText(result.line, 160),
        cursorIn: cursor,
        cursorOut: result.cursor,
        tokenRange: [tokenStart, tokenEnd],
        n,
        candidatesPreview: (result.candidates || [])
          .slice(0, 12)
          .map((c) => previewText(c, 40)),
        filledCommand: previewText(result.line, 200),
        sid: this.sessionId.slice(0, 8),
      });
      if (!result.candidates || result.candidates.length === 0) {
        opsLog("CMD", "complete_empty", {
          line: previewText(line, 120),
          sid: this.sessionId.slice(0, 8),
        });
        this.hideCompletePopup();
        this.writeToTerm("\x07");
        return;
      }
      this.draft.text = result.line;
      this.draft.cursor = result.cursor;
      this.restoreDraftToInput();
      // Log the final draft field value after auto-fill (what the user sees).
      opsLog("CMD", "complete_filled", {
        kind: n === 1 ? "single" : "multi_or_prefix",
        command: previewText(this.draftInput.value, 200),
        cursor: this.draftInput.selectionStart,
        n,
        sid: this.sessionId.slice(0, 8),
      });
      if (result.candidates.length === 1) {
        this.hideCompletePopup();
        return;
      }
      this.completeUi = {
        candidates: result.candidates,
        index: 0,
        tokenStart,
        tokenEnd,
        baseLine: result.line,
        appliedLine: result.line,
        busy: false,
      };
      this.renderCompletePopup();
    } catch (e) {
      opsLog("ERR", "complete_failed", {
        error: String(e),
        line: previewText(line, 120),
        sid: this.sessionId.slice(0, 8),
      });
      this.hideCompletePopup();
      this.setError(String(e));
    }
  }

  async sendDraftLine() {
    this.hideCompletePopup();
    this.syncDraftFromInput();
    const line = this.draft.text;
    opsLog("UI", "draft_send_attempt", {
      line: previewText(line),
      live: this.isLive(),
      mode: this.inputMode,
      sid: this.sessionId.slice(0, 8),
    });
    if (!line.trim()) {
      this.setError("草稿为空，请输入命令后再发送");
      return;
    }
    if (!this.isLive()) {
      this.pendingDraft = line;
      // Still remember for ↑ history after the user reconnects.
      this.pushHistory(line);
      this.setError("当前未连接：草稿已保留，重连成功后将自动发送");
      return;
    }
    await this.flushPendingLine(line);
  }

  private async flushPendingLine(line: string) {
    const logical = line.replace(/[\r\n]+$/g, "");
    if (!logical.trim()) return;
    opsLog("CMD", "ui_submit_line", {
      line: previewText(logical),
      sid: this.sessionId.slice(0, 8),
    });
    // Record history before clear so ↑ works even if IPC fails later.
    this.pushHistory(logical);
    this.draft.text = "";
    this.draft.cursor = 0;
    this.restoreDraftToInput();
    this.setError(null);
    try {
      await invoke("submit_line", {
        sessionId: this.sessionId,
        line: logical,
      });
      opsLog("CMD", "ui_submit_line_ok", { line: previewText(logical) });
      this.draftInput.focus();
    } catch (e) {
      this.draft.text = logical;
      this.draft.cursor = logical.length;
      this.restoreDraftToInput();
      opsLog("ERR", "ui_submit_line_failed", {
        line: previewText(logical),
        error: String(e),
      });
      this.setError(String(e));
    }
  }

  async dispose() {
    if (this.disposed) return;
    this.disposed = true;
    try {
      await invoke("close_session", { sessionId: this.sessionId });
    } catch (e) {
      opsLog("ERR", "close_session failed", {
        sid: this.sessionId.slice(0, 8),
        error: String(e),
      });
    }
    try {
      this.term.dispose();
    } catch {
      /* ignore */
    }
    this.rootEl.remove();
  }
}

// ---------------------------------------------------------------------------
// Session map / tabs
// ---------------------------------------------------------------------------

const sessions = new Map<string, SessionView>();
let activeSessionId: string | null = null;
let windowResizeTimer: number | null = null;
let logsDir: string | null = null;

function getActive(): SessionView | null {
  if (!activeSessionId) return null;
  return sessions.get(activeSessionId) ?? null;
}

function updateEmptyState() {
  $("empty-state").classList.toggle("hidden", sessions.size > 0);
}

function allocateTitle(base: string): string {
  const used = new Set<string>();
  for (const v of sessions.values()) used.add(v.title);
  if (!used.has(base)) return base;
  let n = 2;
  while (used.has(`${base} (${n})`)) n++;
  return `${base} (${n})`;
}

function renderTabBar() {
  const host = $("tab-bar-tabs");
  host.innerHTML = "";
  for (const view of sessions.values()) {
    const tab = document.createElement("div");
    tab.className =
      "tab" + (view.sessionId === activeSessionId ? " active" : "");
    tab.setAttribute("role", "tab");
    tab.dataset.sessionId = view.sessionId;
    tab.title = `${view.title}\n${view.username}@${view.host}`;

    const dot = document.createElement("span");
    dot.className = `tab-dot ${view.state}`;

    const title = document.createElement("span");
    title.className = "tab-title";
    title.textContent = view.title;

    const close = document.createElement("button");
    close.type = "button";
    close.className = "tab-close";
    close.title = "关闭会话";
    close.textContent = "×";
    close.addEventListener("click", (ev) => {
      ev.stopPropagation();
      void closeSessionTab(view.sessionId);
    });

    tab.append(dot, title, close);
    tab.addEventListener("click", () => activate(view.sessionId));
    host.appendChild(tab);
  }
}

function syncGlobalStatusBar() {
  const view = getActive();
  const dot = $("status-dot");
  const text = $("status-text");
  const cwdEl = $("cwd-text");
  const hostEl = $("status-host");
  if (!view) {
    dot.className = "dot idle";
    text.textContent = "未连接";
    cwdEl.textContent = "";
    hostEl.textContent = "";
    return;
  }
  const labels: Record<SessionStateName, string> = {
    idle: "未连接",
    connecting: "连接中…",
    connected: "已连接",
    reconnecting: "重连中…",
    disconnected: "已断开",
    failed: "失败",
  };
  dot.className = `dot ${view.state}`;
  text.textContent = view.message || labels[view.state] || view.state;
  hostEl.textContent = `${view.title} · ${view.username}@${view.host}`;
  if (view.cwd) {
    cwdEl.textContent = view.cwd;
    cwdEl.title = `cwd: ${view.cwd}`;
  } else {
    cwdEl.textContent = "";
    cwdEl.title = "远端工作目录";
  }
}

function activate(sessionId: string) {
  if (!sessions.has(sessionId)) return;
  const prev = activeSessionId;
  activeSessionId = sessionId;
  for (const [id, v] of sessions) {
    v.setActive(id === sessionId);
  }
  renderTabBar();
  syncGlobalStatusBar();
  updateEmptyState();

  const view = sessions.get(sessionId)!;
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      view.fitAndResize();
      if (view.inputMode === "shell") view.draftInput.focus();
      else if (view.isLive()) view.term.focus();
    });
  });

  if (prev !== sessionId) {
    opsLog("UI", "tab_activate", { sid: sessionId.slice(0, 8) });
  }
}

async function closeSessionTab(sessionId: string) {
  const view = sessions.get(sessionId);
  if (!view) return;

  const needsConfirm =
    view.state === "connected" ||
    view.state === "connecting" ||
    view.state === "reconnecting";
  if (needsConfirm) {
    const ok = window.confirm(
      `关闭会话「${view.title}」？\n关闭将断开连接且无法恢复本页终端缓冲。`,
    );
    if (!ok) return;
  }

  opsLog("UI", "tab_close", { sid: sessionId.slice(0, 8) });
  // Prefer close first, then remove from map
  await view.dispose();
  sessions.delete(sessionId);

  if (activeSessionId === sessionId) {
    activeSessionId = null;
    const next = sessions.keys().next().value as string | undefined;
    if (next) activate(next);
    else {
      renderTabBar();
      syncGlobalStatusBar();
      updateEmptyState();
    }
  } else {
    renderTabBar();
    updateEmptyState();
  }
}

function createSessionView(opts: {
  host: string;
  username: string;
  profileId?: string | null;
  baseTitle: string;
  sessionId?: string;
}): SessionView {
  const sessionId = opts.sessionId || newSessionId();
  const title = allocateTitle(opts.baseTitle);
  const view = new SessionView({
    sessionId,
    title,
    host: opts.host,
    username: opts.username,
    profileId: opts.profileId,
  });
  sessions.set(sessionId, view);
  view.mount($("session-views"));
  return view;
}

function countActiveConnections(): number {
  let n = 0;
  for (const v of sessions.values()) {
    if (
      v.state === "connected" ||
      v.state === "connecting" ||
      v.state === "reconnecting"
    ) {
      n++;
    }
  }
  return n;
}

// ---------------------------------------------------------------------------
// Connect / disconnect
// ---------------------------------------------------------------------------

async function connectWithForm(opts: {
  forceNewTab?: boolean;
  reuseSessionId?: string | null;
  keepDialogOpen?: boolean;
}): Promise<boolean> {
  setPropsError(null);
  const { authType, host, port, username, profileName } = readForm();
  if (!host || !username) {
    setPropsError("请填写主机与用户名");
    return false;
  }
  const auth = await buildAuth(authType);
  if (!auth) return false;

  let view: SessionView;
  const reuseId = opts.reuseSessionId;
  const existing = reuseId ? sessions.get(reuseId) : null;

  if (existing && existing.canReconnectSameTab()) {
    view = existing;
    view.host = host;
    view.username = username;
    view.rebindCmdHistory();
    opsLog("UI", "connect_reuse_tab", { sid: view.sessionId.slice(0, 8) });
  } else {
    if (sessions.size >= MAX_TABS) {
      showToast(`最多打开 ${MAX_TABS} 个会话`);
      setPropsError(`最多打开 ${MAX_TABS} 个会话`);
      return false;
    }
    const baseTitle =
      profileName ||
      (selectedProfileId
        ? getProfiles().find((p) => p.id === selectedProfileId)?.name
        : null) ||
      `${username}@${host}`;
    view = createSessionView({
      host,
      username,
      profileId: selectedProfileId,
      baseTitle,
    });
    activate(view.sessionId);
  }

  view.lastForm = {
    host,
    port,
    username,
    authType,
    privateKeyPath:
      authType === "public_key"
        ? ($("private-key-path") as HTMLInputElement).value.trim()
        : null,
    profileId: selectedProfileId,
    profileName: profileName || undefined,
  };
  view.profileId = selectedProfileId;

  // Must fit *before* reading cols/rows (display:none → 80x24 default is wrong).
  const { cols, rows } = await view.fitThenDims();

  opsLog("UI", "click_connect", {
    host,
    port,
    username,
    authType,
    cols,
    rows,
    profile_id: selectedProfileId,
    session_id: view.sessionId.slice(0, 8),
  });

  propsConnecting = true;
  setPropsButtonsDisabled(true);
  view.applyState("connecting", "正在连接…");

  try {
    await invoke<{ session_id?: string; sessionId?: string }>("connect", {
      req: {
        session_id: view.sessionId,
        host,
        port,
        username,
        auth,
        cols,
        rows,
        profile_id: selectedProfileId,
      },
    });
    opsLog("UI", "connect invoke returned ok", {
      session_id: view.sessionId.slice(0, 8),
    });
    if (view.state !== "connected") {
      try {
        const snap = await invoke<SessionSnapshot>("get_session_snapshot", {
          sessionId: view.sessionId,
        });
        view.applyState(snap.state, snap.message, snap.cwd);
      } catch {
        view.applyState("connected", "已连接");
      }
    }
    // Size sync: only via session://state "connected" → fitAndResize (silent stty).
    // Avoid a second inject from this path (duplicate stty echo).
    requestAnimationFrame(() => view.fitAndResize());
    view.draftInput.focus();
    if (!opts.keepDialogOpen) {
      closePropsDialog();
    }
    return true;
  } catch (e) {
    const msg = String(e);
    opsLog("ERR", "connect invoke failed", { message: msg });
    setPropsError(msg);
    view.setError(msg);
    view.applyState("failed", msg);
    // keep dialog open for retry
    return false;
  } finally {
    propsConnecting = false;
    setPropsButtonsDisabled(false);
  }
}

async function disconnectActive() {
  const view = getActive();
  if (!view) {
    showToast("没有活动会话");
    return;
  }
  view.pendingDraft = null;
  opsLog("UI", "click_disconnect", {
    session_id: view.sessionId.slice(0, 8),
  });
  try {
    await invoke("disconnect", { sessionId: view.sessionId });
    view.applyState("idle", "已手动断开", view.cwd);
  } catch (e) {
    view.setError(String(e));
    showToast(String(e));
  }
}

// ---------------------------------------------------------------------------
// Props dialog
// ---------------------------------------------------------------------------

function setPropsButtonsDisabled(disabled: boolean) {
  const footer = $("dlg-props-footer");
  footer.querySelectorAll("button").forEach((b) => {
    (b as HTMLButtonElement).disabled = disabled;
  });
}

function renderPropsFooter() {
  const footer = $("dlg-props-footer");
  footer.innerHTML = "";
  const add = (
    label: string,
    opts: { primary?: boolean; action: string },
  ) => {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = label;
    if (opts.primary) b.className = "primary";
    b.dataset.propsAction = opts.action;
    footer.appendChild(b);
  };

  if (propsMode === "create") {
    add("连接", { primary: true, action: "connect" });
    add("保存并连接", { action: "save-connect" });
    add("仅保存", { action: "save" });
    add("取消", { action: "cancel" });
  } else if (propsMode === "edit-profile") {
    add("保存", { primary: true, action: "save" });
    add("取消", { action: "cancel" });
  } else if (propsMode === "reconnect") {
    add("连接", { primary: true, action: "connect" });
    add("取消", { action: "cancel" });
  } else if (propsMode === "edit-runtime") {
    add("连接", { primary: true, action: "connect" });
    add("取消", { action: "cancel" });
  }

  footer.querySelectorAll("button").forEach((btn) => {
    btn.addEventListener("click", () => {
      void onPropsAction((btn as HTMLElement).dataset.propsAction || "");
    });
  });
}

async function onPropsAction(action: string) {
  if (action === "cancel") {
    if (!propsConnecting) closePropsDialog();
    return;
  }
  if (propsConnecting) return;

  if (action === "save") {
    const id =
      propsMode === "edit-profile" ? selectedProfileId : selectedProfileId;
    const p = await saveProfileFromForm(id);
    if (p) {
      showToast("配置已保存");
      if (propsMode === "edit-profile") closePropsDialog();
      renderManagerList();
    }
    return;
  }

  if (action === "save-connect") {
    const p = await saveProfileFromForm(selectedProfileId);
    if (!p) return;
    selectedProfileId = p.id;
    await connectWithForm({
      forceNewTab: true,
      keepDialogOpen: true,
    });
    return;
  }

  if (action === "connect") {
    if (propsMode === "reconnect" || propsMode === "edit-runtime") {
      await connectWithForm({
        reuseSessionId: propsTargetSessionId,
        keepDialogOpen: true,
      });
    } else {
      await connectWithForm({ forceNewTab: true, keepDialogOpen: true });
    }
  }
}

function openPropsDialog(mode: PropsMode, opts?: { sessionId?: string }) {
  propsMode = mode;
  propsTargetSessionId = opts?.sessionId ?? null;
  setPropsError(null);
  setFormReadonly(false);

  const title = $("dlg-props-title");
  if (mode === "create") {
    title.textContent = "新建会话";
    clearForm();
  } else if (mode === "edit-profile") {
    title.textContent = "编辑会话配置";
  } else if (mode === "reconnect") {
    title.textContent = "重新连接";
    const v = opts?.sessionId
      ? sessions.get(opts.sessionId)
      : getActive();
    if (v?.lastForm) {
      fillFormFromSnapshot(v.lastForm, v.title);
    } else if (v) {
      fillFormFromSnapshot({
        host: v.host,
        port: 22,
        username: v.username,
        authType: "public_key",
        profileId: v.profileId,
      }, v.title);
    }
  } else if (mode === "edit-runtime") {
    title.textContent = "会话属性";
    const v = opts?.sessionId
      ? sessions.get(opts.sessionId)
      : getActive();
    if (v?.lastForm) {
      fillFormFromSnapshot(v.lastForm, v.title);
    } else if (v) {
      fillFormFromSnapshot({
        host: v.host,
        port: 22,
        username: v.username,
        authType: "public_key",
        profileId: v.profileId,
      }, v.title);
    }
    if (v?.isLive() || v?.isBusy()) {
      setFormReadonly(true);
      setPropsError("已连接：字段只读。请先断开后再改连接参数。");
    }
  }

  renderPropsFooter();
  // Hide Connect when viewing live session properties
  if (mode === "edit-runtime") {
    const v = opts?.sessionId
      ? sessions.get(opts.sessionId)
      : getActive();
    if (v?.isLive() || v?.isBusy()) {
      $("dlg-props-footer")
        .querySelectorAll('[data-props-action="connect"]')
        .forEach((b) => ((b as HTMLButtonElement).style.display = "none"));
    }
  }
  const dlg = $("dlg-session-props") as HTMLDialogElement;
  if (!dlg.open) dlg.showModal();
  const hostInput = $("host") as HTMLInputElement;
  if (!hostInput.disabled) hostInput.focus();
}

function closePropsDialog() {
  const dlg = $("dlg-session-props") as HTMLDialogElement;
  if (dlg.open) dlg.close();
  setFormReadonly(false);
  propsConnecting = false;
}

// ---------------------------------------------------------------------------
// Session manager
// ---------------------------------------------------------------------------

function renderManagerList() {
  const list = $("mgr-profile-list");
  list.innerHTML = "";
  const profiles = getProfiles();
  for (const p of profiles) {
    const li = document.createElement("li");
    li.dataset.id = p.id;
    if (p.id === mgrSelectedId) li.classList.add("selected");
    li.innerHTML = `<div class="name">${escapeHtml(p.name)}</div>
      <div class="meta">${escapeHtml(p.username)}@${escapeHtml(p.host)}:${p.port} · ${
        p.auth_type === "password" ? "密码" : "私钥"
      }</div>`;
    li.addEventListener("click", () => {
      mgrSelectedId = p.id;
      renderManagerList();
    });
    li.addEventListener("dblclick", () => {
      mgrSelectedId = p.id;
      void openProfileAsNewTab(p);
    });
    list.appendChild(li);
  }
}

async function openManager() {
  await loadProfiles();
  mgrSelectedId = null;
  renderManagerList();
  const dlg = $("dlg-session-manager") as HTMLDialogElement;
  if (!dlg.open) dlg.showModal();
}

async function openProfileAsNewTab(p: HostProfile) {
  fillFormFromProfile(p);
  ($("dlg-session-manager") as HTMLDialogElement).close();
  selectedProfileId = p.id;
  if (sessions.size >= MAX_TABS) {
    showToast(`最多打开 ${MAX_TABS} 个会话`);
    return;
  }

  // Secrets are never stored in profiles.json. Always prompt before connect when needed.
  let auth: Record<string, unknown>;
  if (p.auth_type === "password") {
    const prompted = await ensurePassword(null);
    if (prompted === undefined) {
      showToast("已取消连接");
      return;
    }
    auth = {
      type: "password",
      // empty → backend may load from Windows Credential Manager via profile_id
      password: prompted || null,
      save_password: false,
    };
  } else {
    if (!p.private_key_path) {
      showToast("配置缺少私钥路径");
      return;
    }
    const prompted = await ensurePassphrase(null);
    if (prompted === undefined) {
      showToast("已取消连接");
      return;
    }
    auth = {
      type: "public_key",
      private_key_path: p.private_key_path,
      passphrase: prompted || null,
    };
  }

  const view = createSessionView({
    host: p.host,
    username: p.username,
    profileId: p.id,
    baseTitle: p.name,
  });
  view.lastForm = {
    host: p.host,
    port: p.port,
    username: p.username,
    authType: p.auth_type,
    privateKeyPath: p.private_key_path,
    profileId: p.id,
    profileName: p.name,
  };
  activate(view.sessionId);
  const { cols, rows } = await view.fitThenDims();
  view.applyState("connecting", "正在连接…");
  try {
    await invoke("connect", {
      req: {
        session_id: view.sessionId,
        host: p.host,
        port: p.port,
        username: p.username,
        auth,
        cols,
        rows,
        profile_id: p.id,
      },
    });
    if (view.state !== "connected") {
      try {
        const snap = await invoke<SessionSnapshot>("get_session_snapshot", {
          sessionId: view.sessionId,
        });
        view.applyState(snap.state, snap.message, snap.cwd);
      } catch {
        view.applyState("connected", "已连接");
      }
    }
    requestAnimationFrame(() => view.fitAndResize());
  } catch (e) {
    view.setError(String(e));
    view.applyState("failed", String(e));
    showToast(String(e));
  }
}

// ---------------------------------------------------------------------------
// Menu + shortcuts
// ---------------------------------------------------------------------------

function closeAllMenus() {
  document
    .querySelectorAll(".menu-item.open")
    .forEach((el) => el.classList.remove("open"));
}

function setupMenubar() {
  document.querySelectorAll(".menu-item").forEach((item) => {
    const btn = item.querySelector(".menu-btn");
    btn?.addEventListener("click", (ev) => {
      ev.stopPropagation();
      const wasOpen = item.classList.contains("open");
      closeAllMenus();
      if (!wasOpen) item.classList.add("open");
    });
  });

  document.addEventListener("click", () => closeAllMenus());

  document.querySelectorAll("[data-action]").forEach((el) => {
    el.addEventListener("click", (ev) => {
      ev.stopPropagation();
      closeAllMenus();
      const action = (el as HTMLElement).dataset.action;
      if (action) void handleMenuAction(action);
    });
  });

  document.querySelectorAll("[data-dlg-close]").forEach((el) => {
    el.addEventListener("click", () => {
      const dlg = (el as HTMLElement).closest("dialog") as HTMLDialogElement;
      if (dlg?.id === "dlg-session-props" && propsConnecting) return;
      dlg?.close();
    });
  });
}

async function handleMenuAction(action: string) {
  const active = getActive();
  switch (action) {
    case "new-session":
      openPropsDialog("create");
      break;
    case "open-manager":
      await openManager();
      break;
    case "exit":
      await requestExit();
      break;
    case "copy": {
      const sel = active?.term.getSelection() || "";
      if (sel) {
        await navigator.clipboard.writeText(sel);
        showToast("已复制");
      }
      break;
    }
    case "paste": {
      if (!active) break;
      try {
        const text = await navigator.clipboard.readText();
        if (!text) break;
        if (active.inputMode === "shell") {
          const input = active.draftInput;
          const start = input.selectionStart ?? input.value.length;
          const end = input.selectionEnd ?? start;
          const v = input.value;
          input.value = v.slice(0, start) + text + v.slice(end);
          const pos = start + text.length;
          input.setSelectionRange(pos, pos);
          input.focus();
        } else if (active.isLive()) {
          const bytes = new TextEncoder().encode(text);
          await invoke("write_bytes", {
            sessionId: active.sessionId,
            dataB64: bytesToBase64(bytes),
          });
        }
      } catch (e) {
        showToast("无法读取剪贴板");
      }
      break;
    }
    case "clear-draft":
      active?.clearDraft();
      break;
    case "toggle-mode":
      active?.toggleInputMode();
      break;
    case "clear-screen":
      // Local only — never send remote clear
      active?.clearScreenLocal();
      break;
    case "disconnect":
      await disconnectActive();
      break;
    case "reconnect": {
      if (!active || !active.canReconnectSameTab()) {
        showToast("当前标签无法重新连接（需先断开或处于失败状态）");
        break;
      }
      openPropsDialog("reconnect", { sessionId: active.sessionId });
      break;
    }
    case "close-tab":
      if (active) await closeSessionTab(active.sessionId);
      break;
    case "session-props":
      if (!active) {
        openPropsDialog("create");
      } else {
        openPropsDialog("edit-runtime", { sessionId: active.sessionId });
      }
      break;
    case "open-logs":
      if (logsDir) {
        try {
          await openPath(logsDir);
        } catch (e) {
          showToast(`无法打开日志目录: ${e}`);
        }
      } else {
        showToast("日志目录未知");
      }
      break;
    case "about":
      ($("dlg-about") as HTMLDialogElement).showModal();
      break;
    case "shell-integration":
      ($("dlg-shell-help") as HTMLDialogElement).showModal();
      break;
  }
}

function setupShortcuts() {
  window.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    if (mod && e.key === "n") {
      e.preventDefault();
      openPropsDialog("create");
      return;
    }
    if (mod && e.key === "o") {
      e.preventDefault();
      void openManager();
      return;
    }
    if (mod && e.key === "w") {
      e.preventDefault();
      const a = getActive();
      if (a) void closeSessionTab(a.sessionId);
      return;
    }
    if (mod && e.key === "Tab") {
      e.preventDefault();
      const ids = [...sessions.keys()];
      if (ids.length < 2) return;
      const i = activeSessionId ? ids.indexOf(activeSessionId) : -1;
      const next = e.shiftKey
        ? ids[(i - 1 + ids.length) % ids.length]
        : ids[(i + 1) % ids.length];
      activate(next);
      return;
    }
    if (mod && e.shiftKey && (e.key === "C" || e.key === "c")) {
      e.preventDefault();
      void handleMenuAction("copy");
      return;
    }
    if (mod && e.shiftKey && (e.key === "V" || e.key === "v")) {
      e.preventDefault();
      void handleMenuAction("paste");
      return;
    }
    if (mod && e.shiftKey && (e.key === "M" || e.key === "m")) {
      e.preventDefault();
      void handleMenuAction("toggle-mode");
      return;
    }
  });
}

/** Set while shutting down so close-requested handlers do not re-enter. */
let isQuitting = false;

/**
 * Dispose all tabs with a per-tab timeout so a stuck IPC cannot block exit forever.
 */
async function cleanupAllSessionsForExit() {
  const ids = [...sessions.keys()];
  for (const id of ids) {
    const v = sessions.get(id);
    if (!v) continue;
    try {
      await Promise.race([
        v.dispose(),
        new Promise<void>((resolve) => window.setTimeout(resolve, 1500)),
      ]);
    } catch {
      /* ignore */
    }
    sessions.delete(id);
  }
  sessions.clear();
  activeSessionId = null;
}

/**
 * App exit (menu 退出).
 *
 * Must NOT call `window.close()` from inside `onCloseRequested` after
 * `preventDefault()` — that re-enters the close pipeline and can deadlock
 * (window stays open / process never dies). Prefer backend `app_quit` → `app.exit(0)`.
 */
async function requestExit() {
  if (isQuitting) {
    opsLog("SYS", "app_exit_ignored_already_quitting");
    return;
  }
  const n = countActiveConnections();
  if (n > 0) {
    const ok = window.confirm(
      `有 ${n} 个会话仍在连接中，退出将全部断开。确定退出？`,
    );
    if (!ok) {
      opsLog("UI", "app_exit_cancelled", { active: n });
      return;
    }
  }
  isQuitting = true;
  opsLog("SYS", "app_exit_begin", {
    active: n,
    tabs: sessions.size,
    source: "menu_or_api",
  });
  await cleanupAllSessionsForExit();
  opsLog("SYS", "app_exit_cleanup_done");
  try {
    await invoke("app_quit");
  } catch (e) {
    opsLog("ERR", "app_quit_invoke_failed", String(e));
    // Fallback: force-destroy window (does not re-fire closeRequested).
    try {
      await getCurrentWindow().destroy();
    } catch {
      try {
        await getCurrentWindow().close();
      } catch {
        window.close();
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

function eventRouteMiss(event: string, sid: string | undefined) {
  opsLog("ERR", "event_route_miss", {
    event,
    sid: sid ? sid.slice(0, 8) : null,
    active: activeSessionId ? activeSessionId.slice(0, 8) : null,
  });
}

async function setupEvents() {
  await listen<DataEvent>("session://data", (event) => {
    try {
      const p = event.payload;
      const sid = payloadSessionId(p || {});
      if (!sid || !sessions.has(sid)) {
        eventRouteMiss("session://data", sid);
        return;
      }
      const view = sessions.get(sid)!;
      const b64 = p.data_b64 || p.dataB64 || "";
      if (!b64) return;
      const bytes = base64ToBytes(b64);
      const text = new TextDecoder("utf-8", { fatal: false }).decode(bytes);
      opsLog("ECHO", "ui_receive", {
        len: bytes.length,
        text: previewText(text, 200),
        sid: sid.slice(0, 8),
      });
      view.writeToTerm(bytes);
    } catch (e) {
      console.error("session://data decode failed", e);
      opsLog("ERR", "echo decode failed", { error: String(e) });
    }
  });

  await listen<CwdEvent>("session://cwd", (event) => {
    const p = event.payload;
    const sid = payloadSessionId(p || {});
    if (!sid || !sessions.has(sid)) {
      eventRouteMiss("session://cwd", sid);
      return;
    }
    const view = sessions.get(sid)!;
    view.cwd = p.cwd || null;
    opsLog("CWD", "ui_cwd_event", {
      path: p.cwd,
      sid: sid.slice(0, 8),
    });
    if (activeSessionId === sid) syncGlobalStatusBar();
  });

  await listen<SessionSnapshot & { sessionId?: string }>(
    "session://state",
    (event) => {
      const snap = event.payload as SessionSnapshot & { sessionId?: string };
      const sid = payloadSessionId(snap || {});
      if (!sid || !sessions.has(sid)) {
        eventRouteMiss("session://state", sid);
        return;
      }
      const view = sessions.get(sid)!;
      opsLog("STATE", "ui_state_event", {
        state: snap.state,
        message: snap.message ?? null,
        cwd: snap.cwd ?? null,
        sid: sid.slice(0, 8),
      });
      view.applyState(snap.state, snap.message, snap.cwd);
      if (snap.state === "failed" && snap.message) {
        view.setError(snap.message);
      }
      if (snap.state === "connected") {
        requestAnimationFrame(() => {
          if (activeSessionId === sid) view.fitAndResize();
        });
      }
    },
  );

  await listen<ErrorEvent>("session://error", (event) => {
    const p = event.payload;
    const sid = payloadSessionId(p || {});
    if (!sid || !sessions.has(sid)) {
      eventRouteMiss("session://error", sid);
      return;
    }
    if (p.message) sessions.get(sid)!.setError(p.message);
  });
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

window.addEventListener("DOMContentLoaded", async () => {
  syncAuthFields();
  updateEmptyState();
  renderTabBar();
  syncGlobalStatusBar();
  setupMenubar();
  setupShortcuts();
  await setupEvents();

  $("auth-type").addEventListener("change", () => syncAuthFields());

  $("btn-new-tab").addEventListener("click", () => openPropsDialog("create"));
  $("btn-empty-new").addEventListener("click", () => openPropsDialog("create"));
  $("btn-empty-manager").addEventListener("click", () => void openManager());

  $("mgr-btn-new").addEventListener("click", () => {
    ($("dlg-session-manager") as HTMLDialogElement).close();
    openPropsDialog("create");
  });
  $("mgr-btn-edit").addEventListener("click", () => {
    if (!mgrSelectedId) {
      showToast("请先选择配置");
      return;
    }
    const p = getProfiles().find((x) => x.id === mgrSelectedId);
    if (!p) return;
    fillFormFromProfile(p);
    ($("dlg-session-manager") as HTMLDialogElement).close();
    openPropsDialog("edit-profile");
  });
  $("mgr-btn-delete").addEventListener("click", async () => {
    if (!mgrSelectedId) {
      showToast("请先选择配置");
      return;
    }
    if (!window.confirm("删除该主机配置？（不会关闭已打开的标签）")) return;
    try {
      await invoke("delete_profile", { id: mgrSelectedId });
      mgrSelectedId = null;
      await loadProfiles();
      renderManagerList();
      showToast("已删除配置");
    } catch (e) {
      showToast(String(e));
    }
  });
  $("mgr-btn-open").addEventListener("click", () => {
    if (!mgrSelectedId) {
      showToast("请先选择配置");
      return;
    }
    const p = getProfiles().find((x) => x.id === mgrSelectedId);
    if (p) void openProfileAsNewTab(p);
  });

  // Prevent form submit default
  $("form-session-props").addEventListener("submit", (e) => e.preventDefault());

  window.addEventListener("resize", () => {
    if (windowResizeTimer != null) window.clearTimeout(windowResizeTimer);
    windowResizeTimer = window.setTimeout(() => {
      getActive()?.fitAndResize();
    }, 100);
  });

  // Title-bar X / Alt+F4 — Tauri pattern:
  // await confirm first; only preventDefault if user cancels.
  // Never call close() again from inside this handler after preventDefault
  // (re-entrancy deadlock: handler waits on close, close waits on handler).
  try {
    const win = getCurrentWindow();
    await win.onCloseRequested(async (event) => {
      if (isQuitting) {
        // Already decided to quit (menu exit / app_quit in flight) — allow.
        return;
      }
      const n = countActiveConnections();
      if (n > 0) {
        const ok = window.confirm(
          `有 ${n} 个会话仍在连接中，退出将全部断开。确定退出？`,
        );
        if (!ok) {
          event.preventDefault();
          opsLog("UI", "window_close_cancelled", { active: n });
          return;
        }
      }
      // Accept close: cleanup while Tauri waits for this async handler, then
      // let the default close proceed (do not preventDefault, do not re-close).
      isQuitting = true;
      opsLog("SYS", "window_close_accepted", {
        active: n,
        tabs: sessions.size,
        source: "close_requested",
      });
      await cleanupAllSessionsForExit();
      opsLog("SYS", "window_close_cleanup_done");
      // Ensure process exits even if window teardown leaves runtime tasks alive.
      invoke("app_quit").catch((e) => {
        opsLog("ERR", "app_quit_from_close_failed", String(e));
      });
    });
  } catch {
    /* ignore if API unavailable */
  }

  try {
    await loadProfiles();
    const info = await invoke<{
      dir: string;
      latest: string;
      session?: string;
      source?: string;
    }>("ops_log_info");
    logsDir = info.dir;
    opsLog("SYS", "ui_ready", {
      ...info,
      multi_tab: true,
      max_tabs: MAX_TABS,
      pr4_menubar: true,
    });
  } catch (e) {
    showToast(`启动失败: ${e}`);
  }

  // Phase B: surface missing OpenSSH clearly (do not silently fail on connect only).
  await checkSshOnStartup();
});

interface SshCheckResult {
  available: boolean;
  path?: string | null;
  message: string;
  helpUrl?: string | null;
}

async function checkSshOnStartup() {
  try {
    const r = await invoke<SshCheckResult>("check_ssh");
    if (r.available) {
      opsLog("SYS", "ssh_check_ok", { path: r.path ?? null });
      return;
    }
    opsLog("SYS", "ssh_check_missing", { message: r.message });
    showSshMissingDialog(r);
  } catch (e) {
    opsLog("ERR", "ssh_check_invoke_failed", String(e));
  }
}

function showSshMissingDialog(r: SshCheckResult) {
  const dlg = $("dlg-ssh-missing") as HTMLDialogElement;
  const msg = $("dlg-ssh-missing-msg");
  if (r.message) msg.textContent = r.message;

  const helpBtn = $("dlg-ssh-missing-help") as HTMLButtonElement;
  const helpUrl =
    r.helpUrl ||
    "https://learn.microsoft.com/windows-server/administration/openssh/openssh_install_firstuse";
  helpBtn.onclick = () => {
    void openUrl(helpUrl).catch((e) => showToast(`无法打开链接: ${e}`));
  };

  if (!dlg.open) dlg.showModal();
  showToast("未检测到系统 OpenSSH（ssh.exe）", 4500);
}
