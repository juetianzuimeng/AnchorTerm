# PR2 代码评审报告

- **范围：** 真实 `session_id` API、事件 payload 对象化、lifecycle（disconnect 保留 / close 移除）、前后端同提交；cwd 失败回滚已在基线  
- **评审人：** 按 [CODE-REVIEW-PLAN.md](../CODE-REVIEW-PLAN.md) §PR2  
- **日期：** 2026-07-25  
- **对照设计：** [UI-MULTI-TAB.md](../UI-MULTI-TAB.md) §3.4、§4.1–4.2、KD3/5/11/12  
- **结论：** **通过**  
- **允许进入 PR3：** **是**

---

## 变更摘要

删除 PR1 的 `"default"` shim；`AppState.sessions` 默认空 map，客户端 UUID 经 `get_or_insert_runtime` 在 **任何 emit 前** 入表。会话命令一律强制 `session_id`；`connect` 返回 `{ session_id }`；新增 `close_session` / `list_sessions` / `SessionNotFound`。`session://data|cwd|error` 改为带 `session_id` 的对象；前端生成 UUID、IPC 顶层参数用 camelCase `sessionId`（Tauri 2 约定）、事件按 sid 路由。单 tab UI 在 disconnect 后复用同一 sid，以支持 restore_target。

用户联调（`session-20260725-030601`）：连接、多命令、失败 `cd` 回滚、手动断再连恢复 `/home/tguser/tg1` 均通过。

---

## 门禁结果

### 5.2.1 范围门禁

| 检查 | 结果 | 证据 |
|------|------|------|
| Shim 删除 | ✅ | 源码无 `DEFAULT_SESSION_ID`；`AppState::default` 空 map |
| 同提交 | ✅ | Rust emit + `main.ts` listen 同批改造；曾暴露 `sessionId` camelCase 问题已修 |
| API | ✅ | `ConnectRequest.session_id` 必填；`ConnectResponse`；`close_session`/`list_sessions` 已注册 |
| 无参 snapshot | ✅ | `get_session_snapshot(session_id)` 强制参数；无 current_session |

### 5.2.2 session_id 时序与生命周期

| ID | 结果 | 说明 |
|----|------|------|
| R2-1 | ✅ P0 | 前端 `requireSessionId()` / `newSessionId()` 在 invoke 前；单 tab 无多 View 路由表属 PR3 |
| R2-2 | ✅ P0 | `connect`：`get_or_insert_runtime` 后才 `set_state(Connecting)` / SSH |
| R2-3 | ✅ P1 | `transport.is_some()` → `AlreadyConnected`（语义同设计 SessionAlreadyConnected） |
| R2-4 | ✅ P0 | disconnect 不 remove map；同 sid 再连 + freeze/restore；**用户日志验证** |
| R2-5 | ✅ P0 | close：`auto_reconnect=false` → `gen++` → take transport → `remove_runtime` |
| R2-6 | ✅ P0 | reconnect 查 gen；finish/on_data 用 `get_runtime`，已 remove 则 return + ERR 日志 |
| R2-7 | ✅ P0 | pump/wait/finish 捕获 `session_id` |
| R2-8 | ✅ P1 | `SessionNotFound`；前端 `event_route_miss` |

### 5.2.3 事件契约

| ID | 结果 | 说明 |
|----|------|------|
| R2-9 | ✅ | `DataEvent { session_id, data_b64 }` |
| R2-10 | ✅ | state snapshot / `CwdEvent` / `ErrorEvent` 均含 sid |
| R2-11 | ✅ | 前端对象解码 + `payloadSessionId` 兼容 snake/camel；无裸 string 当 base64 |

### 5.2.4 测试

| ID | 结果 | 说明 |
|----|------|------|
| R2-12 UT-1 | ✅ | `snapshot_includes_session_id` |
| R2-13 UT-2 | ✅ | `get_or_insert_and_remove` |
| R2-14 UT-3 | ✅ | `disconnect_keeps_map_entry_transport_none` |
| R2-15 UT-4 | ✅ | `reconnect_gen_bump_isolates_loops` |
| R2-16 UT-5 | ✅ | `last_stty_is_per_runtime` |
| R2-17 手工 | ✅ | 用户验证：命令、cd 回滚、手动断再连 cwd |
| R2-18 ACCEPTANCE | ✅ | MS-\* 草稿 + PR2 单 tab 项已写入 |

### 公共 G-*（抽查）

| ID | 结果 |
|----|------|
| G-1 OpenSSH | ✅ |
| G-2/G-3 cwd 锁与 restore | ✅ |
| G-5 日志无密文；`sid=` | ✅ |
| G-6 临时 key Drop | ✅ 日志有 temp removed |
| G-7 阶段 1 | ✅ 用户验证 |

### 自动化

```
cargo test -p anchorterm --lib
→ 26 passed
```

---

## 问题列表

| ID | 级别 | 位置 | 描述 | 状态 |
|----|------|------|------|------|
| — | — | — | 无 open P0/P1 | — |

### 已关闭（本阶段过程问题，不阻断收口）

| 项 | 处理 |
|----|------|
| Tauri 顶层参数需 `sessionId` 非 `session_id` | 前端已改；联调通过 |
| 乐观 cd 污染 restore_target | 基线已有 cd_rollback；联调 `cd tg10` 验证 |

### 遗留（下移，非阻断）

| 项 | 级别 | 目标 |
|----|------|------|
| 断开后 UI 可能二次 `setStatus` 清空 cwd 展示 | P2 | PR3/PR4 状态栏绑定 |
| `close_session` 后仍持 `Arc` 的 seed 任务可能晚到 emit | P2 | PR3 可在 seed 开头 `get_runtime` 再校验 map（可选加固） |
| 双 tab 隔离 / MS-1–5 全量手工 | P0 for PR3 | **PR3** |
| UI 未接 `close_session` 按钮（仅 API） | P2 | PR3 关 tab 时调用 |

---

## 与设计偏差

| 项 | 说明 |
|----|------|
| 错误枚举名 | 使用 `AlreadyConnected` 而非 `SessionAlreadyConnected`；消息已 per-session，可接受 |
| ErrorEvent 后端少 emit 路径 | 结构已定义；现网多走 state.message；前端已 listen，不阻断 |
| 单 tab | 符合 PR2 范围（PR3 才多 SessionView） |

---

## 是否允许进入下一阶段

**是** → 可开 **PR3：多 `SessionView` + 标签栏**。

---

## 签字区

| 角色 | 结论 | 日期 |
|------|------|------|
| 评审（本报告） | 通过 | 2026-07-25 |
| 开发自检 | 见 PR2-selfcheck.md | 2026-07-25 |
| 人工联调 | 通过（用户确认） | 2026-07-25 |
