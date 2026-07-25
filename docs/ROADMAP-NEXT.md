# AnchorTerm — 下一阶段开发方向参考

| 字段 | 内容 |
|------|------|
| **文档标题** | 后续可选能力与路线图 |
| **日期** | 2026-07-25 |
| **状态** | 参考（非已承诺交付） |
| **前置交付** | PR1–PR5 多标签 / 菜单化已完成，见 [UI-MULTI-TAB.md](UI-MULTI-TAB.md)、[CODE-REVIEW-PLAN.md](CODE-REVIEW-PLAN.md)、[../README.md](../README.md) 会话沉淀 |
| **用途** | 给后续开发 / AI 会话选型、估工作量、拆 PR 时对照；**不是**当前必须做的 backlog 合同 |

---

## 1. 背景

当前产品已交付：

- 真终端（xterm.js）+ 系统 OpenSSH `ssh -tt`
- 密码 / 加密私钥（含 DES-EDE3-CBC）登录；口令连接前弹窗、**不落盘**
- per-session cwd 恢复、草稿、Tab 补全、Shell/TUI 模式
- 多标签多主机、`session_id` 化、菜单 + 对话框（无常驻连接侧栏）
- 设计 / 评审 / 验收文档闭环

以下方向来自 [DESIGN.md](../DESIGN.md) 阶段 2+、[UI-MULTI-TAB.md](UI-MULTI-TAB.md) **Non-Goals** 与菜单灰项，属于**刻意延后**，避免与多标签改造缠在一起。

---

## 2. 总览

| 方向 | 类型 | 用户价值 | 实现难度 | 依赖现状 |
|------|------|----------|----------|----------|
| **TUI 自动恢复** | 产品·阶段 2 | 断线后 `vim`/`top` 类场景可续 | 高 | 已有 Shell/TUI、恢复剧本 |
| **Jump Host / ProxyJump** | 产品·网络 | 经跳板进内网 | 中高 | 现 OpenSSH 命令行易扩展 |
| **分屏（split pane）** | 产品·布局 | 同屏看多会话 | 中 | 已有多 `SessionView` |
| **SFTP / 文件传输** | 产品·文件 | 拖拽上下传 | 高 | 新子系统 |
| **会话树 / 分组 / 配色** | 产品·组织 | 大量主机管理 | 中 | 已有 HostProfile 列表 |
| **`reconnect_enabled`** | 配置微调 | 按主机开关自动重连 | 低 | schema 已有字段 |
| **原生系统菜单** | 工程/体验 | 系统级菜单栏 | 中 | 现 HTML menubar |
| **本地 shell 标签** | 产品扩展 | 本机 PowerShell/cmd | 中 | 与 SSH 会话并列 |
| **多窗口** | 产品·窗口 | 多显示器拖拽 | 中 | Tauri 多 window |
| **密钥管理 UI** | 产品·安全 | 证书库可视化 | 中 | 现路径 + 口令弹窗 |
| **字体 / 查找 / 宏等** | 打磨 | 舒适度 | 低–中 | 菜单已占位 |
| **代码模块拆分** | 工程债 | 可维护性 | 低–中 | `main.ts` 体量大 |
| **known_hosts / 更严安全** | 安全 | 防中间人 | 中 | 现 `accept-new` 一类策略 |

---

## 3. 产品能力（按设计阶段）

### 3.1 TUI 自动恢复（阶段 2 · AppKind）

**是什么**  
重连后不仅 `cd` 回目录，还能对**白名单全屏应用**做有限恢复（例如重新跑 `top`，或记录「上次在跑 vim」）。**不能** magically 恢复未保存 buffer。

**解决什么**  
现在：断线 = 新 login shell；`top`/`vim` 画面没了，只能手重开。  
期望：运维盯盘时短暂断网，回来仍在同一类界面语境。

**和现状关系**

- 已有：xterm 真 VT、TUI 直通、per-session 恢复剧本（PTY 就绪 → `cd` → 草稿）。
- 缺：会话「应用类型」识别（AppKind）、白名单、重跑策略与安全边界。

**难点 / 风险**

- **不能**对任意命令自动重跑（危险脚本）。
- 无法恢复 vim 内存态；最多「重开命令」或提示用户。
- 要区分：用户在 shell 提示符 vs alt-screen TUI。

**建议优先级**  
若核心卖点是「Anchor 锚定」，这是产品路线图上**最贴定位**的一项；工作量中–高，**需单独设计文档**后再开工。

---

### 3.2 Jump Host / ProxyJump

**是什么**  
经跳板机进目标机，例如：本机 → bastion → 内网主机。  
OpenSSH 侧常见：`ProxyJump`、`ProxyCommand`、`-J`。

**解决什么**  
内网机不能直连、只暴露堡垒机时的日常路径（对标 Xshell / SecureCRT）。

**和现状关系**

- 交互已是系统 `ssh.exe`，**加参数/配置**比纯 russh 实现跳板更自然。
- `HostProfile` 需扩展：`jump_host` / `proxy_jump` 等；属性对话框多一组字段。
- 侧信道补全 / `openssh_exec` / 临时 key **都要走同一跳板**，否则「交互能连、补全不能」。

**难点 / 风险**

- 双重认证（跳板 + 目标）、密钥/密码组合。
- Windows OpenSSH 与配置路径细节。
- 多会话时每个 Runtime 独立跳板参数。

**建议优先级**  
对企业/运维场景**价值很高**；实现中高，建议作为「网络能力」独立 PR 串。

---

### 3.3 分屏（split pane）

**是什么**  
同一窗口内左右/上下多个终端 pane；每个 pane 仍可对应不同 `session_id`。

**解决什么**  
多 tab 需切换才能对比；分屏可同时看两台机输出。

**和现状关系**

- 已有多 `SessionView` + 独立 xterm。
- 缺：布局树、pane 焦点、拖拽分隔条、与 tab 的关系（tab 内 split vs 全局）。

**难点 / 风险**

- 焦点与快捷键（Ctrl+Tab 是 tab 还是 pane）。
- `fit`/`resize` 在复杂布局下易回归（已踩过 `display:none` 尺寸问题）。
- 产品上先定：分屏是「tab 内」还是「替代 tab」。

**建议优先级**  
体验提升明显，**纯 UI 工程量大**；在 Jump/SFTP 之前或之后均可，取决于用户更痛「连不上」还是「看不过来」。

---

### 3.4 SFTP / 文件传输

**是什么**  
图形化浏览远端目录、上传/下载、拖拽；独立面板或侧栏。

**解决什么**  
Xshell / WinSCP / MobaXterm 的传文件刚需。

**和现状关系**

- 与交互 PTY **并行**的另一条通道（SFTP subsystem 或独立 `sftp` 进程）。
- 认证可复用 HostProfile / 临时 key 思路。
- **不要**把 `rz`/`sz` 当主方案。

**难点 / 风险**

- 完整文件管理：列表、权限、进度、断点、冲突策略。
- 与多会话：每个 host 一个 SFTP 会话，生命周期与 tab 对齐。
- 工作量接近「半个产品」。

**建议优先级**  
商业对标重要，但**投入最大**之一；适合独立里程碑。

---

### 3.5 会话树 / 文件夹 / 颜色标签 / 快捷方式栏

**是什么**  
把 HostProfile 做成树（生产/测试/客户）、颜色点、一键栏，而不是扁平列表。

**解决什么**  
主机一多，会话管理器列表难扫。

**和现状关系**

- 已有：`profiles.json` + 管理器对话框。
- schema 可加：`folder`、`color`、`order`（设计中提过可选字段）。
- UI：可折叠**会话树**（不是连接表单侧栏；PR4 已去掉连接表单常驻区）。

**难点 / 风险**  
中等；主要是信息架构与持久化迁移，终端核心几乎不动。

**建议优先级**  
主机数量上来后再做性价比最高。

---

### 3.6 `HostProfile.reconnect_enabled` 生效

**是什么**  
配置里已有 `reconnect_enabled`（默认 true），**现网故意不读**。

**现在行为**

- 意外断线 → 该 session 自动重连  
- 手动断开 → 不自动重连  

**打开后**  
可对某主机关闭自动重连（例如只想手工连的生产机）。

**实现量**  
低：connect 时写入 Runtime；`finish_session` / `spawn_reconnect_loop` 读该标志；UI 勾选。

**风险**  
与阶段 1 文档不一致时要同步验收文案；故当时「勿顺手接上」。

**建议优先级**  
**小而快**的产品补丁，可插在大功能之间。

---

## 4. 体验与工程类

### 4.1 原生系统菜单（Tauri Menu）

- **现在**：HTML menubar（主题一致、中文好控）。
- **可选**：Windows 原生菜单 / 托盘。
- **利弊**：更「桌面」；与 HTML 双轨时维护成本高。
- **优先级**：偏打磨，非刚需。

### 4.2 本地 shell 标签

- tab 内开本机 PowerShell/cmd，与 SSH 并列。
- 难点：Windows ConPTY，与 OpenSSH 路径完全不同。
- 与「纯 SSH 客户端」定位略分叉。

### 4.3 多窗口（multi-window）

- 多 AnchorTerm 窗口，或 tab 拖成新窗。
- 难点：状态共享、session map、全局快捷键。
- 当前多 tab 已覆盖大部分场景。

### 4.4 密钥管理 UI

- **现在**：路径字符串 + 连接前口令弹窗 + 临时解密文件 Drop。
- **可选**：证书库列表、导入、指纹、默认密钥。
- **约束**：仍**不要**把 passphrase 明文进 `profiles.json`。

### 4.5 菜单灰项：查找 / 字体 / 宏 / 剪贴板历史

| 项 | 说明 | 难度 |
|----|------|------|
| 终端内查找 | scrollback 搜索 | 中低 |
| 字体 +/- | 改 xterm option + 持久化 | 低 |
| 宏 / 常用命令 | 插入草稿或发 PTY | 中 |
| 剪贴板历史 | 跨 tab 历史，注意隐私 | 中 |

适合作为体验小 PR 穿插。

### 4.6 工程：前端模块拆分

- **现在**：`src/main.ts` 集中 menubar、对话框、SessionView、IPC。
- **可选**：按设计拆 `session-view.ts`、`menu.ts`、`dialogs/*`、`ipc.ts` 等。
- **价值**：不增功能，利于分屏/SFTP 并行开发。
- **建议**：开大功能前先拆一轮。

### 4.7 安全加固：known_hosts

- **现在**：较宽松的 host key 策略（如 accept-new）。
- **可选**：严格 known_hosts、首次确认 UI、指纹展示。
- 对公网/高安全环境有意义。

---

## 5. 硬边界（后续开发勿破坏）

无论做哪条后续线，应继续遵守 README **会话沉淀**：

1. **交互仍走系统 OpenSSH `ssh -tt`**（除非有充分理由与 PoC）。  
2. **重连默认 = 新 login shell**；TUI 恢复只能是白名单、显式、可关。  
3. **多会话纯度**：命令/事件带 `session_id`，无全局 current_session。  
4. **disconnect ≠ close**；关 tab 才 Drop 临时 key。  
5. **cwd**：禁止侧信道 `pwd` 当交互 cwd 真相；失败 `cd` 要回滚。  
6. **密钥**：口令/密码不进 profiles.json、不进 ops_log。  
7. **锁顺序**：短持 sessions 取 Arc；勿持 cwd 锁再 snapshot/emit。  
8. **Tauri 2 IPC**：顶层参数 camelCase（`sessionId`）；`req` 内 snake_case。

---

## 6. 推荐路线（按投入）

```text
短线（约 1 个小 PR 量级）
├─ reconnect_enabled 生效
├─ 字体大小 / 终端查找
└─ main.ts 模块拆分（还债）

中线（产品可见）
├─ Jump Host（运维刚需）
├─ 会话树/分组（主机一多就痛）
└─ 分屏（对比两机）

长线（里程碑）
├─ TUI 自动恢复（贴 Anchor 定位，需专设）
└─ SFTP（工作量最大）
```

| 若目标是… | 更建议先做 |
|-----------|------------|
| 对标 Xshell 连内网 | **Jump Host** |
| 强化「断线可锚定」品牌 | **TUI 恢复（阶段 2）** |
| 主机上百台 | **会话树** |
| 少切窗口传文件 | **SFTP** |
| 先把代码养好再开大坑 | **模块拆分 + reconnect_enabled** |

---

## 7. 开工建议（给后续会话）

1. **先定一项**主方向，写短设计（问题、范围、Non-Goals、数据模型、PR 切片、验收）。  
2. 对照 [CODE-REVIEW-PLAN.md](CODE-REVIEW-PLAN.md) 为新系列建评审清单（可复制改名）。  
3. 大功能（Jump / SFTP / TUI 恢复）**不要**与无关 UI 大改同一 PR。  
4. 验收仍以真实 SSH 主机 + `操作日志/latest.log`（含 `sid=`）为准。

---

## 8. 相关文档

| 文档 | 说明 |
|------|------|
| [../README.md](../README.md) | 会话沉淀（实现真相源） |
| [../DESIGN.md](../DESIGN.md) | 产品总设计与阶段表 |
| [UI-MULTI-TAB.md](UI-MULTI-TAB.md) | 多标签设计（含 Non-Goals） |
| [ACCEPTANCE.md](ACCEPTANCE.md) | 当前验收清单 |
| [CODE-REVIEW-PLAN.md](CODE-REVIEW-PLAN.md) | PR1–PR5 评审进度 |
| [shell-integration.md](shell-integration.md) | OSC 7 |
| [DEPLOYMENT.md](DEPLOYMENT.md) | **部署与安装**（与功能路线图正交，建议优先落地） |

---

*本文为路线图参考，具体排期与取舍由产品/负责人决定。功能迭代前建议先具备可安装包（见 DEPLOYMENT.md）。*
