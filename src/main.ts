/**
 * AnchorTerm frontend — PR4: menubar + dialogs, no sidebar, multi-tab SessionView.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  openPath,
  openUrl,
  revealItemInDir,
} from "@tauri-apps/plugin-opener";
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
/** vim-like editor sub-mode while alternate screen is active. */
type EditorSubMode = "normal" | "insert" | "replace";
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
  has_saved_passphrase?: boolean;
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

/**
 * Defer ops_log IPC so it never runs re-entrantly inside a Tauri event
 * handler (e.g. `session://data` → invoke while emit is still on the stack).
 * That pattern can deadlock the runtime during high-rate tail/grep floods.
 */
function opsLogDeferred(
  category: string,
  message: string,
  detail?: Record<string, unknown> | string | null,
) {
  window.setTimeout(() => opsLog(category, message, detail), 0);
}

/** Per-session budget for large ui_receive ops-log lines (reset every 2s). */
const uiEchoLogBudget = new Map<string, { n: number; resetAt: number }>();

function shouldLogUiEcho(sid: string): boolean {
  const now = Date.now();
  let b = uiEchoLogBudget.get(sid);
  if (!b || now >= b.resetAt) {
    b = { n: 20, resetAt: now + 2000 };
    uiEchoLogBudget.set(sid, b);
  }
  if (b.n <= 0) return false;
  b.n -= 1;
  return true;
}

function previewText(s: string, max = 160): string {
  const one = s
    .replace(/\r/g, "\\r")
    .replace(/\n/g, "\\n")
    .replace(/\t/g, "\\t");
  return one.length > max ? one.slice(0, max) + "…" : one;
}

/** DEC private modes that switch the terminal to the alternate screen buffer. */
const ALT_SCREEN_MODES = new Set([47, 1047, 1049]);

/**
 * CSI private-mode set/reset: ESC [ ? <nums> h/l
 * (e.g. vim: ESC[?1049h enter, ESC[?1049l leave).
 */
const ALT_SCREEN_CSI_RE = /\x1b\[\?([0-9;]+)([hl])/g;

/**
 * Keep a short tail that might be an incomplete CSI private-mode sequence
 * spanning chunks (ESC, ESC[, ESC[?, ESC[?1049, …).
 */
function altScreenResidualTail(buf: string): string {
  const max = 48;
  const start = Math.max(0, buf.length - max);
  const slice = buf.slice(start);
  const esc = slice.lastIndexOf("\x1b");
  if (esc < 0) return "";
  const frag = slice.slice(esc);
  // Complete private-mode sequence → no residual needed for that match.
  if (/^\x1b\[\?[0-9;]+[hl]/.test(frag)) {
    const m = frag.match(/^\x1b\[\?[0-9;]+[hl]/);
    return m ? altScreenResidualTail(frag.slice(m[0].length)) : "";
  }
  // Incomplete private-mode CSI we still care about.
  if (/^\x1b(\[\??|\[[?][0-9;]*)?$/.test(frag)) return frag;
  return "";
}

/**
 * Scan a stream chunk for alt-screen private modes. Walks all complete CSIs so
 * enter+exit in one chunk nets correctly; incomplete CSI is kept in residual.
 */
function scanAltScreenChunk(
  residual: string,
  chunk: string,
  currentlyActive: boolean,
): {
  residual: string;
  nextActive: boolean;
  lastSeq: string | null;
  lastModes: number[];
} {
  const data = residual + chunk;
  ALT_SCREEN_CSI_RE.lastIndex = 0;
  let nextActive = currentlyActive;
  let lastSeq: string | null = null;
  let lastModes: number[] = [];
  let m: RegExpExecArray | null;
  while ((m = ALT_SCREEN_CSI_RE.exec(data)) !== null) {
    const modes = m[1]
      .split(";")
      .map((x) => Number(x))
      .filter((n) => Number.isFinite(n));
    if (!modes.some((n) => ALT_SCREEN_MODES.has(n))) continue;
    const set = m[2] === "h";
    nextActive = set;
    lastSeq = m[0];
    lastModes = modes.filter((n) => ALT_SCREEN_MODES.has(n));
  }
  return {
    residual: altScreenResidualTail(data),
    nextActive,
    lastSeq,
    lastModes,
  };
}

/**
 * Detect vim/neovim status-line editor mode from remote stream.
 * Order matters: REPLACE before INSERT.
 * Returns null if this chunk does not indicate a mode change.
 */
function scanEditorSubMode(text: string): EditorSubMode | null {
  // Replace / 替换 (Ins while already inserting, or R)
  if (
    /--\s*REPLACE\s*--/i.test(text) ||
    /REPLACE\s*--/.test(text) ||
    /--\s*替换\s*-*/.test(text) ||
    /\[1mREPLACE/i.test(text)
  ) {
    return "replace";
  }
  // Insert / 插入 (Ins, i, a, o, …)
  if (
    /--\s*INSERT\s*--/i.test(text) ||
    /INSERT\s*--/.test(text) ||
    /--\s*插入\s*-*/.test(text) ||
    /\[1m--\s*INSERT/i.test(text) ||
    /\[1mINSERT/i.test(text)
  ) {
    return "insert";
  }
  // Visual is not "editing text", treat as normal for our tip purposes.
  if (/--\s*VISUAL/i.test(text) || /--\s*可视/.test(text)) {
    return "normal";
  }
  return null;
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
// App UI settings (localStorage — survives restart)
// ---------------------------------------------------------------------------

const SETTINGS_STORE_KEY = "anchorterm.settings.v1";

interface AppUiSettings {
  /** After each Shell draft command finishes, print a green separator line. */
  cmdSeparator: boolean;
  /** On first Connected of a tab, auto-launch Xftp (once per tab lifetime). */
  xftpAutoLaunch: boolean;
}

const DEFAULT_SETTINGS: AppUiSettings = {
  cmdSeparator: false,
  xftpAutoLaunch: false,
};

function loadAppSettings(): AppUiSettings {
  try {
    const raw = localStorage.getItem(SETTINGS_STORE_KEY);
    if (!raw) return { ...DEFAULT_SETTINGS };
    const parsed = JSON.parse(raw) as Partial<AppUiSettings>;
    return {
      cmdSeparator: Boolean(parsed?.cmdSeparator),
      xftpAutoLaunch: Boolean(parsed?.xftpAutoLaunch),
    };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}

function saveAppSettings(s: AppUiSettings) {
  try {
    localStorage.setItem(SETTINGS_STORE_KEY, JSON.stringify(s));
  } catch (e) {
    opsLog("ERR", "settings_persist_failed", { error: String(e) });
  }
}

let appSettings: AppUiSettings = loadAppSettings();

function isCmdSeparatorEnabled(): boolean {
  return appSettings.cmdSeparator;
}

function setCmdSeparatorEnabled(on: boolean) {
  appSettings = { ...appSettings, cmdSeparator: on };
  saveAppSettings(appSettings);
  syncCmdSeparatorMenu();
  opsLog("UI", "cmd_separator_toggle", { enabled: on });
}

function syncCmdSeparatorMenu() {
  const btn = document.getElementById("menu-cmd-separator");
  if (!btn) return;
  const on = isCmdSeparatorEnabled();
  btn.setAttribute("aria-checked", on ? "true" : "false");
}

function isXftpAutoLaunchEnabled(): boolean {
  return appSettings.xftpAutoLaunch;
}

function setXftpAutoLaunchEnabled(on: boolean) {
  appSettings = { ...appSettings, xftpAutoLaunch: on };
  saveAppSettings(appSettings);
  syncXftpAutoMenu();
  opsLog("UI", "xftp_auto_toggle", { enabled: on });
}

function syncXftpAutoMenu() {
  const on = isXftpAutoLaunchEnabled();
  document
    .querySelectorAll("#menu-xftp-auto, .menu-xftp-auto-mirror")
    .forEach((btn) => {
      btn.setAttribute("aria-checked", on ? "true" : "false");
    });
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

let toastHideTimer: number | null = null;

function showToast(
  msg: string,
  ms = 2800,
  opts?: { tone?: "info" | "warn" | "ok"; position?: "top" | "bottom" },
) {
  let el = document.getElementById("app-toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "app-toast";
    el.className = "toast hidden";
    document.body.appendChild(el);
  }
  const tone = opts?.tone ?? "info";
  const position = opts?.position ?? (tone === "warn" ? "top" : "bottom");
  el.className = `toast toast-${tone} toast-${position}`;
  el.textContent = msg;
  el.classList.remove("hidden");
  if (toastHideTimer != null) window.clearTimeout(toastHideTimer);
  toastHideTimer = window.setTimeout(() => {
    el?.classList.add("hidden");
    toastHideTimer = null;
  }, ms);
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

/**
 * Ensure public-key auth has a passphrase when needed.
 * - Form value wins.
 * - Only skip prompt when profile flag says keyring has it (NOT merely the
 *   "save" checkbox — checkbox means "write after connect", not "already stored").
 * - Otherwise prompt (empty allowed for unencrypted keys). Cancel → undefined.
 */
async function ensurePassphrase(
  current: string | null | undefined,
  keyringHasSecret?: boolean,
): Promise<string | null | undefined> {
  if (current && current.length > 0) return current;
  if (keyringHasSecret) {
    opsLog("UI", "passphrase_use_keyring");
    return null; // backend loads from keyring via profile_id
  }
  opsLog("UI", "prompt_passphrase_before_connect");
  const entered = await promptSecret({
    title: "私钥口令",
    label: "私钥口令 / 证书密码",
    hint: "该私钥可能已加密。口令默认不写入配置；可勾选「保存私钥口令」写入系统凭据库。无口令可留空后确定。",
    allowEmpty: true,
  });
  if (entered === null) return undefined; // cancelled
  return entered;
}

/**
 * Ensure password auth has a password when needed.
 * - Form value wins.
 * - Only skip prompt when profile reports keyring has a password.
 * - "保存密码" checkbox alone must NOT skip the prompt (that was a bug).
 */
async function ensurePassword(
  current: string | null | undefined,
  keyringHasSecret?: boolean,
): Promise<string | null | undefined> {
  if (current && current.length > 0) return current;
  if (keyringHasSecret) {
    opsLog("UI", "password_use_keyring");
    return null;
  }
  opsLog("UI", "prompt_password_before_connect");
  const entered = await promptSecret({
    title: "登录密码",
    label: "密码",
    hint: "请输入登录密码。勾选「保存登录密码」可在连接成功后写入系统凭据库；下次可自动填充。",
    allowEmpty: false,
  });
  if (entered === null) return undefined;
  // allowEmpty false → empty string still possible if user clears; treat as cancel
  if (!entered || entered.length === 0) {
    return undefined;
  }
  return entered;
}

function updateSecretSaveHints() {
  const pwHint = document.getElementById("save-password-hint");
  const ppHint = document.getElementById("save-passphrase-hint");
  const savePw = ($("save-password") as HTMLInputElement).checked;
  const savePp = ($("save-passphrase") as HTMLInputElement | null)?.checked ?? false;
  const profile = selectedProfileId
    ? getProfiles().find((p) => p.id === selectedProfileId)
    : undefined;
  const authType = ($("auth-type") as HTMLSelectElement).value as AuthType;

  if (pwHint) {
    if (authType === "password" && savePw) {
      pwHint.hidden = false;
      pwHint.textContent = profile?.has_saved_password
        ? "已保存登录密码；留空（或 ******）将使用凭据库中的密码。"
        : "勾选后，连接/保存成功会把密码写入系统凭据库。";
    } else {
      pwHint.hidden = true;
    }
  }
  if (ppHint) {
    if (authType === "public_key" && savePp) {
      ppHint.hidden = false;
      ppHint.textContent = profile?.has_saved_passphrase
        ? "已保存私钥口令；留空（或 ******）将使用凭据库中的口令。"
        : "勾选后，连接/保存成功会把口令写入系统凭据库。";
    } else {
      ppHint.hidden = true;
    }
  }
}

// ---------------------------------------------------------------------------
// Form / dialogs (connection properties)
// ---------------------------------------------------------------------------

let selectedProfileId: string | null = null;
let propsMode: PropsMode = "create";
/** When reconnect / edit-runtime: target session id. */
let propsTargetSessionId: string | null = null;
let propsConnecting = false;
/** After closing session-props, re-open the session manager (edit/new from manager). */
let propsReturnToManager = false;
let mgrSelectedId: string | null = null;
/** Checked profile ids in the session manager (batch open / export). */
const mgrCheckedIds = new Set<string>();
/** Anchor for Shift+click checkbox range. */
let mgrCheckAnchorId: string | null = null;

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

/** Shown in password/passphrase fields when a secret is stored in the keyring. */
const SECRET_PLACEHOLDER = "******";

function isSecretPlaceholder(v: string | null | undefined): boolean {
  if (v == null) return false;
  const t = v.trim();
  // Common mask lengths users might see / type by habit.
  return t === SECRET_PLACEHOLDER || /^[*•●]{4,16}$/.test(t);
}

/** Form field value for connect/save: placeholder → treat as empty (use keyring). */
function readSecretField(id: string): string | null {
  const el = document.getElementById(id) as HTMLInputElement | null;
  if (!el) return null;
  const v = el.value;
  if (!v || isSecretPlaceholder(v)) return null;
  return v;
}

function clearFormSecrets() {
  ($("password") as HTMLInputElement).value = "";
  ($("passphrase") as HTMLInputElement).value = "";
}

/** Fill ****** when keyring has a saved secret (never the real password). */
function applySavedSecretMasks(p: HostProfile) {
  const pw = $("password") as HTMLInputElement;
  const pp = $("passphrase") as HTMLInputElement;
  if (p.auth_type === "password" && p.has_saved_password) {
    pw.value = SECRET_PLACEHOLDER;
    pw.dataset.masked = "1";
  } else {
    pw.value = "";
    delete pw.dataset.masked;
  }
  if (p.auth_type === "public_key" && p.has_saved_passphrase) {
    pp.value = SECRET_PLACEHOLDER;
    pp.dataset.masked = "1";
  } else {
    pp.value = "";
    delete pp.dataset.masked;
  }
}

/** Clear mask on focus so the user can type a new secret. */
function setupSecretMaskClearOnEdit() {
  for (const id of ["password", "passphrase"]) {
    const el = document.getElementById(id) as HTMLInputElement | null;
    if (!el || el.dataset.maskHooked === "1") continue;
    el.dataset.maskHooked = "1";
    el.addEventListener("focus", () => {
      if (el.dataset.masked === "1" || isSecretPlaceholder(el.value)) {
        el.value = "";
        delete el.dataset.masked;
      }
    });
    el.addEventListener("input", () => {
      if (el.dataset.masked === "1") delete el.dataset.masked;
    });
  }
}

function fillFormFromProfile(p: HostProfile) {
  selectedProfileId = p.id;
  ($("profile-name") as HTMLInputElement).value = p.name;
  ($("host") as HTMLInputElement).value = p.host;
  ($("port") as HTMLInputElement).value = String(p.port);
  ($("username") as HTMLInputElement).value = p.username;
  ($("auth-type") as HTMLSelectElement).value = p.auth_type;
  ($("private-key-path") as HTMLInputElement).value = p.private_key_path || "";
  ($("save-password") as HTMLInputElement).checked = !!p.has_saved_password;
  const savePp = $("save-passphrase") as HTMLInputElement | null;
  if (savePp) savePp.checked = !!p.has_saved_passphrase;
  applySavedSecretMasks(p);
  syncAuthFields();
  updateSecretSaveHints();
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
  const savePp = $("save-passphrase") as HTMLInputElement | null;
  if (savePp) savePp.checked = false;
  clearFormSecrets();
  syncAuthFields();
  updateSecretSaveHints();
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
  const savePp = $("save-passphrase") as HTMLInputElement | null;
  if (savePp) savePp.checked = false;
  clearFormSecrets();
  syncAuthFields();
  updateSecretSaveHints();
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
    "save-passphrase",
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
 * **before** connect (or uses keyring when saved). Secrets never go into
 * profiles.json — only Windows Credential Manager when the user opts in.
 * Returns null if validation fails or user cancels a required prompt.
 */
async function buildAuth(
  authType: AuthType,
): Promise<Record<string, unknown> | null> {
  const profile = selectedProfileId
    ? getProfiles().find((p) => p.id === selectedProfileId)
    : undefined;

  if (authType === "password") {
    const save_password = ($("save-password") as HTMLInputElement).checked;
    // ****** means "use keyring", not a literal password.
    let password = readSecretField("password");
    const keyringHas = !!profile?.has_saved_password;
    const prompted = await ensurePassword(password, keyringHas);
    if (prompted === undefined) {
      setPropsError("已取消连接或未填写密码");
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
  const save_passphrase =
    ($("save-passphrase") as HTMLInputElement | null)?.checked ?? false;
  let passphrase = readSecretField("passphrase");
  const keyringHasPp = !!profile?.has_saved_passphrase;
  const prompted = await ensurePassphrase(passphrase, keyringHasPp);
  if (prompted === undefined) {
    setPropsError("已取消连接");
    return null;
  }
  passphrase = prompted || null;
  // Reflect real typed passphrase into form (never write ****** back as real).
  if (passphrase && !isSecretPlaceholder(passphrase)) {
    ($("passphrase") as HTMLInputElement).value = passphrase;
    delete ($("passphrase") as HTMLInputElement).dataset.masked;
  }
  return {
    type: "public_key",
    private_key_path,
    passphrase,
    save_passphrase,
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
        // Placeholder ****** must not be written to keyring as a real password.
        password:
          authType === "password" ? readSecretField("password") : null,
        save_passphrase:
          authType === "public_key" &&
          !!($("save-passphrase") as HTMLInputElement | null)?.checked,
        passphrase:
          authType === "public_key" ? readSecretField("passphrase") : null,
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
  /** True after auto-launch Xftp was attempted for this tab (once per lifetime). */
  xftpAutoLaunched = false;

  term: Terminal;
  fitAddon: FitAddon;
  draft: PendingInputBuffer = { text: "", cursor: 0 };
  inputMode: InputMode = "shell";
  pendingDraft: string | null = null;
  completeUi: CompleteUiState | null = null;

  /**
   * True while the remote TUI holds the alternate screen (vim/htop/less…).
   * Detected from DEC private modes 47 / 1047 / 1049 in the PTY stream.
   * When set, Shell-mode draft send is blocked so commands don't inject into TUI.
   */
  altScreenActive = false;
  /** Incomplete CSI tail across `session://data` chunks. */
  private altScreenResidual = "";
  /** Sequence that last toggled alt-screen (for logs / status title). */
  altScreenLastSeq: string | null = null;
  /**
   * vim 等编辑器子模式（仅备用屏有效）。
   * 由远端状态行（-- INSERT -- / REPLACE）或本地 Esc / Ins 推断。
   */
  editorSubMode: EditorSubMode = "normal";

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

  /**
   * UI-side diagnostics after a post-separator submit (correlates with backend SEP).
   * Reset on each separator-enabled submit; logs cumulative ui_receive stats.
   */
  private sepUi: {
    active: boolean;
    startedAt: number;
    chunks: number;
    bytes: number;
    lastAt: number;
    lastLogAt: number;
  } | null = null;

  rootEl: HTMLElement;
  termHost: HTMLElement;
  draftWrap: HTMLElement;
  tuiBanner: HTMLElement;
  draftInput: HTMLInputElement;
  btnSend: HTMLButtonElement;
  btnMode: HTMLButtonElement;
  completePopup: HTMLUListElement;
  overlay: HTMLElement;
  overlayText: HTMLElement;
  errorEl: HTMLElement;

  private disposed = false;
  /** Dispose handle for xterm buffer-change listener. */
  private bufferChangeDisposable: { dispose(): void } | null = null;

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

    this.draftWrap = document.createElement("div");
    this.draftWrap.className = "draft-wrap";

    this.tuiBanner = document.createElement("div");
    this.tuiBanner.className = "tui-banner hidden";
    this.tuiBanner.setAttribute("role", "status");
    this.tuiBanner.setAttribute("aria-live", "polite");

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
      "<p><b>全屏程序锁定</b></p>",
      "<ul>",
      "<li>检测到备用屏（vim 等）时，Shell 草稿发送会自动锁定</li>",
      "<li>请切换到 TUI 直通操作；vim 用 <code>Esc</code> 后 <code>:q!</code> 退出</li>",
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
    this.draftWrap.append(this.tuiBanner, this.completePopup, draftBar);

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

    this.rootEl.append(
      this.termHost,
      this.errorEl,
      this.draftWrap,
      this.overlay,
    );

    this.term = new Terminal({
      cursorBlink: true,
      fontSize: 14,
      fontFamily: 'Consolas, "Cascadia Mono", "Courier New", monospace',
      convertEol: true,
      // Cap scrollback so multi-MB tail/grep dumps do not balloon WebView memory
      // and freeze the UI thread while painting.
      scrollback: 5000,
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

    // Secondary detection: trust xterm's own alternate/normal buffer switch.
    this.bufferChangeDisposable = this.term.buffer.onBufferChange((buf) => {
      const onAlt = buf.type === "alternate";
      if (onAlt !== this.altScreenActive) {
        this.setAltScreenActive(onAlt, "xterm_buffer_change", {
          seq: onAlt ? "buffer:alternate" : "buffer:normal",
          modes: onAlt ? [1049] : [],
        });
      }
    });

    this.wireTerminal();
    this.wireDraft();
  }

  isLive(): boolean {
    return this.state === "connected";
  }

  /** Send ETX (Ctrl+C) to the remote PTY. */
  sendInterrupt() {
    if (!this.isLive()) return;
    const bytes = new TextEncoder().encode("\x03");
    invoke("write_bytes", {
      sessionId: this.sessionId,
      dataB64: bytesToBase64(bytes),
    }).catch((e) => {
      opsLog("ERR", "interrupt write failed", { error: String(e) });
      this.setError(String(e));
    });
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
      this.btnMode.textContent = this.altScreenActive
        ? "Shell · 已锁定"
        : "Shell 模式";
      this.btnMode.classList.remove("raw");
      this.btnMode.title = this.altScreenActive
        ? "当前：Shell 模式 · 全屏程序运行中，发送已锁定 · 点击切换 TUI 直通"
        : "当前：Shell 模式 · 点击切换为 TUI 直通";
      this.termHost.classList.add("shell-mode");
      this.term.options.disableStdin = true;
    } else {
      this.btnMode.textContent = this.altScreenActive
        ? "TUI · 全屏中"
        : "TUI 直通";
      this.btnMode.classList.add("raw");
      this.btnMode.title = this.altScreenActive
        ? "当前：TUI 直通 · 全屏程序运行中，按键直达远端 · 点击可切回 Shell（发送仍锁定）"
        : "当前：TUI 直通 · 点击切换为 Shell 模式";
      this.termHost.classList.remove("shell-mode");
      this.term.options.disableStdin = false;
      if (this.isLive()) this.term.focus();
    }
    this.updateShellLockUi();
  }

  toggleInputMode() {
    const prev = this.inputMode;
    this.inputMode = this.inputMode === "shell" ? "raw" : "shell";
    opsLog("UI", `input_mode_toggle mode=${this.inputMode}`, {
      sid: this.sessionId.slice(0, 8),
      from: prev,
      alt_screen: this.altScreenActive,
      alt_seq: this.altScreenLastSeq,
    });
    this.applyInputMode();
    if (this.inputMode === "shell") {
      this.draftInput.focus();
      showToast(
        this.altScreenActive
          ? "已切回 Shell 模式 · 全屏程序仍在运行，草稿发送保持锁定"
          : "已切换到 Shell 模式 · 在底部草稿框输入命令",
        2800,
        { tone: this.altScreenActive ? "warn" : "info" },
      );
    } else {
      if (this.isLive()) this.term.focus();
      showToast(
        this.altScreenActive
          ? "已切换到 TUI 直通 · 全屏程序中，按键直达远端（vim: Esc 后 :q! 退出）"
          : "已切换到 TUI 直通 · 按键直接发给远端（适合 vim / htop）",
        3200,
        { tone: this.altScreenActive ? "warn" : "ok", position: "bottom" },
      );
    }
  }

  /**
   * Sync draft bar / banner / send button when live, alt-screen, or editor mode changes.
   */
  updateShellLockUi() {
    const locked = this.altScreenActive;
    const live = this.isLive();
    const edit = this.editorSubMode;
    this.btnSend.disabled = !live || locked;
    this.btnSend.title = locked
      ? "全屏程序运行中，Shell 发送已锁定。请切换到 TUI 直通操作。"
      : live
        ? "发送草稿命令到远端 shell"
        : "未连接，无法发送";
    this.btnSend.textContent = locked ? "已锁定" : "发送";

    this.draftWrap.classList.toggle("alt-screen-lock", locked);
    this.draftWrap.classList.toggle(
      "editor-insert",
      locked && (edit === "insert" || edit === "replace"),
    );
    this.btnMode.classList.toggle(
      "need-raw",
      locked && this.inputMode === "shell",
    );

    if (locked) {
      this.draftInput.placeholder =
        edit === "insert" || edit === "replace"
          ? "编辑中 · Shell 发送已锁定 · Esc 回普通模式 · :q! 退出"
          : "全屏程序运行中 · Shell 发送已锁定 · 请切换「TUI 直通」操作";
      this.tuiBanner.classList.remove("hidden");
      if (edit === "insert") {
        this.tuiBanner.innerHTML =
          "<b>编辑模式 INSERT</b> · 可直接输入文字 · " +
          "按 <code>Esc</code> 回普通模式 · 再 <code>:wq</code> / <code>:q!</code> 退出（勿用 Ctrl+C）";
      } else if (edit === "replace") {
        this.tuiBanner.innerHTML =
          "<b>替换模式 REPLACE</b> · 输入将覆盖原字符 · " +
          "再按 <code>Ins</code> 可回插入 · <code>Esc</code> 回普通模式";
      } else if (this.inputMode === "raw") {
        this.tuiBanner.innerHTML =
          "全屏程序运行中 · 当前 <b>TUI 直通</b>（普通模式）· " +
          "按 <code>Ins</code> / <code>i</code> 进入编辑 · " +
          "<code>Esc</code> 后 <code>:q!</code> / <code>:wq</code> 退出";
      } else {
        this.tuiBanner.innerHTML =
          "全屏程序运行中 · <b>Shell 发送已锁定</b> · " +
          "请切换到 <b>TUI 直通</b> 后按 <code>Ins</code>/<code>i</code> 编辑";
      }
    } else {
      this.draftInput.placeholder =
        "命令在此输入 · ↑↓ 历史 · Tab 补全 · Enter 发送（断线保留）";
      this.tuiBanner.classList.add("hidden");
      this.tuiBanner.textContent = "";
    }

    if (activeSessionId === this.sessionId) {
      syncGlobalStatusBar();
    }
  }

  /**
   * Update vim insert/replace/normal tip (bottom banner + status badge).
   */
  setEditorSubMode(mode: EditorSubMode, reason: string) {
    if (!this.altScreenActive && mode !== "normal") {
      // Ignore insert markers that arrive after alt-screen already closed.
      return;
    }
    if (this.editorSubMode === mode) return;
    const prev = this.editorSubMode;
    this.editorSubMode = mode;
    opsLog("STATE", "editor_submode", {
      sid: this.sessionId.slice(0, 8),
      from: prev,
      to: mode,
      reason,
      input_mode: this.inputMode,
      alt_screen: this.altScreenActive,
    });
    this.updateShellLockUi();
    // Light toast only when entering edit modes (bottom); no top strip.
    if (mode === "insert") {
      showToast("已进入编辑模式 INSERT · 按 Esc 回到普通模式", 2800, {
        tone: "ok",
        position: "bottom",
      });
    } else if (mode === "replace") {
      showToast("已进入替换模式 REPLACE · 再按 Ins 或 Esc 可退出", 2800, {
        tone: "warn",
        position: "bottom",
      });
    }
  }

  /** Observe local keys that affect vim editor mode (Ins / Esc). */
  private noteLocalEditorKey(data: string) {
    if (!this.altScreenActive) return;
    // Insert key (xterm: CSI 2 ~)
    if (data === "\x1b[2~") {
      opsLog("UI", "editor_key_ins", {
        sid: this.sessionId.slice(0, 8),
        current: this.editorSubMode,
      });
      // Optimistic: Ins from normal → insert; from insert → replace; from replace → insert.
      // Stream status line will correct if wrong.
      if (this.editorSubMode === "normal") {
        this.setEditorSubMode("insert", "local_ins");
      } else if (this.editorSubMode === "insert") {
        this.setEditorSubMode("replace", "local_ins_toggle");
      } else {
        this.setEditorSubMode("insert", "local_ins_toggle");
      }
      return;
    }
    // Bare Esc → normal mode (common leave-insert path)
    if (data === "\x1b") {
      if (this.editorSubMode !== "normal") {
        this.setEditorSubMode("normal", "local_esc");
      }
    }
  }

  /** Parse remote status line for -- INSERT -- / REPLACE. */
  private feedEditorModeProbe(text: string) {
    if (!this.altScreenActive) return;
    const mode = scanEditorSubMode(text);
    if (mode) {
      this.setEditorSubMode(mode, "stream_status");
    }
  }

  /**
   * Enter/leave alternate screen from DEC private modes in the remote stream.
   * @param active target state
   * @param reason log-friendly cause
   * @param detail optional CSI / mode numbers
   */
  setAltScreenActive(
    active: boolean,
    reason: string,
    detail?: { seq?: string | null; modes?: number[] },
  ) {
    if (this.altScreenActive === active) return;
    this.altScreenActive = active;
    if (detail?.seq) this.altScreenLastSeq = detail.seq;
    if (!active) {
      this.editorSubMode = "normal";
      if (reason === "session_state_reset") {
        this.altScreenLastSeq = null;
      }
    }

    opsLog("STATE", active ? "alt_screen_enter" : "alt_screen_exit", {
      sid: this.sessionId.slice(0, 8),
      reason,
      seq: detail?.seq ?? this.altScreenLastSeq,
      modes: detail?.modes ?? null,
      input_mode: this.inputMode,
      live: this.isLive(),
      draft_len: this.draft.text.length,
      pending_draft: this.pendingDraft != null,
      editor_submode: this.editorSubMode,
    });

    // Refresh mode titles + lock banner / send button / status badge.
    this.applyInputMode();

    if (active) {
      if (this.inputMode === "shell") {
        showToast(
          "检测到全屏程序：Shell 发送已锁定，请切换到「TUI 直通」",
          4000,
          { tone: "warn", position: "bottom" },
        );
      } else {
        showToast(
          "全屏程序运行中 · 按 Ins 或 i 进入编辑 · Esc 后 :q! 退出",
          3600,
          { tone: "warn", position: "bottom" },
        );
      }
      opsLog("UI", "alt_screen_ui_shown", {
        sid: this.sessionId.slice(0, 8),
        input_mode: this.inputMode,
        reason,
        seq: this.altScreenLastSeq,
      });
    } else if (reason !== "session_state_reset") {
      showToast("全屏程序已退出，Shell 草稿发送已恢复", 2400, {
        tone: "ok",
        position: "bottom",
      });
      opsLog("UI", "alt_screen_ui_cleared", {
        sid: this.sessionId.slice(0, 8),
        reason,
      });
      // If a reconnect-held draft was blocked while still on alt-screen, send now.
      if (this.isLive() && this.pendingDraft !== null) {
        const line = this.pendingDraft;
        this.pendingDraft = null;
        opsLog("CMD", "pending_draft_flush_after_alt_exit", {
          line: previewText(line),
          sid: this.sessionId.slice(0, 8),
        });
        void this.flushPendingLine(line);
      }
    }
  }

  /** Clear alt-screen lock on disconnect/reconnect (exit CSI may be muted). */
  resetAltScreenTracking(reason: string) {
    this.altScreenResidual = "";
    this.editorSubMode = "normal";
    if (this.altScreenActive) {
      this.setAltScreenActive(false, reason);
    } else {
      this.altScreenLastSeq = null;
    }
  }

  /** Parse remote stream for alt-screen private modes (chunk-safe). */
  private feedAltScreenProbe(text: string) {
    const scanned = scanAltScreenChunk(
      this.altScreenResidual,
      text,
      this.altScreenActive,
    );
    this.altScreenResidual = scanned.residual;
    if (scanned.nextActive !== this.altScreenActive) {
      this.setAltScreenActive(scanned.nextActive, "csi_private_mode", {
        seq: scanned.lastSeq,
        modes: scanned.lastModes,
      });
    } else if (scanned.lastSeq && scanned.nextActive) {
      // Still on alt-screen but saw another related CSI (refresh last seq for logs).
      this.altScreenLastSeq = scanned.lastSeq;
    }
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

    // Disconnect / reconnect starts a new shell; exit CSI is often muted, so
    // clear alt-screen lock proactively.
    if (
      state === "connecting" ||
      state === "reconnecting" ||
      state === "disconnected" ||
      state === "failed" ||
      state === "idle"
    ) {
      this.resetAltScreenTracking("session_state_reset");
    }

    this.updateShellLockUi();

    if (this.isLive() && this.pendingDraft !== null) {
      if (this.altScreenActive) {
        opsLog("CMD", "pending_draft_held_alt_screen", {
          line: previewText(this.pendingDraft),
          sid: this.sessionId.slice(0, 8),
        });
      } else {
        const line = this.pendingDraft;
        this.pendingDraft = null;
        void this.flushPendingLine(line);
      }
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

  /** Pending terminal text coalesced across rAF (avoids main-thread storms). */
  private termWriteBuf = "";
  private termWriteScheduled = false;

  writeToTerm(data: string | Uint8Array) {
    const text =
      typeof data === "string"
        ? data
        : new TextDecoder("utf-8", { fatal: false }).decode(data);
    this.feedAltScreenProbe(text);
    this.feedEditorModeProbe(text);
    // Coalesce into one xterm write per animation frame so a tail flood
    // cannot re-enter hundreds of term.write calls on the same turn.
    this.termWriteBuf += text;
    if (this.termWriteBuf.length > 256 * 1024) {
      // Hard flush if backlog is huge (avoid multi-MB string hold).
      this.flushTermWriteBuf();
      return;
    }
    if (!this.termWriteScheduled) {
      this.termWriteScheduled = true;
      requestAnimationFrame(() => {
        this.termWriteScheduled = false;
        this.flushTermWriteBuf();
      });
    }
  }

  private flushTermWriteBuf() {
    if (!this.termWriteBuf) return;
    // Chunk large dumps across frames so a single 100KB+ write cannot freeze
    // the UI thread for hundreds of ms.
    const CHUNK = 24 * 1024;
    let text: string;
    if (this.termWriteBuf.length <= CHUNK) {
      text = this.termWriteBuf;
      this.termWriteBuf = "";
    } else {
      text = this.termWriteBuf.slice(0, CHUNK);
      this.termWriteBuf = this.termWriteBuf.slice(CHUNK);
    }
    const t0 = performance.now();
    this.term.write(text);
    const writeMs = performance.now() - t0;
    if (writeMs >= 40) {
      opsLogDeferred("SSH", "ui_term_write_slow", {
        sid: this.sessionId.slice(0, 8),
        len: text.length,
        write_ms: Math.round(writeMs),
        remain: this.termWriteBuf.length,
      });
    }
    if (this.termWriteBuf.length > 0) {
      this.termWriteScheduled = true;
      requestAnimationFrame(() => {
        this.termWriteScheduled = false;
        this.flushTermWriteBuf();
      });
    }
  }

  private wireTerminal() {
    this.term.onData((data) => {
      if (this.inputMode === "shell" && !isControlKeyPayload(data)) {
        this.draftInput.focus();
        return;
      }
      if (!this.isLive()) return;
      // Track Ins / Esc for editor-mode tip while full-screen TUI is open.
      this.noteLocalEditorKey(data);
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
      alt_screen: this.altScreenActive,
      alt_seq: this.altScreenLastSeq,
      sid: this.sessionId.slice(0, 8),
    });
    if (!line.trim()) {
      this.setError("草稿为空，请输入命令后再发送");
      return;
    }
    if (this.altScreenActive) {
      opsLog("CMD", "draft_send_blocked_alt_screen", {
        line: previewText(line),
        mode: this.inputMode,
        seq: this.altScreenLastSeq,
        sid: this.sessionId.slice(0, 8),
      });
      this.setError(
        "全屏程序运行中，Shell 发送已锁定。请切换到「TUI 直通」操作；退出程序（如 vim 的 Esc → :q!）后自动解锁。",
      );
      showToast("已拦截发送：当前处于全屏 TUI，请切换到 TUI 直通", 3200);
      if (this.inputMode === "shell") {
        this.btnMode.classList.add("need-raw");
      }
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
    if (this.altScreenActive) {
      opsLog("CMD", "flush_blocked_alt_screen", {
        line: previewText(logical),
        mode: this.inputMode,
        seq: this.altScreenLastSeq,
        sid: this.sessionId.slice(0, 8),
      });
      this.setError(
        "全屏程序运行中，已阻止草稿发送，避免写入 vim 等程序。请切换 TUI 直通。",
      );
      return;
    }
    const postSeparator = isCmdSeparatorEnabled();
    opsLog("CMD", "ui_submit_line", {
      line: previewText(logical),
      sid: this.sessionId.slice(0, 8),
      alt_screen: false,
      post_separator: postSeparator,
    });
    if (postSeparator) {
      const now = Date.now();
      this.sepUi = {
        active: true,
        startedAt: now,
        chunks: 0,
        bytes: 0,
        lastAt: now,
        lastLogAt: now,
      };
      opsLog("SEP", "ui_sep_begin", {
        sid: this.sessionId.slice(0, 8),
        line: previewText(logical, 120),
      });
    } else {
      this.sepUi = null;
    }
    // Record history before clear so ↑ works even if IPC fails later.
    // History stores the original command only (no separator suffix).
    this.pushHistory(logical);
    this.draft.text = "";
    this.draft.cursor = 0;
    this.restoreDraftToInput();
    this.setError(null);
    try {
      const t0 = performance.now();
      await invoke("submit_line", {
        sessionId: this.sessionId,
        line: logical,
        postSeparator,
      });
      const invokeMs = Math.round(performance.now() - t0);
      opsLog("CMD", "ui_submit_line_ok", {
        line: previewText(logical),
        post_separator: postSeparator,
        invoke_ms: invokeMs,
      });
      if (postSeparator) {
        opsLog("SEP", "ui_submit_invoke_ok", {
          sid: this.sessionId.slice(0, 8),
          invoke_ms: invokeMs,
        });
      }
      this.draftInput.focus();
    } catch (e) {
      this.draft.text = logical;
      this.draft.cursor = logical.length;
      this.restoreDraftToInput();
      this.sepUi = null;
      opsLog("ERR", "ui_submit_line_failed", {
        line: previewText(logical),
        error: String(e),
      });
      this.setError(String(e));
    }
  }

  /** Track UI-side receive stats after a post-separator submit. */
  noteSepUiData(len: number) {
    const s = this.sepUi;
    if (!s?.active) return;
    const now = Date.now();
    s.chunks += 1;
    s.bytes += len;
    s.lastAt = now;
    // First 5 chunks, then every 500ms, always log slow gaps.
    const gap = now - s.lastLogAt;
    if (s.chunks <= 5 || gap >= 500) {
      s.lastLogAt = now;
      // Deferred: called from session://data handler (must not re-enter IPC).
      opsLogDeferred("SEP", "ui_receive_progress", {
        sid: this.sessionId.slice(0, 8),
        chunk_len: len,
        chunks: s.chunks,
        bytes: s.bytes,
        elapsed_ms: now - s.startedAt,
        since_last_log_ms: gap,
      });
    }
  }

  async dispose() {
    if (this.disposed) return;
    this.disposed = true;
    try {
      this.bufferChangeDisposable?.dispose();
    } catch {
      /* ignore */
    }
    this.bufferChangeDisposable = null;
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
  const modeBadge = $("mode-badge");
  if (!view) {
    dot.className = "dot idle";
    text.textContent = "未连接";
    text.className = "";
    cwdEl.textContent = "";
    hostEl.textContent = "";
    if (modeBadge) {
      modeBadge.textContent = "—";
      modeBadge.className = "mode-badge";
      modeBadge.title = "无会话";
    }
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

  // Connection status text + editor/fullscreen tips.
  let status = view.message || labels[view.state] || view.state;
  text.className = "";
  if (view.altScreenActive && view.state === "connected") {
    if (view.editorSubMode === "insert") {
      status = "编辑模式 INSERT · Esc 回普通模式";
      text.className = "status-text-warn";
    } else if (view.editorSubMode === "replace") {
      status = "替换模式 REPLACE · Esc / Ins 可退出";
      text.className = "status-text-warn";
    } else {
      status =
        view.inputMode === "raw"
          ? "全屏 TUI · 普通模式 · Ins/i 进入编辑"
          : "全屏 TUI · Shell 发送已锁定";
      text.className = "status-text-warn";
    }
    dot.className = "dot alt-screen";
  } else if (view.state === "connected" && view.inputMode === "raw") {
    status = view.message
      ? `${view.message} · TUI 直通`
      : "已连接 · TUI 直通";
    dot.className = `dot ${view.state}`;
  } else {
    dot.className = `dot ${view.state}`;
  }
  text.textContent = status;
  text.title = view.altScreenActive
    ? `备用屏 · 编辑子模式=${view.editorSubMode}${view.altScreenLastSeq ? ` · ${previewText(view.altScreenLastSeq, 40)}` : ""}`
    : view.message || labels[view.state] || "";

  // Mode badge: Shell / TUI / 全屏 / 插入 / 替换
  if (modeBadge) {
    if (view.altScreenActive && view.editorSubMode === "insert") {
      modeBadge.textContent = "INSERT";
      modeBadge.className = "mode-badge mode-badge-insert";
      modeBadge.title = "vim 编辑模式（INSERT）· 按 Esc 回普通模式";
    } else if (view.altScreenActive && view.editorSubMode === "replace") {
      modeBadge.textContent = "REPLACE";
      modeBadge.className = "mode-badge mode-badge-replace";
      modeBadge.title = "vim 替换模式（REPLACE）";
    } else if (view.altScreenActive) {
      modeBadge.textContent =
        view.inputMode === "raw" ? "TUI·全屏" : "Shell·锁定";
      modeBadge.className = "mode-badge mode-badge-alt";
      modeBadge.title =
        view.inputMode === "raw"
          ? "TUI 直通 · 全屏程序 · 普通模式（可按 Ins/i 编辑）"
          : "Shell · 全屏程序中，草稿发送已锁定";
    } else if (view.inputMode === "raw") {
      modeBadge.textContent = "TUI 直通";
      modeBadge.className = "mode-badge mode-badge-raw";
      modeBadge.title = "输入模式：TUI 直通 · 按键直达远端";
    } else {
      modeBadge.textContent = "Shell";
      modeBadge.className = "mode-badge mode-badge-shell";
      modeBadge.title = "输入模式：Shell · 底部草稿发送命令";
    }
  }

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

  // Saving secrets requires a profile_id (keyring is keyed by profile).
  // If user checked save but has no profile yet, persist one first.
  const wantSavePw =
    authType === "password" &&
    ($("save-password") as HTMLInputElement).checked;
  const wantSavePp =
    authType === "public_key" &&
    !!($("save-passphrase") as HTMLInputElement | null)?.checked;
  if ((wantSavePw || wantSavePp) && !selectedProfileId) {
    const p = await saveProfileFromForm(null);
    if (!p) {
      setPropsError("保存凭据需要先创建会话配置，请检查主机/用户名后重试");
      return false;
    }
    selectedProfileId = p.id;
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

type LaunchXftpResult = {
  executable: string;
  xfp_path: string;
  host: string;
  port: number;
  username: string;
  remote: string | null;
  auth_mode: string;
  user_key_name?: string | null;
  user_hint: string;
};

/** Open external Xftp for a session (host/user/port/cwd; pubkey uses clear temp key). */
async function openXftpForSession(
  sessionId: string,
  opts?: { silent?: boolean; reason?: string },
): Promise<boolean> {
  const view = sessions.get(sessionId);
  if (!view) {
    if (!opts?.silent) showToast("会话不存在");
    return false;
  }
  if (!view.isLive()) {
    if (!opts?.silent) showToast("请先连接会话后再打开 Xftp");
    return false;
  }
  opsLog("UI", "open_xftp", {
    sid: view.sessionId.slice(0, 8),
    host: view.host || undefined,
    reason: opts?.reason || "manual",
  });
  try {
    const r = await invoke<LaunchXftpResult>("launch_xftp", {
      sessionId: view.sessionId,
    });
    const remoteHint = r.remote ? ` · ${r.remote}` : "";
    const hint = r.user_hint ? ` — ${r.user_hint}` : "";
    showToast(`已打开 Xftp：${r.username}@${r.host}${remoteHint}${hint}`);
    opsLog("UI", "open_xftp ok", {
      host: r.host,
      port: r.port,
      user: r.username,
      remote: r.remote,
      auth_mode: r.auth_mode,
      user_key_name: r.user_key_name ?? null,
      exe: r.executable,
      reason: opts?.reason || "manual",
    });
    return true;
  } catch (e) {
    const msg = String(e);
    opsLog("ERR", "open_xftp failed", {
      error: msg,
      reason: opts?.reason || "manual",
    });
    if (!opts?.silent) {
      showToast(msg.replace(/^[^:]+:\s*/, "") || msg);
    } else {
      showToast(`自动打开 Xftp 失败：${msg.replace(/^[^:]+:\s*/, "") || msg}`);
    }
    return false;
  }
}

async function openXftpForActive() {
  const view = getActive();
  if (!view) {
    showToast("请先打开并连接一个会话");
    return;
  }
  await openXftpForSession(view.sessionId, { reason: "manual" });
}

/**
 * After first Connected of a tab: optional auto-launch (once).
 * Slight delay so cwd seed / restore can fill Remote= path.
 */
function maybeAutoLaunchXftp(view: SessionView) {
  if (!isXftpAutoLaunchEnabled()) return;
  if (view.xftpAutoLaunched) return;
  if (!view.isLive()) return;
  view.xftpAutoLaunched = true;
  window.setTimeout(() => {
    if (!sessions.has(view.sessionId) || !view.isLive()) return;
    void openXftpForSession(view.sessionId, {
      reason: "auto_on_connect",
      silent: true,
    });
  }, 700);
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
    const id = selectedProfileId;
    const p = await saveProfileFromForm(id);
    if (p) {
      showToast("配置已保存");
      // edit-profile (and save-only create from manager): close props → manager
      if (propsMode === "edit-profile" || propsReturnToManager) {
        closePropsDialog();
      }
      // If still on create form without return-to-manager, keep dialog open.
      void loadProfiles().then(() => renderManagerList());
    }
    return;
  }

  if (action === "save-connect") {
    // Connecting: do not bounce back to manager.
    propsReturnToManager = false;
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
    // Connecting from create: leave manager closed.
    propsReturnToManager = false;
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

function openPropsDialog(
  mode: PropsMode,
  opts?: { sessionId?: string; returnToManager?: boolean },
) {
  propsMode = mode;
  propsTargetSessionId = opts?.sessionId ?? null;
  propsReturnToManager = !!opts?.returnToManager;
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
  const backToMgr = propsReturnToManager;
  propsReturnToManager = false;
  if (backToMgr) {
    // Defer so the props <dialog> fully closes before re-opening manager.
    void Promise.resolve().then(() => openManager());
  }
}

// ---------------------------------------------------------------------------
// Session manager
// ---------------------------------------------------------------------------

function pruneManagerChecks() {
  const ids = new Set(getProfiles().map((p) => p.id));
  for (const id of [...mgrCheckedIds]) {
    if (!ids.has(id)) mgrCheckedIds.delete(id);
  }
  if (mgrCheckAnchorId && !ids.has(mgrCheckAnchorId)) mgrCheckAnchorId = null;
}

function managerSearchQuery(): string {
  const el = document.getElementById("mgr-search") as HTMLInputElement | null;
  return (el?.value ?? "").trim().toLowerCase();
}

function profileMatchesSearch(p: HostProfile, q: string): boolean {
  if (!q) return true;
  const name = p.name.toLowerCase();
  const host = p.host.toLowerCase();
  return q.split(/\s+/).every((t) => name.includes(t) || host.includes(t));
}

function visibleManagerProfiles(): HostProfile[] {
  const q = managerSearchQuery();
  const all = getProfiles();
  if (!q) return all;
  return all.filter((p) => profileMatchesSearch(p, q));
}

function checkedManagerProfiles(): HostProfile[] {
  pruneManagerChecks();
  return getProfiles().filter((p) => mgrCheckedIds.has(p.id));
}

function syncManagerCheckUi() {
  pruneManagerChecks();
  const all = getProfiles();
  const visible = visibleManagerProfiles();
  const n = mgrCheckedIds.size;
  const visChecked = visible.filter((p) => mgrCheckedIds.has(p.id)).length;
  const allBox = $("mgr-check-all") as HTMLInputElement | null;
  if (allBox) {
    allBox.disabled = visible.length === 0;
    allBox.checked = visible.length > 0 && visChecked === visible.length;
    allBox.indeterminate = visChecked > 0 && visChecked < visible.length;
  }
  const count = $("mgr-check-count");
  if (count) {
    const parts: string[] = [];
    if (managerSearchQuery()) {
      parts.push(`显示 ${visible.length} / ${all.length}`);
    }
    if (n > 0) parts.push(`已勾选 ${n} 条`);
    count.textContent = parts.join(" · ");
  }
  const openBtn = $("mgr-btn-open") as HTMLButtonElement | null;
  if (openBtn) openBtn.textContent = n > 1 ? `打开 (${n})` : "打开";
  const exportBtn = $("mgr-btn-export") as HTMLButtonElement | null;
  if (exportBtn) exportBtn.textContent = n > 1 ? `导出 (${n})…` : "导出…";
  const deleteBtn = $("mgr-btn-delete") as HTMLButtonElement | null;
  if (deleteBtn) deleteBtn.textContent = n > 1 ? `删除 (${n})` : "删除";
}

function applyManagerCheckRange(toId: string) {
  const ids = visibleManagerProfiles().map((p) => p.id);
  const a = mgrCheckAnchorId ? ids.indexOf(mgrCheckAnchorId) : -1;
  const b = ids.indexOf(toId);
  if (a < 0 || b < 0) {
    mgrCheckedIds.add(toId);
    mgrCheckAnchorId = toId;
    return;
  }
  const lo = Math.min(a, b);
  const hi = Math.max(a, b);
  for (let i = lo; i <= hi; i++) mgrCheckedIds.add(ids[i]);
}

function setManagerSelected(id: string | null) {
  mgrSelectedId = id;
  $("mgr-profile-list")
    .querySelectorAll("li")
    .forEach((el) => {
      el.classList.toggle(
        "selected",
        !!id && (el as HTMLElement).dataset.id === id,
      );
    });
}

function paintManagerChecks() {
  $("mgr-profile-list")
    .querySelectorAll("li")
    .forEach((el) => {
      const id = (el as HTMLElement).dataset.id;
      const cb = el.querySelector(".mgr-item-check") as HTMLInputElement | null;
      if (id && cb) cb.checked = mgrCheckedIds.has(id);
    });
  syncManagerCheckUi();
}

function renderManagerList() {
  const list = $("mgr-profile-list");
  list.innerHTML = "";
  pruneManagerChecks();
  const profiles = visibleManagerProfiles();
  if (profiles.length === 0) {
    const empty = document.createElement("li");
    empty.className = "mgr-empty";
    empty.textContent =
      getProfiles().length === 0 ? "暂无保存的会话" : "无匹配的会话";
    list.appendChild(empty);
    syncManagerCheckUi();
    return;
  }
  for (const p of profiles) {
    const li = document.createElement("li");
    li.dataset.id = p.id;
    li.setAttribute("role", "option");
    if (p.id === mgrSelectedId) li.classList.add("selected");

    const check = document.createElement("input");
    check.type = "checkbox";
    check.className = "mgr-item-check";
    check.checked = mgrCheckedIds.has(p.id);
    check.title = "勾选以批量打开 / 导出 / 删除";
    check.setAttribute("aria-label", `勾选 ${p.name}`);
    check.addEventListener("click", (e) => {
      e.stopPropagation();
      if (e.detail > 1) {
        e.preventDefault();
        return;
      }
      if (e.shiftKey && mgrCheckAnchorId) {
        e.preventDefault();
        applyManagerCheckRange(p.id);
        setManagerSelected(p.id);
        paintManagerChecks();
      }
    });
    check.addEventListener("change", () => {
      if (check.checked) mgrCheckedIds.add(p.id);
      else mgrCheckedIds.delete(p.id);
      mgrCheckAnchorId = p.id;
      setManagerSelected(p.id);
      syncManagerCheckUi();
    });
    check.addEventListener("dblclick", (e) => {
      e.stopPropagation();
      e.preventDefault();
    });

    const body = document.createElement("div");
    body.className = "mgr-item-body";
    body.innerHTML = `<div class="name">${escapeHtml(p.name)}</div>
      <div class="meta">${escapeHtml(p.username)}@${escapeHtml(p.host)}:${p.port} · ${
        p.auth_type === "password" ? "密码" : "私钥"
      }</div>`;

    li.append(check, body);
    li.addEventListener("click", () => {
      setManagerSelected(p.id);
    });
    li.addEventListener("dblclick", () => {
      setManagerSelected(p.id);
      void openProfileAsNewTab(p);
    });
    list.appendChild(li);
  }
  syncManagerCheckUi();
}

async function openManager() {
  await loadProfiles();
  const dlg = $("dlg-session-manager") as HTMLDialogElement;
  const wasOpen = dlg.open;
  const list = $("mgr-profile-list");
  const scrollTop = wasOpen ? list.scrollTop : 0;
  renderManagerList();
  if (wasOpen) {
    list.scrollTop = scrollTop;
  } else {
    dlg.showModal();
    const search = document.getElementById("mgr-search") as HTMLInputElement | null;
    search?.focus();
  }
}

interface ProfilesExportResult {
  json: string;
  profileCount: number;
  secretsCount: number;
  includeSecrets: boolean;
  savedPath?: string | null;
  cancelled?: boolean;
}

interface ProfilesImportResult {
  imported: number;
  updated: number;
  created: number;
  secretsRestored: number;
  skipped: number;
  mode: string;
  warnings: string[];
}

async function exportProfilesFromManager() {
  const profiles = getProfiles();
  if (profiles.length === 0) {
    showToast("没有可导出的会话配置");
    return;
  }

  // Scope: checked rows → those; else highlighted row (confirm vs all); else all.
  let profileIds: string[] | null = null;
  const checked = checkedManagerProfiles();
  if (checked.length > 0) {
    profileIds = checked.map((p) => p.id);
  } else if (mgrSelectedId) {
    const sel = profiles.find((p) => p.id === mgrSelectedId);
    const label = sel
      ? `${sel.name}（${sel.username}@${sel.host}）`
      : mgrSelectedId.slice(0, 8);
    const onlySelected = window.confirm(
      `导出范围：\n\n` +
        `· 选「确定」：仅导出当前选中的\n  ${label}\n\n` +
        `· 选「取消」：导出全部 ${profiles.length} 条配置`,
    );
    if (onlySelected) {
      profileIds = [mgrSelectedId];
    }
  } else {
    const ok = window.confirm(
      `未勾选会话，将导出全部 ${profiles.length} 条。\n\n` +
        `若只导出部分，请先勾选再点「导出…」。\n\n继续导出全部？`,
    );
    if (!ok) return;
  }

  let includeSecrets = window.confirm(
    "是否在导出文件中包含已保存的登录密码 / 私钥口令？\n\n" +
      "· 选「确定」：便于无缝迁移（文件含敏感信息，请妥善保管）\n" +
      "· 选「取消」：仅导出主机/用户/路径等配置，不含密码",
  );
  if (includeSecrets) {
    includeSecrets = window.confirm(
      "二次确认：导出文件将包含【明文密码/口令】。\n\n" +
        "请勿上传网盘、邮件或提交到 Git。用完后请删除该文件。\n\n继续导出？",
    );
  }
  try {
    // Native Save As dialog (default folder: Downloads); returns absolute path.
    const r = await invoke<ProfilesExportResult>("export_profiles", {
      req: {
        includeSecrets,
        profileIds: profileIds,
      },
    });
    if (r.cancelled) {
      showToast("已取消导出");
      return;
    }
    opsLog("CFG", "export_profiles_ui", {
      count: r.profileCount,
      secrets: r.secretsCount,
      include_secrets: r.includeSecrets,
      path: r.savedPath ?? null,
      selected_only: !!profileIds,
    });
    const pathHint = r.savedPath ? `\n${r.savedPath}` : "";
    showToast(
      `已导出 ${r.profileCount} 条配置` +
        (r.includeSecrets
          ? `（含 ${r.secretsCount} 条明文凭据，用后请删除文件）`
          : "（不含密码）") +
        pathHint,
      6000,
    );
  } catch (e) {
    showToast(`导出失败: ${e}`);
    opsLog("ERR", "export_profiles_ui_failed", { error: String(e) });
  }
}

async function importProfilesFromManager(file: File) {
  let text: string;
  try {
    text = await file.text();
  } catch (e) {
    showToast(`无法读取文件: ${e}`);
    return;
  }

  const replace = window.confirm(
    "导入模式：\n\n" +
      "· 选「确定」：用导入内容【完全替换】当前所有会话配置（危险）\n" +
      "· 选「取消」：【合并】导入（同 id 更新，新 id 新增）",
  );
  const generateNewIds =
    !replace &&
    window.confirm(
      "合并时是否为导入项【全部生成新 ID】？\n\n" +
        "· 确定：作为副本导入，不覆盖本机同 id 配置\n" +
        "· 取消：保留文件中的 id，同 id 会覆盖本机配置",
    );

  if (
    replace &&
    !window.confirm("确认【替换】全部会话配置？此操作会清空当前列表（含已存凭据）后导入。")
  ) {
    return;
  }

  try {
    const r = await invoke<ProfilesImportResult>("import_profiles", {
      req: {
        json: text,
        mode: replace ? "replace" : "merge",
        generateNewIds,
      },
    });
    await loadProfiles();
    renderManagerList();
    const warnN = r.warnings?.length ?? 0;
    opsLog("CFG", "import_profiles_ui", {
      mode: r.mode,
      created: r.created,
      updated: r.updated,
      secrets: r.secretsRestored,
      skipped: r.skipped,
      warnings: warnN,
    });
    let msg = `导入完成：新增 ${r.created}，更新 ${r.updated}`;
    if (r.secretsRestored > 0) msg += `，恢复凭据 ${r.secretsRestored}`;
    if (r.skipped > 0) msg += `，跳过 ${r.skipped}`;
    showToast(msg, 4000);
    if (warnN > 0) {
      const sample = r.warnings.slice(0, 5).join("\n");
      window.alert(
        `导入注意（${warnN} 条）：\n\n${sample}` +
          (warnN > 5 ? `\n…另有 ${warnN - 5} 条` : ""),
      );
    }
  } catch (e) {
    showToast(`导入失败: ${e}`);
    opsLog("ERR", "import_profiles_ui_failed", { error: String(e) });
  }
}

type OpenProfileResult = "ok" | "cancel" | "limit" | "skip" | "fail";

async function openProfileAsNewTab(
  p: HostProfile,
  opts?: { closeManager?: boolean; quiet?: boolean },
): Promise<OpenProfileResult> {
  fillFormFromProfile(p);
  if (opts?.closeManager !== false) {
    ($("dlg-session-manager") as HTMLDialogElement).close();
  }
  selectedProfileId = p.id;
  if (sessions.size >= MAX_TABS) {
    if (!opts?.quiet) showToast(`最多打开 ${MAX_TABS} 个会话`);
    return "limit";
  }

  // Secrets never live in profiles.json. Keyring (optional) or prompt.
  let auth: Record<string, unknown>;
  if (p.auth_type === "password") {
    const prompted = await ensurePassword(null, !!p.has_saved_password);
    if (prompted === undefined) {
      if (!opts?.quiet) showToast("已取消连接");
      return "cancel";
    }
    auth = {
      type: "password",
      // null/empty → backend loads from Windows Credential Manager via profile_id
      password: prompted || null,
      // keep flag so successful connect can refresh keyring if user re-saves later
      save_password: !!p.has_saved_password,
    };
  } else {
    if (!p.private_key_path) {
      if (!opts?.quiet) showToast("配置缺少私钥路径");
      return "skip";
    }
    const prompted = await ensurePassphrase(null, !!p.has_saved_passphrase);
    if (prompted === undefined) {
      if (!opts?.quiet) showToast("已取消连接");
      return "cancel";
    }
    auth = {
      type: "public_key",
      private_key_path: p.private_key_path,
      passphrase: prompted || null,
      save_passphrase: !!p.has_saved_passphrase,
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
    if (view.state === "failed") {
      if (!opts?.quiet) showToast(view.message || "连接失败");
      return "fail";
    }
    return "ok";
  } catch (e) {
    view.setError(String(e));
    view.applyState("failed", String(e));
    if (!opts?.quiet) showToast(String(e));
    return "fail";
  }
}

function managerOpenTargets(): HostProfile[] {
  const checked = checkedManagerProfiles();
  if (checked.length > 0) return checked;
  if (mgrSelectedId) {
    const p = getProfiles().find((x) => x.id === mgrSelectedId);
    if (p) return [p];
  }
  return [];
}

async function openManagerProfiles() {
  const targets = managerOpenTargets();
  if (targets.length === 0) {
    showToast("请先勾选或选择要打开的会话");
    return;
  }
  if (targets.length > 1) {
    const room = Math.max(0, MAX_TABS - sessions.size);
    if (room <= 0) {
      showToast(`最多打开 ${MAX_TABS} 个会话`);
      return;
    }
    let msg = `将打开已勾选的 ${targets.length} 个会话（每个新建标签）。继续？`;
    if (targets.length > room) {
      msg =
        `当前还可打开 ${room} 个标签，已勾选 ${targets.length} 个。\n\n` +
        `将只打开前 ${room} 个。继续？`;
    }
    if (!window.confirm(msg)) return;
  }

  ($("dlg-session-manager") as HTMLDialogElement).close();

  let opened = 0;
  let failed = 0;
  let cancelled = 0;
  let skipped = 0;
  let limited = 0;
  const batch = targets.length > 1;
  for (let i = 0; i < targets.length; i++) {
    if (sessions.size >= MAX_TABS) {
      limited = targets.length - i;
      break;
    }
    const r = await openProfileAsNewTab(targets[i], {
      closeManager: false,
      quiet: batch,
    });
    if (r === "ok") opened++;
    else if (r === "fail") failed++;
    else if (r === "cancel") cancelled++;
    else if (r === "skip") skipped++;
    else if (r === "limit") {
      limited = targets.length - i;
      break;
    }
  }
  if (batch) {
    const parts = [`已打开 ${opened} 个`];
    if (failed) parts.push(`失败 ${failed} 个`);
    if (cancelled) parts.push(`取消 ${cancelled} 个`);
    if (skipped) parts.push(`跳过 ${skipped} 个`);
    if (limited) parts.push(`标签已满未打开 ${limited} 个`);
    showToast(parts.join("，"), 4500);
  }
}

async function deleteManagerProfiles() {
  const targets = managerOpenTargets();
  if (targets.length === 0) {
    showToast("请先勾选或选择要删除的会话");
    return;
  }
  const n = targets.length;
  const names = targets
    .slice(0, 8)
    .map((p) => `· ${p.name}（${p.username}@${p.host}）`)
    .join("\n");
  const extra = n > 8 ? `\n…另有 ${n - 8} 条` : "";
  const ok = window.confirm(
    (n > 1
      ? `将删除已勾选的 ${n} 条主机配置（不会关闭已打开的标签）。\n\n${names}${extra}\n\n`
      : `删除该主机配置？（不会关闭已打开的标签）\n\n${names}\n\n`) + "继续？",
  );
  if (!ok) return;

  let deleted = 0;
  const errors: string[] = [];
  for (const p of targets) {
    try {
      await invoke("delete_profile", { id: p.id });
      mgrCheckedIds.delete(p.id);
      if (mgrSelectedId === p.id) mgrSelectedId = null;
      deleted++;
    } catch (e) {
      errors.push(`${p.name}: ${e}`);
    }
  }
  await loadProfiles();
  renderManagerList();
  if (errors.length === 0) {
    showToast(n > 1 ? `已删除 ${deleted} 条配置` : "已删除配置");
  } else {
    showToast(`已删除 ${deleted} 条，失败 ${errors.length} 条`);
    window.alert(`删除失败：\n\n${errors.slice(0, 8).join("\n")}`);
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
      if (dlg?.id === "dlg-session-props") {
        if (propsConnecting) return;
        // Use closePropsDialog so return-to-manager works after edit/new.
        closePropsDialog();
        return;
      }
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
    case "toggle-cmd-separator":
      setCmdSeparatorEnabled(!isCmdSeparatorEnabled());
      showToast(
        isCmdSeparatorEnabled()
          ? "已启用：命令后显示绿色分割线"
          : "已关闭：命令后分割线",
      );
      break;
    case "disconnect":
      await disconnectActive();
      break;
    case "open-xftp":
      await openXftpForActive();
      break;
    case "toggle-xftp-auto":
      setXftpAutoLaunchEnabled(!isXftpAutoLaunchEnabled());
      showToast(
        isXftpAutoLaunchEnabled()
          ? "已启用：连接成功后自动打开 Xftp（每标签一次）"
          : "已关闭：连接后自动打开 Xftp",
      );
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
    case "open-logs": {
      // Prefer native backend open (explorer/xdg-open) — plugin-opener's
      // `openPath` needs path scope and is not in opener:default.
      try {
        const dir = await invoke<string>("open_ops_log_dir");
        logsDir = dir;
        opsLog("UI", "open_logs ok", { dir, via: "open_ops_log_dir" });
        break;
      } catch (e0) {
        opsLog("ERR", "open_logs backend failed", { error: String(e0) });
      }
      try {
        if (!logsDir) {
          const info = await invoke<{ dir: string }>("ops_log_info");
          logsDir = info.dir || null;
        }
        if (!logsDir) {
          showToast("日志目录未知");
          break;
        }
        try {
          await openPath(logsDir);
          opsLog("UI", "open_logs ok", { dir: logsDir, via: "openPath" });
        } catch (e1) {
          opsLog("ERR", "open_logs openPath failed", { error: String(e1) });
          await revealItemInDir(logsDir);
          opsLog("UI", "open_logs ok", {
            dir: logsDir,
            via: "revealItemInDir",
          });
        }
      } catch (e) {
        opsLog("ERR", "open_logs failed", { error: String(e) });
        showToast(`无法打开日志目录: ${e}`);
      }
      break;
    }
    case "about":
      ($("dlg-about") as HTMLDialogElement).showModal();
      break;
    case "shell-integration":
      ($("dlg-shell-help") as HTMLDialogElement).showModal();
      break;
    case "open-mcp":
      await openMcpDialog();
      break;
  }
}

// ---------------------------------------------------------------------------
// MCP server dialog (PR-M1 skeleton)
// ---------------------------------------------------------------------------

interface McpStatus {
  enabled: boolean;
  running: boolean;
  bindHost: string;
  port: number;
  actualPort?: number | null;
  token: string;
  endpointUrl?: string | null;
  healthUrl?: string | null;
  clientConfigJson: string;
  stdioClientConfigJson?: string;
  exePath?: string | null;
  lastError?: string | null;
  allowPtyTools: boolean;
  phase: string;
}

let mcpTokenVisible = false;

function applyMcpStatusToForm(st: McpStatus) {
  const en = $("mcp-enabled") as HTMLInputElement;
  const port = $("mcp-port") as HTMLInputElement;
  const statusText = $("mcp-status-text") as HTMLInputElement;
  const health = $("mcp-health-url") as HTMLInputElement;
  const token = $("mcp-token") as HTMLInputElement;
  const cfg = $("mcp-client-config") as HTMLTextAreaElement;
  const stdioCfg = $("mcp-stdio-config") as HTMLTextAreaElement | null;
  const err = $("dlg-mcp-error");

  en.checked = st.enabled;
  port.value = String(st.port);
  const runLabel = st.running
    ? `运行中 :${st.actualPort ?? st.port}`
    : st.enabled
      ? "已启用但未监听"
      : "已停止";
  statusText.value = `${runLabel} · ${st.phase}`;
  health.value = st.healthUrl || "";
  token.value = st.token || "";
  token.type = mcpTokenVisible ? "text" : "password";
  cfg.value = st.clientConfigJson || "";
  if (stdioCfg) {
    stdioCfg.value = st.stdioClientConfigJson || "";
  }

  if (st.lastError) {
    err.hidden = false;
    err.textContent = st.lastError;
  } else {
    err.hidden = true;
    err.textContent = "";
  }
  updateMcpBadge(st);
}

function updateMcpBadge(st: McpStatus) {
  const badge = $("mcp-badge");
  if (!badge) return;
  if (st.running) {
    badge.hidden = false;
    badge.classList.remove("off");
    badge.classList.add("on");
    const p = st.actualPort ?? st.port;
    badge.textContent = `MCP :${p}`;
    badge.title = `MCP 运行中 http://127.0.0.1:${p}/ · 点击菜单「工具 → MCP 服务器」`;
  } else if (st.enabled) {
    badge.hidden = false;
    badge.classList.remove("on");
    badge.classList.add("off");
    badge.textContent = "MCP !";
    badge.title = st.lastError
      ? `MCP 启用失败: ${st.lastError}`
      : "MCP 已启用但未监听";
  } else {
    badge.hidden = true;
    badge.classList.remove("on");
    badge.classList.add("off");
    badge.textContent = "MCP";
  }
}

async function refreshMcpStatus(): Promise<McpStatus | null> {
  try {
    const st = await invoke<McpStatus>("mcp_get_status");
    updateMcpBadge(st);
    return st;
  } catch (e) {
    opsLog("ERR", "mcp_get_status failed", { error: String(e) });
    return null;
  }
}

async function openMcpDialog() {
  const st = await refreshMcpStatus();
  if (!st) {
    showToast("无法读取 MCP 状态");
    return;
  }
  applyMcpStatusToForm(st);
  ($("dlg-mcp") as HTMLDialogElement).showModal();
}

function setupMcpDialog() {
  $("mcp-btn-show-token").addEventListener("click", () => {
    mcpTokenVisible = !mcpTokenVisible;
    const token = $("mcp-token") as HTMLInputElement;
    token.type = mcpTokenVisible ? "text" : "password";
    ($("mcp-btn-show-token") as HTMLButtonElement).textContent = mcpTokenVisible
      ? "隐藏"
      : "显示";
  });

  $("mcp-btn-copy-token").addEventListener("click", async () => {
    const token = ($("mcp-token") as HTMLInputElement).value;
    if (!token) {
      showToast("Token 为空");
      return;
    }
    try {
      await navigator.clipboard.writeText(token);
      showToast("Token 已复制");
    } catch {
      showToast("无法写入剪贴板");
    }
  });

  $("mcp-btn-copy-config").addEventListener("click", async () => {
    const text = ($("mcp-client-config") as HTMLTextAreaElement).value;
    if (!text) {
      showToast("配置为空");
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
      showToast("HTTP Client 配置已复制");
    } catch {
      showToast("无法写入剪贴板");
    }
  });

  $("mcp-btn-copy-stdio")?.addEventListener("click", async () => {
    const text = ($("mcp-stdio-config") as HTMLTextAreaElement).value;
    if (!text) {
      showToast("stdio 配置为空");
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
      showToast("stdio Client 配置已复制");
    } catch {
      showToast("无法写入剪贴板");
    }
  });

  $("mcp-btn-regen").addEventListener("click", async () => {
    if (
      !window.confirm(
        "重新生成 Token 后，已配置的 AI Client 需要更新 Authorization。继续？",
      )
    ) {
      return;
    }
    try {
      const st = await invoke<McpStatus>("mcp_regenerate_token");
      applyMcpStatusToForm(st);
      showToast("Token 已重新生成");
      opsLog("MCP", "ui_token_regenerated");
    } catch (e) {
      showToast(String(e));
    }
  });

  $("mcp-btn-apply").addEventListener("click", async () => {
    const enabled = ($("mcp-enabled") as HTMLInputElement).checked;
    const portRaw = Number(($("mcp-port") as HTMLInputElement).value);
    if (!Number.isFinite(portRaw) || portRaw < 1 || portRaw > 65535) {
      showToast("端口无效");
      return;
    }
    try {
      const st = await invoke<McpStatus>("mcp_apply", {
        req: { enabled, port: Math.floor(portRaw) },
      });
      applyMcpStatusToForm(st);
      if (enabled && st.running) {
        showToast(`MCP 已启动 :${st.actualPort ?? st.port}`);
      } else if (enabled && !st.running) {
        showToast(st.lastError || "MCP 启用失败");
      } else {
        showToast("MCP 已关闭");
      }
      opsLog("MCP", "ui_apply", {
        enabled: st.enabled,
        running: st.running,
        port: st.actualPort ?? st.port,
      });
    } catch (e) {
      showToast(String(e));
      opsLog("ERR", "mcp_apply failed", { error: String(e) });
    }
  });
}

function isCopyChord(e: KeyboardEvent): boolean {
  if (e.altKey || e.shiftKey) return false;
  if (!e.ctrlKey && !e.metaKey) return false;
  return e.key === "c" || e.key === "C" || e.code === "KeyC";
}

/** True when Ctrl+C should stay native (dialogs, selected text in an input). */
function shouldLetBrowserHandleCopy(e: KeyboardEvent): boolean {
  const t = e.target;
  if (!(t instanceof HTMLElement)) return false;
  // xterm keeps selection on canvas; its helper textarea is usually empty.
  if (t.classList.contains("xterm-helper-textarea")) return false;
  if (t.closest("dialog")) return true;
  if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement) {
    return t.selectionStart !== t.selectionEnd;
  }
  return t.isContentEditable;
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

  // Capture: run before xterm maps Ctrl+C to ETX / SIGINT.
  window.addEventListener(
    "keydown",
    (e) => {
      if (!isCopyChord(e)) return;
      if (shouldLetBrowserHandleCopy(e)) return;
      const active = getActive();
      if (active?.term.hasSelection()) {
        e.preventDefault();
        e.stopPropagation();
        if (!e.repeat) void handleMenuAction("copy");
        return;
      }
      if (e.ctrlKey && !e.metaKey && active?.isLive()) {
        e.preventDefault();
        e.stopPropagation();
        active.sendInterrupt();
      }
    },
    true,
  );
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
      // Post-separator UI diagnostics (independent of ECHO throttle).
      view.noteSepUiData(bytes.length);
      // Never invoke ops_log synchronously from this handler — re-entrant IPC
      // during emit can deadlock the runtime under tail/grep floods.
      // Small chunks only, deferred + budgeted.
      if (bytes.length <= 256 && shouldLogUiEcho(sid)) {
        const text = new TextDecoder("utf-8", { fatal: false }).decode(bytes);
        opsLogDeferred("ECHO", "ui_receive", {
          len: bytes.length,
          text: previewText(text, 120),
          sid: sid.slice(0, 8),
        });
      }
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
        maybeAutoLaunchXftp(view);
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
  syncCmdSeparatorMenu();
  syncXftpAutoMenu();
  setupShortcuts();
  await setupEvents();

  $("auth-type").addEventListener("change", () => {
    syncAuthFields();
    updateSecretSaveHints();
  });
  $("save-password").addEventListener("change", () => updateSecretSaveHints());
  document
    .getElementById("save-passphrase")
    ?.addEventListener("change", () => updateSecretSaveHints());
  setupSecretMaskClearOnEdit();

  $("btn-new-tab").addEventListener("click", () => openPropsDialog("create"));
  $("btn-empty-new").addEventListener("click", () => openPropsDialog("create"));
  $("btn-empty-manager").addEventListener("click", () => void openManager());

  $("mgr-btn-new").addEventListener("click", () => {
    ($("dlg-session-manager") as HTMLDialogElement).close();
    // Cancel / 仅保存 → return to manager; 连接 paths clear the flag.
    openPropsDialog("create", { returnToManager: true });
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
    openPropsDialog("edit-profile", { returnToManager: true });
  });
  $("mgr-btn-delete").addEventListener("click", () => {
    void deleteManagerProfiles();
  });
  $("mgr-btn-open").addEventListener("click", () => {
    void openManagerProfiles();
  });
  $("mgr-check-all")?.addEventListener("click", (e) => {
    e.stopPropagation();
  });
  $("mgr-check-all")?.addEventListener("change", () => {
    const allBox = $("mgr-check-all") as HTMLInputElement;
    const visible = visibleManagerProfiles();
    if (allBox.checked) {
      for (const p of visible) mgrCheckedIds.add(p.id);
    } else {
      for (const p of visible) mgrCheckedIds.delete(p.id);
    }
    mgrCheckAnchorId = visible[0]?.id ?? null;
    paintManagerChecks();
  });
  const mgrSearch = document.getElementById("mgr-search") as HTMLInputElement | null;
  mgrSearch?.addEventListener("input", () => {
    renderManagerList();
  });
  mgrSearch?.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && mgrSearch.value) {
      e.preventDefault();
      e.stopPropagation();
      mgrSearch.value = "";
      renderManagerList();
    }
  });
  $("dlg-session-manager")?.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && (e.key === "f" || e.key === "F")) {
      e.preventDefault();
      mgrSearch?.focus();
      mgrSearch?.select();
    }
  });

  $("mgr-btn-export")?.addEventListener("click", () => {
    void exportProfilesFromManager();
  });
  $("mgr-btn-import")?.addEventListener("click", () => {
    const input = $("mgr-import-file") as HTMLInputElement;
    input.value = "";
    input.click();
  });
  $("mgr-import-file")?.addEventListener("change", () => {
    const input = $("mgr-import-file") as HTMLInputElement;
    const file = input.files?.[0];
    if (file) void importProfilesFromManager(file);
  });

  // Prevent form submit default
  $("form-session-props").addEventListener("submit", (e) => e.preventDefault());

  setupMcpDialog();
  void refreshMcpStatus();

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
