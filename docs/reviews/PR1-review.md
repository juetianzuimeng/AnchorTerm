# PR1 代码评审报告

- **范围：** `SessionRuntime` + map + `"default"` shim；cwd 失败回滚（联调修复）
- **评审人：** 按 [CODE-REVIEW-PLAN.md](../CODE-REVIEW-PLAN.md) 清单
- **日期：** 2026-07-25
- **对照设计：** [UI-MULTI-TAB.md](../UI-MULTI-TAB.md)
- **结论：** **通过**
- **允许进入 PR2：** **是**

## 变更摘要

将单会话字段从 `AppState` 抽到 `SessionRuntime`，`AppState` 仅持 `HashMap`；PR1 仅允许 `session_id == "default"`。读泵 / `finish_session` / reconnect / seed 均捕获 runtime 或 sid。事件 payload 形状未改（`session://data` 仍为裸 base64）。另含联调修复：乐观 `cd` 远端失败回滚 + restore 目录缺失时降级登录目录。用户已人工验证通过。

## 门禁结果

### 公共 G-*

| ID | 结果 |
|----|------|
| G-1 OpenSSH `ssh -tt` | 通过 |
| G-2 Mutex 不重入（cwd 后释放再 snapshot） | 通过 |
| G-3 restore 交互 PTY `cd`；无侧信道校验交互 cwd | 通过 |
| G-4 草稿解耦 | 通过（前端未改） |
| G-5 日志无密码；含 `sid=` 前缀 | 通过 |
| G-6 临时密钥 Drop | 通过 |
| G-7 阶段 1 冒烟 | **用户验证通过**（含 cwd 回滚场景） |

### §4.3.1 迁移

| 符号 | 状态 |
|------|------|
| AppState → SessionRuntime + map | ✅ |
| snapshot / emit / set_state | ✅ per-runtime |
| connect / disconnect / restore / seed / reconnect | ✅ |
| submit / write / complete / resize | ✅ |
| on_data / finish_session / wait | ✅ 绑定 sid |
| connect_openssh / connect_session | ✅ 传 sid |
| last_stty 上 runtime | ✅ |

### 本阶段 R1-*

| ID | 结果 |
|----|------|
| R1-1 阶段 1 零回归 | 通过（用户验证） |
| R1-2 sessions 锁不嵌套 runtime 长临界区 | 通过（短锁取 Arc） |
| R1-3 不持 cwd 锁 emit/snapshot | 通过 |
| R1-4 无第二会话公开路径 | 通过（`get_runtime` 拒非 default） |
| R1-5 UT snapshot 含 id | 通过 |

### 自动化

`cargo test -p anchorterm --lib` — 24 passed（含 PR1 app_state 与 cwd 回滚用例）。

## 问题列表

| ID | 级别 | 描述 | 状态 |
|----|------|------|------|
| — | — | 无 open P0/P1 | — |

## 遗留项（下移 PR2）

| 项 | 说明 |
|----|------|
| 删除 `DEFAULT_SESSION_ID` shim | PR2 |
| 命令/事件强制 `session_id` + payload 对象化 | PR2 |
| `close_session` / map 多会话 lifecycle | PR2 |
| UT-2–5 lifecycle | PR2 |

## 与设计偏差

- **cwd 失败回滚** 属联调缺陷修复，不在原 PR1 文案内，但不改变 PR1 结构目标，纳入同基线合理。
- `SessionSnapshot.session_id` 为附加字段；`data` 事件仍裸 string（符合 PR1「不改 payload 形状」对 data 的要求；state 多一字段前端可忽略）。

## 是否允许进入下一阶段

**是** → 开 PR2。
