import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

type AuthType = "password" | "public_key";
type SessionStateName =
  | "idle"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "disconnected"
  | "failed";

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
  state: SessionStateName;
  host?: string | null;
  username?: string | null;
  message?: string | null;
  cwd?: string | null;
  attempt?: number | null;
}

/** Local draft survives disconnect/reconnect (not tied to SSH socket). */
interface PendingInputBuffer {
  text: string;
  cursor: number;
}

const $ = <T extends HTMLElement>(id: string) =>
  document.getElementById(id) as T;

/** Write UI/diagnostic lines to C:\zengshangchun\AnchorTerm\操作日志 via Rust. */
function opsLog(
  category: string,
  message: string,
  detail?: Record<string, unknown> | string | null,
) {
  let d: string | null = null;
  if (detail != null) {
    d = typeof detail === "string" ? detail : JSON.stringify(detail);
  }
  // Fire-and-forget; never block UI on log failures.
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

function base64ToBytes(b64: string): Uint8Array {
  const binary = atob(b64);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

let term: Terminal;
let fitAddon: FitAddon;
let selectedProfileId: string | null = null;
let sessionState: SessionStateName = "idle";
let pendingDraft: string | null = null;

/**
 * shell — default: type in draft only; terminal is display (avoids double-type).
 * raw — keyboard goes to PTY (top/vim/passwd prompts).
 */
type InputMode = "shell" | "raw";
let inputMode: InputMode = "shell";

const draft: PendingInputBuffer = { text: "", cursor: 0 };

function isLive(): boolean {
  return sessionState === "connected";
}

function canEditDraft(): boolean {
  // Draft always editable; send only when connected.
  return true;
}

function applyInputMode() {
  const btn = $("btn-input-mode") as HTMLButtonElement;
  const terminalEl = $("terminal");
  if (inputMode === "shell") {
    btn.textContent = "Shell 模式";
    btn.classList.remove("raw");
    btn.title =
      "当前：Shell — 请在草稿框输入命令。点击可切换为 TUI 直通（top/vim）";
    terminalEl.classList.add("shell-mode");
    // Disable xterm keyboard capture so keys don't go to SSH twice.
    term.options.disableStdin = true;
  } else {
    btn.textContent = "TUI 直通";
    btn.classList.add("raw");
    btn.title =
      "当前：TUI 直通 — 键盘直接进终端。点击回到 Shell（草稿）模式";
    terminalEl.classList.remove("shell-mode");
    term.options.disableStdin = false;
    if (isLive()) term.focus();
  }
}

function toggleInputMode() {
  inputMode = inputMode === "shell" ? "raw" : "shell";
  opsLog("UI", `input_mode_toggle mode=${inputMode}`);
  applyInputMode();
  if (inputMode === "shell") {
    $("draft-input").focus();
  }
}

function setError(msg: string | null) {
  const el = $("error-msg");
  if (!msg) {
    el.hidden = true;
    el.textContent = "";
    return;
  }
  el.hidden = false;
  el.textContent = msg;
  opsLog("UI", "error_shown", { message: msg });
}

function setCwd(cwd?: string | null) {
  const el = $("cwd-text");
  if (cwd) {
    el.textContent = cwd;
    el.title = `cwd: ${cwd}`;
  } else {
    el.textContent = "";
    el.title = "远端工作目录";
  }
}

function setStatus(state: SessionStateName, message?: string | null, cwd?: string | null) {
  sessionState = state;
  const dot = $("status-dot");
  const text = $("status-text");
  dot.className = `dot ${state}`;
  const labels: Record<SessionStateName, string> = {
    idle: "未连接",
    connecting: "连接中…",
    connected: "已连接",
    reconnecting: "重连中…",
    disconnected: "已断开",
    failed: "失败",
  };
  text.textContent = message || labels[state] || state;
  opsLog("STATE", `ui_status state=${state}`, {
    message: message ?? null,
    cwd: cwd ?? null,
  });

  if (cwd !== undefined) {
    setCwd(cwd);
  }

  const overlay = $("overlay");
  const overlayText = $("overlay-text");
  if (state === "connecting" || state === "reconnecting") {
    overlay.classList.remove("hidden");
    overlay.classList.toggle("reconnecting", state === "reconnecting");
    overlayText.textContent =
      message || (state === "reconnecting" ? "重连中…" : "连接中…");
  } else {
    overlay.classList.add("hidden");
    overlay.classList.remove("reconnecting");
  }

  const busy = state === "connecting" || state === "reconnecting";
  const live = state === "connected";
  ($("btn-connect") as HTMLButtonElement).disabled = live || busy;
  // Allow cancel via disconnect while reconnecting / connecting.
  ($("btn-disconnect") as HTMLButtonElement).disabled =
    state === "idle" || state === "failed" || state === "disconnected";
  ($("btn-draft-send") as HTMLButtonElement).disabled = !live;

  if (live && pendingDraft !== null) {
    const line = pendingDraft;
    pendingDraft = null;
    void flushPendingLine(line);
  }
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
  return { authType, host, port, username };
}

function fillForm(p: HostProfile) {
  selectedProfileId = p.id;
  ($("profile-name") as HTMLInputElement).value = p.name;
  ($("host") as HTMLInputElement).value = p.host;
  ($("port") as HTMLInputElement).value = String(p.port);
  ($("username") as HTMLInputElement).value = p.username;
  ($("auth-type") as HTMLSelectElement).value = p.auth_type;
  ($("private-key-path") as HTMLInputElement).value = p.private_key_path || "";
  ($("password") as HTMLInputElement).value = "";
  ($("save-password") as HTMLInputElement).checked = p.has_saved_password;
  ($("passphrase") as HTMLInputElement).value = "";
  syncAuthFields();
  renderProfiles();
}

async function loadProfiles() {
  const profiles = await invoke<HostProfile[]>("list_profiles");
  (window as unknown as { __profiles: HostProfile[] }).__profiles = profiles;
  renderProfiles();
}

function renderProfiles() {
  const profiles =
    (window as unknown as { __profiles?: HostProfile[] }).__profiles || [];
  const list = $("profile-list");
  list.innerHTML = "";
  for (const p of profiles) {
    const li = document.createElement("li");
    if (p.id === selectedProfileId) li.classList.add("active");
    li.innerHTML = `<div class="name">${escapeHtml(p.name)}</div>
      <div class="meta">${escapeHtml(p.username)}@${escapeHtml(p.host)}:${p.port} · ${
        p.auth_type === "password" ? "密码" : "私钥"
      }</div>`;
    li.addEventListener("click", () => fillForm(p));
    list.appendChild(li);
  }
}

function escapeHtml(s: string) {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function buildAuth(authType: AuthType): Record<string, unknown> | null {
  if (authType === "password") {
    const password = ($("password") as HTMLInputElement).value;
    const save_password = ($("save-password") as HTMLInputElement).checked;
    return {
      type: "password",
      password: password || null,
      save_password,
    };
  }
  const private_key_path = (
    $("private-key-path") as HTMLInputElement
  ).value.trim();
  if (!private_key_path) {
    setError("请填写私钥路径");
    return null;
  }
  const passphrase = ($("passphrase") as HTMLInputElement).value;
  return {
    type: "public_key",
    private_key_path,
    passphrase: passphrase || null,
  };
}

async function doConnect() {
  setError(null);
  const { authType, host, port, username } = readForm();
  if (!host || !username) {
    setError("请填写主机与用户名");
    return;
  }
  const auth = buildAuth(authType);
  if (!auth) return;

  const dims = termDims();
  opsLog("UI", "click_connect", {
    host,
    port,
    username,
    authType,
    cols: dims.cols,
    rows: dims.rows,
    profile_id: selectedProfileId,
    // never log password/passphrase
  });
  setStatus("connecting", "正在连接…");
  try {
    await invoke("connect", {
      req: {
        host,
        port,
        username,
        auth,
        cols: dims.cols,
        rows: dims.rows,
        profile_id: selectedProfileId,
      },
    });
    opsLog("UI", "connect invoke returned ok");
    // Fallback if the state event is missed: mark live so draft Send works.
    if (sessionState !== "connected") {
      try {
        const snap = await invoke<SessionSnapshot>("get_session_snapshot");
        setStatus(snap.state, snap.message, snap.cwd);
      } catch {
        setStatus("connected", "已连接");
      }
    }
    fitAddon.fit();
    invoke("resize", { cols: term.cols, rows: term.rows }).catch(() => {});
    // Focus draft for shell line entry; xterm still accepts raw keys.
    $("draft-input").focus();
  } catch (e) {
    const msg = String(e);
    opsLog("ERR", "connect invoke failed", { message: msg });
    setError(msg);
    setStatus("failed", msg);
  }
}

async function doDisconnect() {
  setError(null);
  pendingDraft = null;
  opsLog("UI", "click_disconnect");
  try {
    await invoke("disconnect");
    setStatus("idle", "已手动断开");
  } catch (e) {
    setError(String(e));
  }
}

async function saveProfile() {
  setError(null);
  const { authType, host, port, username } = readForm();
  const name =
    ($("profile-name") as HTMLInputElement).value.trim() ||
    `${username}@${host}`;
  if (!host || !username) {
    setError("保存配置需要主机与用户名");
    return;
  }

  try {
    const profile = await invoke<HostProfile>("save_profile", {
      req: {
        id: selectedProfileId,
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
    fillForm(profile);
  } catch (e) {
    setError(String(e));
  }
}

async function deleteProfile() {
  if (!selectedProfileId) {
    setError("请先选择要删除的配置");
    return;
  }
  try {
    await invoke("delete_profile", { id: selectedProfileId });
    selectedProfileId = null;
    await loadProfiles();
  } catch (e) {
    setError(String(e));
  }
}

function termDims() {
  return {
    cols: term.cols || 80,
    rows: term.rows || 24,
  };
}

function syncDraftFromInput() {
  const input = $("draft-input") as HTMLInputElement;
  draft.text = input.value;
  draft.cursor = input.selectionStart ?? draft.text.length;
}

function restoreDraftToInput() {
  const input = $("draft-input") as HTMLInputElement;
  input.value = draft.text;
  const pos = Math.min(draft.cursor, draft.text.length);
  input.setSelectionRange(pos, pos);
}

async function sendDraftLine() {
  // Always dismiss completion UI before send — Enter must run the line,
  // not only close the popup (regression when multi-match Tab left completeUi open).
  hideCompletePopup();

  if (!canEditDraft()) return;
  syncDraftFromInput();
  const line = draft.text;
  opsLog("UI", "draft_send_attempt", {
    line: previewText(line),
    live: isLive(),
    mode: inputMode,
  });
  if (!line.trim()) {
    setError("草稿为空，请输入命令后再发送");
    return;
  }

  if (!isLive()) {
    // Queue until Connected (e.g. mid-reconnect).
    pendingDraft = line;
    setError("当前未连接：草稿已保留，重连成功后将自动发送");
    opsLog("UI", "draft_queued_offline", { line: previewText(line) });
    return;
  }

  await flushPendingLine(line);
}

/** True for PTY control / interrupt sequences that shell mode must still send. */
function isControlKeyPayload(data: string): boolean {
  if (!data) return false;
  // Single C0 control (Ctrl+C = \x03, Ctrl+D = \x04, Ctrl+Z = \x1a, Ctrl+L = \x0c, …)
  if (data.length === 1) {
    const c = data.charCodeAt(0);
    if (c < 32 && c !== 9 /* Tab — draft handles Tab complete */) return true;
    if (c === 127) return true; // DEL
  }
  // CSI / SS3 sequences (arrows, Home/End, etc.) — allow navigation on remote if focused
  if (data.startsWith("\x1b")) return true;
  return false;
}

/** Write remote/local bytes into xterm (text-safe). */
function writeToTerm(data: string | Uint8Array) {
  if (typeof data === "string") {
    term.write(data);
  } else {
    // Prefer string path so multi-byte UTF-8 is not split across chunks oddly.
    term.write(new TextDecoder("utf-8", { fatal: false }).decode(data));
  }
}

async function flushPendingLine(line: string) {
  const logical = line.replace(/[\r\n]+$/g, "");
  if (!logical.trim()) return;

  opsLog("CMD", "ui_submit_line", { line: previewText(logical) });

  // Clear draft immediately so the user sees the send was accepted.
  draft.text = "";
  draft.cursor = 0;
  restoreDraftToInput();
  setError(null);

  try {
    // Direct PTY write on the Rust side. Remote ECHO paints the terminal.
    await invoke("submit_line", { line: logical });
    opsLog("CMD", "ui_submit_line_ok", { line: previewText(logical) });
    $("draft-input").focus();
  } catch (e) {
    // Put the line back so the user can retry.
    draft.text = logical;
    draft.cursor = logical.length;
    restoreDraftToInput();
    opsLog("ERR", "ui_submit_line_failed", {
      line: previewText(logical),
      error: String(e),
    });
    setError(String(e));
  }
}

function setupTerminal() {
  term = new Terminal({
    cursorBlink: true,
    fontSize: 14,
    fontFamily: 'Consolas, "Cascadia Mono", "Courier New", monospace',
    // Safety when remote sends bare LF without CR (common with partial tty modes).
    convertEol: true,
    theme: {
      background: "#0a0e14",
      foreground: "#e7ecf3",
      cursor: "#3d8bfd",
      selectionBackground: "rgba(61,139,253,0.35)",
    },
    allowProposedApi: true,
  });
  fitAddon = new FitAddon();
  term.loadAddon(fitAddon);
  term.open($("terminal"));
  // Layout may not be ready on first paint; fit now and again next frame.
  fitAddon.fit();
  requestAnimationFrame(() => {
    fitAddon.fit();
    if (isLive()) {
      invoke("resize", { cols: term.cols, rows: term.rows }).catch(() => {});
    }
  });

  // Keyboard → SSH:
  // - raw/TUI mode: all keys go to the remote PTY
  // - shell mode: draft bar owns typing, BUT still forward control keys
  //   (Ctrl+C/D/Z/L, arrows sometimes) so a hung remote command can be interrupted.
  term.onData((data) => {
    if (!isLive()) return;
    if (inputMode !== "raw") {
      if (!isControlKeyPayload(data)) return;
      opsLog("UI", "shell_ctrl_key", {
        text: previewText(data, 40),
        len: data.length,
      });
    } else {
      opsLog("UI", "raw_key", { text: previewText(data, 40), len: data.length });
    }
    const bytes = new TextEncoder().encode(data);
    invoke("write_bytes", { dataB64: bytesToBase64(bytes) }).catch((e) => {
      opsLog("ERR", "term_key write failed", { error: String(e) });
      setError(String(e));
    });
  });

  // In shell mode, clicking the terminal focuses the draft (not a second input path).
  $("terminal").addEventListener("mousedown", (e) => {
    if (inputMode === "shell" && isLive()) {
      e.preventDefault();
      opsLog("UI", "click_terminal_focus_draft");
      $("draft-input").focus();
    }
  });

  let resizeTimer: number | undefined;
  const ro = new ResizeObserver(() => {
    fitAddon.fit();
    if (!isLive()) return;
    // Coalesce rapid ResizeObserver storms — each resize used to block PTY writes.
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(() => {
      if (!isLive()) return;
      invoke("resize", { cols: term.cols, rows: term.rows }).catch(() => {});
    }, 100);
  });
  ro.observe($("terminal"));

  applyInputMode();
}

// --- Tab completion (Xshell-like) ------------------------------------------

interface CompleteResult {
  line: string;
  cursor: number;
  candidates: string[];
  token_start: number;
  token_end: number;
}

interface CompleteUiState {
  /** Candidates for current token (full replacement strings). */
  candidates: string[];
  /** Index into candidates when cycling. */
  index: number;
  tokenStart: number;
  tokenEnd: number;
  /** Line snapshot when the candidate list was fetched. */
  baseLine: string;
  busy: boolean;
}

let completeUi: CompleteUiState | null = null;

function hideCompletePopup() {
  const popup = $("complete-popup");
  popup.classList.add("hidden");
  popup.innerHTML = "";
  completeUi = null;
}

function applyCompleteToDraft(line: string, cursor: number) {
  draft.text = line;
  draft.cursor = cursor;
  restoreDraftToInput();
  const input = $("draft-input") as HTMLInputElement;
  input.focus();
}

function showCompletePopup(candidates: string[], activeIndex: number) {
  const popup = $("complete-popup");
  popup.innerHTML = "";
  if (candidates.length <= 1) {
    popup.classList.add("hidden");
    return;
  }

  const maxShow = 80;
  const slice = candidates.slice(0, maxShow);
  for (let i = 0; i < slice.length; i++) {
    const li = document.createElement("li");
    li.setAttribute("role", "option");
    li.textContent = slice[i];
    if (i === activeIndex) li.classList.add("active");
    li.addEventListener("mousedown", (ev) => {
      // mousedown so input doesn't blur before click.
      ev.preventDefault();
      pickCandidate(i);
    });
    popup.appendChild(li);
  }
  if (candidates.length > maxShow) {
    const hint = document.createElement("div");
    hint.className = "complete-hint";
    hint.textContent = `…共 ${candidates.length} 项，Tab 循环 · Enter 发送 · Esc 关闭`;
    popup.appendChild(hint);
  } else {
    const hint = document.createElement("div");
    hint.className = "complete-hint";
    hint.textContent = `Tab / ↓↑ 切换候选 · Enter 发送 · Esc 关闭`;
    popup.appendChild(hint);
  }
  popup.classList.remove("hidden");

  // Scroll active into view.
  const active = popup.querySelector("li.active") as HTMLElement | null;
  active?.scrollIntoView({ block: "nearest" });
}

function pickCandidate(index: number) {
  if (!completeUi || index < 0 || index >= completeUi.candidates.length) return;
  const c = completeUi.candidates[index];
  const chars = [...completeUi.baseLine];
  const before = chars.slice(0, completeUi.tokenStart).join("");
  const after = chars.slice(completeUi.tokenEnd).join("");
  const newLine = before + c + after;
  const newCursor = completeUi.tokenStart + [...c].length;
  completeUi.index = index;
  applyCompleteToDraft(newLine, newCursor);
  showCompletePopup(completeUi.candidates, index);
}

function cycleCandidate(delta: number) {
  if (!completeUi || completeUi.candidates.length === 0) return;
  const n = completeUi.candidates.length;
  const next = (completeUi.index + delta + n) % n;
  pickCandidate(next);
}

async function requestTabComplete() {
  if (!isLive()) {
    setError("未连接，无法补全");
    return;
  }
  syncDraftFromInput();
  const input = $("draft-input") as HTMLInputElement;
  const line = draft.text;
  const cursor = input.selectionStart ?? line.length;

  // If popup is open with candidates, cycle instead of re-fetching.
  if (completeUi && completeUi.candidates.length > 1 && !completeUi.busy) {
    cycleCandidate(1);
    return;
  }

  if (completeUi?.busy) return;
  completeUi = {
    candidates: [],
    index: 0,
    tokenStart: 0,
    tokenEnd: cursor,
    baseLine: line,
    busy: true,
  };

  try {
    const result = await invoke<CompleteResult>("complete_draft", {
      line,
      cursor,
    });
    setError(null);

    if (!result.candidates || result.candidates.length === 0) {
      hideCompletePopup();
      // Still apply line (unchanged) — soft bell in terminal.
      writeToTerm("\x07");
      return;
    }

    applyCompleteToDraft(result.line, result.cursor);

    if (result.candidates.length === 1) {
      hideCompletePopup();
      // Directory completions end with / — stay ready for next Tab.
      return;
    }

    // Find which candidate matches applied common prefix best for highlight.
    let idx = result.candidates.findIndex(
      (c) => c === result.line.slice(result.token_start, result.cursor)
    );
    if (idx < 0) idx = 0;

    completeUi = {
      candidates: result.candidates,
      index: idx,
      tokenStart: result.token_start,
      tokenEnd: result.token_end,
      baseLine: line,
      busy: false,
    };
    // After applying common prefix, token_end for cycling should be the new cursor
    // relative to original base — pickCandidate uses baseLine token_start/end.
    // Update tokenEnd on baseLine to original end; pickCandidate replaces
    // [tokenStart, tokenEnd) of baseLine with candidate — correct.
    showCompletePopup(result.candidates, idx);
  } catch (e) {
    hideCompletePopup();
    setError(String(e));
  } finally {
    if (completeUi) completeUi.busy = false;
  }
}

function setupDraft() {
  const input = $("draft-input") as HTMLInputElement;
  input.addEventListener("input", () => {
    syncDraftFromInput();
    // Editing invalidates the candidate list.
    hideCompletePopup();
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Tab") {
      e.preventDefault();
      void requestTabComplete();
      return;
    }
    if (e.key === "Escape") {
      if (completeUi) {
        e.preventDefault();
        hideCompletePopup();
      }
      return;
    }
    // Navigate candidates only — Enter always sends the draft line (Xshell-like).
    if (completeUi && completeUi.candidates.length > 1) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        cycleCandidate(1);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        cycleCandidate(-1);
        return;
      }
    }
    if (e.key === "Enter") {
      e.preventDefault();
      void sendDraftLine();
    }
  });
  // Click outside closes popup (do not steal the send button click).
  document.addEventListener("click", (ev) => {
    const t = ev.target as Node | null;
    if (!t) return;
    const wrap = document.querySelector(".draft-wrap");
    if (wrap && !wrap.contains(t)) {
      hideCompletePopup();
    }
  });
  $("btn-draft-send").addEventListener("click", (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    void sendDraftLine();
  });
  $("btn-input-mode").addEventListener("click", (ev) => {
    ev.preventDefault();
    toggleInputMode();
  });
  restoreDraftToInput();
}

async function setupEvents() {
  await listen<string>("session://data", (event) => {
    try {
      const raw = event.payload;
      const b64 = typeof raw === "string" ? raw : String(raw ?? "");
      if (!b64) return;
      const bytes = base64ToBytes(b64);
      const text = new TextDecoder("utf-8", { fatal: false }).decode(bytes);
      opsLog("ECHO", "ui_receive", {
        len: bytes.length,
        text: previewText(text, 200),
      });
      writeToTerm(bytes);
    } catch (e) {
      console.error("session://data decode failed", e);
      opsLog("ERR", "echo decode failed", { error: String(e) });
    }
  });

  await listen<string>("session://cwd", (event) => {
    opsLog("CWD", "ui_cwd_event", { path: event.payload });
    setCwd(event.payload);
  });

  await listen<SessionSnapshot>("session://state", (event) => {
    const snap = event.payload;
    opsLog("STATE", "ui_state_event", {
      state: snap.state,
      message: snap.message ?? null,
      cwd: snap.cwd ?? null,
      attempt: snap.attempt ?? null,
    });
    setStatus(snap.state, snap.message, snap.cwd);
    if (snap.state === "failed") {
      setError(snap.message || "连接失败");
    }
    // Ensure local xterm size is correct; remote stty is debounced in Rust.
    // Avoid multiple immediate resize→stty floods right after connect (races with shell).
    if (snap.state === "connected" && term) {
      fitAddon.fit();
      window.setTimeout(() => {
        if (!isLive()) return;
        invoke("resize", { cols: term.cols, rows: term.rows }).catch(() => {});
      }, 600);
      // Stay in shell mode by default; focus draft for command entry.
      if (inputMode === "shell") {
        applyInputMode();
        $("draft-input").focus();
      }
    }
  });

  await listen<{ message?: string }>("session://error", (event) => {
    const msg = event.payload?.message;
    if (msg) {
      opsLog("ERR", "session_error_event", { message: msg });
      setError(String(msg));
    }
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  setupTerminal();
  setupDraft();
  await setupEvents();
  syncAuthFields();

  $("auth-type").addEventListener("change", () => {
    opsLog("UI", "auth_type_change", {
      value: ($("auth-type") as HTMLSelectElement).value,
    });
    syncAuthFields();
  });
  $("btn-connect").addEventListener("click", () => {
    opsLog("UI", "click btn-connect");
    void doConnect();
  });
  $("btn-disconnect").addEventListener("click", () => {
    opsLog("UI", "click btn-disconnect");
    void doDisconnect();
  });
  $("btn-save-profile").addEventListener("click", () => {
    opsLog("UI", "click btn-save-profile");
    void saveProfile();
  });
  $("btn-delete-profile").addEventListener("click", () => {
    opsLog("UI", "click btn-delete-profile");
    void deleteProfile();
  });

  try {
    await loadProfiles();
    const info = await invoke<{ dir: string; latest: string; session?: string }>(
      "ops_log_info",
    );
    opsLog("SYS", "ui_ready", info);
  } catch (e) {
    setError(`加载配置失败: ${e}`);
  }

  setStatus("idle");
});
