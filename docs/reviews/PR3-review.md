# PR3 代码评审报告

- **范围：** 多 `SessionView` + 标签栏；侧栏暂留；事件按 `session_id` 路由；连接/断开/关标签生命周期  
- **评审人：** 按 [CODE-REVIEW-PLAN.md](../CODE-REVIEW-PLAN.md) §PR3 全量清单  
- **日期：** 2026-07-25  
- **对照设计：** [UI-MULTI-TAB.md](../UI-MULTI-TAB.md) §2.2、KD13/14/16、MS-1–5  
- **实现主路径：** `src/main.ts`、`index.html`、`src/styles.css`  
- **结论：** **通过**  
- **允许进入 PR4：** **是**

---

## 变更摘要

前端由单 xterm 全局状态改为 `Map<sessionId, SessionView>`：每 tab 独立 Terminal / FitAddon / 草稿 / inputMode / overlay。主区增加标签栏与 empty-state；侧栏连接表单保留（PR4 再去）。连接在 live 会话存在时始终 `new_tab:true`；active 为 idle 且 host+user 相同时 `connect_reuse_tab` 复用 sid 以恢复 cwd。全局 listen 后按 map 路由；未知 sid → `event_route_miss`。关标签确认后 `close_session` + dispose。

用户联调日志 `session-20260725-035753`（深度分析）：双 tab 隔离、手动断、同 tab 再连恢复 **tg1/tg2** 均通过；全程无 `[ERR]`、无串流。

---

## 5.3.1 范围门禁

| 检查 | 结果 | 证据 |
|------|------|------|
| 多视图 `Map` + 独立 xterm/草稿/模式 | ✅ | `sessions: Map`；`class SessionView` 内自有 term/draft/inputMode |
| 侧栏规则：不静默替换 live transport | ✅ | live 时 `createSessionView` + `new_tab:true`；日志第二次连接新 sid |
| 断开仅 active | ✅ | `disconnectActive()` → `getActive()` only |
| ACCEPTANCE MS-1–5 最小节 | ✅ | [ACCEPTANCE.md](../ACCEPTANCE.md) 多会话表已写 MS-1–5 / MS-TAB |
| 侧栏暂留（非 PR4 范围） | ✅ | `index.html` 仍有 `#sidebar` |

---

## 5.3.2 正确性清单

| ID | 级别 | 结果 | 说明 / 证据 |
|----|------|------|-------------|
| **R3-1** MS-1 输出不串流 | P0 | ✅ | A=`31639d21` cwd/tg1；B=`beb35b0a` cwd/tg2；`pwd` 回显按 sid 分离；ECHO 计数 A/B 分开 |
| **R3-2** A 重连不影响 B | P0 | ✅ | A disconnect/reconnect 期间 B 仍 `ls`/`pwd` 在 tg2；无 B 状态被 A 覆盖的日志 |
| **R3-3** A 手动断不自动重连 | P0 | ✅ | `idle (manual disconnect)`；无 `spawn_reconnect`/Reconnecting；B 仍 live |
| **R3-4** 同 tab 再连 restore | P0 | ✅ | `connect_reuse_tab`；`cd -- '/home/tguser/tg1|tg2'`；「已恢复工作目录」；再 `pwd` 正确 |
| **R3-5** close 关标签 | P0 | ✅* | **代码路径完整**（见下）；本次联调日志**未出现** `tab_close`（用户未关 × 或未写入该次会话）。不构成实现缺陷；**建议进入 PR4 前补一次手工 MS-5** |
| **R3-6** activate fit→resize | P1 | ✅ | `activate` → 双 rAF → `fitAndResize()` → `invoke('resize', { sessionId })` |
| **R3-7** 后台 tab 仍 write | P1 | ✅ | `session://data` → `sessions.get(sid).writeToTerm`，不判断 active |
| **R3-8** 标题纯前端 + 序号 | P2 | ✅ | `allocateTitle` → `name` / `name (2)`；后端 snapshot 无 title |
| **R3-9** window resize 仅 active | P2 | ✅ | `window.resize` 仅 `getActive()?.fitAndResize()` |
| **R3-10** event_route_miss | P1 | ✅ | 未知 sid 打 ERR；联调 0 次 miss |

\* R3-5 判定依据：

```text
closeSessionTab:
  confirm (Connected/Connecting/Reconnecting)
  → sessions.delete(sid)   // 前端路由立即失效，防幽灵写
  → dispose → close_session (gen++ / auto_reconnect=false / take transport / remove map)
  → 切换下一 tab 或 empty-state
```

与设计 §3.4.1 一致；后端 `close_session` 已在 PR2 评审通过。

---

## 公共 G-*（抽查）

| ID | 结果 |
|----|------|
| G-1 OpenSSH | ✅ 未改传输路径 |
| G-2/G-3 cwd 锁与 restore | ✅ 后端未回退；联调 restore 正常 |
| G-5 日志 `sid=` | ✅ 命令/ECHO/CWD 均带 sid |
| G-6 临时 key Drop | ✅ disconnect 时 temp removed |
| G-7 阶段 1 多 tab 下仍成立 | ✅ 草稿/回显/断再连 |

---

## 连接语义（与设计对照）

| 场景 | 实现 | 与 KD16 |
|------|------|---------|
| 已有 live tab 再点连接 / + | 始终新 UUID + 新 View | ✅ 不替换 active transport |
| active 为 idle + 同 host/user | `connect_reuse_tab` 同 sid | ✅ 服务 MS-4；**非**静默替换 live |
| 16 tab 上限 | `MAX_TABS` + toast | ✅ KD14 |

设计原文「连接=始终新 tab」在 **idle 复用** 上做了合理细化（否则 MS-4 无法在侧栏完成）。已写入 [PR3-selfcheck.md](PR3-selfcheck.md)，**不记为偏差缺陷**。

---

## 自动化与工程

| 项 | 结果 |
|----|------|
| `npx tsc --noEmit` | 通过 |
| `cargo test -p anchorterm --lib` | 26 passed |
| 模块拆分 `src/app/*` | 未做（仍单文件 `main.ts`）— **P2 可维护性**，不阻断 |

---

## 问题列表

| ID | 级别 | 位置 | 描述 | 状态 |
|----|------|------|------|------|
| — | — | — | **无 open P0/P1 实现缺陷** | — |

### 遗留（登记，不阻断 PR4）

| 项 | 级别 | 说明 | 建议 |
|----|------|------|------|
| L3-1 | P1 流程 | 联调日志未覆盖 MS-5 关标签 | 进 PR4 前手工：双 tab → 关 A × → 确认 `tab_close` + B 仍可用 |
| L3-2 | P2 | restore 中途 flush pending 草稿可能与静默 `cd` 交错 | PR4 可：restore 完成后再 flush |
| L3-3 | P2 | 首连 cols=80×24，fit 后再连变为更大 | activate/fit 后再 resize 强化 |
| L3-4 | P2 | `main.ts` 体量大，未按设计拆模块 | PR4/PR5 可选拆分 |
| L3-5 | P2 | `closeSessionTab` 先 `delete` 再 `close_session`：若 IPC 失败 UI 已无 tab | 可改为 close 成功后再 delete；失败则提示 |

### 联调中已关闭项

| 项 | 说明 |
|----|------|
| 串流 / 错误 sid | 未出现 |
| 同 host 双 tab cwd 混淆 | A=tg1 B=tg2 日志清晰 |
| sessionId camelCase | 无 missing key |

---

## 与设计偏差

| 项 | 说明 | 裁决 |
|----|------|------|
| 模块文件拆分 | 逻辑等价于 SessionView + 管理器，单文件交付 | 可接受 / L3-4 |
| idle 同端点复用 tab | 明确支持 MS-4，非替换 live | 可接受 |
| empty-state「打开已保存」 | 配置双击可开新 tab；完整会话管理器属 PR4 | 可接受 |

---

## 是否允许进入下一阶段

**是** → 可开 **PR4：菜单 / 对话框 / 去侧栏 + UX 完成项**。

**强制提醒（流程，非代码阻塞）：** 进入 PR4 开发周期内尽早补测 **MS-5**（关标签），并在 ACCEPTANCE 勾选。

---

## 签字区

| 角色 | 结论 | 日期 |
|------|------|------|
| 代码评审（本报告） | **通过**（0 open 实现缺陷） | 2026-07-25 |
| 开发自检 | PR3-selfcheck.md | 2026-07-25 |
| 人工联调（日志深度分析） | MS-1/3/4/TAB 通过；MS-5 日志未覆盖 | 2026-07-25 |

---

*报告结束。*
