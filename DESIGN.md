# AnchorTerm — 设计文档

**项目名称：** AnchorTerm  
**目标平台：** Windows  
**类型：** 支持自动重连与会话状态保留的 SSH 终端客户端  
**文档状态：** 可行性 + 功能设计 + 技术方案  
**产品 UI 语言（建议）：** 中英双语  

**第一阶段可执行方案：** [docs/PHASE1.md](docs/PHASE1.md)

---

## 1. 产品定位

**AnchorTerm** 将 **终端 UI 与 SSH 传输层解耦**。网络中断时保留屏幕与未发送输入；重连后 **重新锚定** 远端会话（工作目录，以及后续可选的全屏 TUI）。

| 部分 | 含义 |
|------|------|
| **Anchor** | 断线后锚定位置 — cwd、草稿、可选 TUI |
| **Term** | 真正的终端模拟器 |

---

## 2. 目标（用户需求）

| # | 需求 | 验收思路 |
|---|------|----------|
| 1 | 重连后静默恢复远端工作目录 | 安静 `cd` 到最近已知 cwd |
| 2 | 断线时保留未执行输入 | 草稿文本在断线/重连后仍在 |
| 3 | UI 与 SSH 分离 | SSH 仅 I/O；UI 拥有屏幕与输入 |
| 4 | `top` 类区域重绘 + 选择性恢复 | 真 VT；白名单 TUI；不自动跑普通脚本 |

---

## 3. 可行性结论

**可行。** 对标：Xshell、SecureCRT、MobaXterm、Termius。

**主模式 = PTY 真终端**（非命令日志）。

---

## 4. 架构

```
Presentation  → TerminalView + StatusBar + 草稿
Terminal Emulator → xterm.js (VT / alt screen / scrollback)
Session Controller → FSM · CwdTracker · Playbook · 发送队列
SSH Transport → Auth · Keepalive · PTY · 字节流
```

原则：UI 不拥有 socket；屏幕缓冲不断开即销毁；SSH 可拆除重建后走恢复剧本。

---

## 5. 功能要点

### 5.1 CWD

- 主策略：OSC 7 Shell Integration  
- 回退：解析用户 `cd` / `pushd` / `popd`  
- 重连剧本：PTY → 就绪 → `cd -- 'path'` → 恢复草稿  
- 阶段 1 **不做** TUI 重跑  

### 5.2 草稿输入

本地 `PendingInputBuffer`，与 socket 生命周期无关。阶段 1 可用独立草稿框。

### 5.3 认证

- 密码登录  
- OpenSSH 私钥（可选 passphrase）  
- 密码存系统凭据库（keyring），**禁止明文落盘**

---

## 6. 连接状态机

`Idle → Connecting → Connected → Disconnected → Reconnecting → Connected`  
失败 → `Failed`；手动断开 → `Idle`（不自动重连）。  
退避示例：1s, 2s, 5s, 10s, 30s。

---

## 7. 技术选型（已锁定）

**Tauri 2 + xterm.js + russh（ring 后端，避免 Windows 上 NASM/aws-lc 依赖）**

---

## 8. 实施阶段

| 阶段 | 内容 | 状态 |
|------|------|------|
| **1** | 密码/私钥登录、真终端、cwd+草稿+自动/手动重连恢复 | ✅ 已交付（见 README 会话沉淀） |
| **多标签** | 菜单化 UI、多 tab 多主机、session_id 化（[UI-MULTI-TAB.md](docs/UI-MULTI-TAB.md) PR1–PR5） | ✅ **提前交付**（不与 Jump 捆绑） |
| **2** | AppKind、可选 TUI 重跑恢复 | 未开始 |
| **3 余项** | Jump Host、分屏、SFTP、会话树等 | 未开始（见 [docs/ROADMAP-NEXT.md](docs/ROADMAP-NEXT.md)） |

详情见 [docs/PHASE1.md](docs/PHASE1.md)、[docs/UI-MULTI-TAB.md](docs/UI-MULTI-TAB.md)、[docs/CODE-REVIEW-PLAN.md](docs/CODE-REVIEW-PLAN.md)、**[docs/ROADMAP-NEXT.md](docs/ROADMAP-NEXT.md)**。

---

## 9. 关键决策

1. PTY 真终端  
2. 三层：UI · 模拟器 · SSH  
3. 草稿本地  
4. CWD：OSC 7 优先 + `cd` 回退 + 失败回滚；integration 手动文档  
5. 阶段 1 保证 cwd + 草稿；重连为新 login shell  
6. 不自研 VT（xterm.js）  
7. 恢复顺序：PTY → 就绪 → `cd` →（阶段 2）TUI → 草稿  
8. 技术栈锁定 Tauri 2；交互传输 = 系统 OpenSSH  
9. 密码 + 私钥认证；凭据不落明文；私钥口令连接前弹窗  
10. 默认自动重连；手动断开不重连  
11. 多会话：`session_id` 客户端 UUID；disconnect ≠ close；事件 payload 带 sid  
12. UI：菜单 + 对话框 + 标签栏（无常驻连接侧栏）  

---

## 10. 总结

难点是 **正确终端模拟** 与 **保守恢复策略**，不是单纯 TCP 重连。  
MVP 路径：成熟 VT + 清晰会话状态机；先保证连接与认证，再做 cwd/草稿/重连。
