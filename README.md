# AnchorTerm

**面向 Windows 的 SSH 终端客户端**，专注一件事：

> **SSH 总会断——断线之后，尽量保住「你在哪、你准备打什么」。**

它提供真终端（xterm.js）、多标签多主机、自动重连、**工作目录锚定恢复**、草稿保留与远端 Tab 补全。  
但它 **不是** 又一个「功能大而全的远程桌面式终端套件」，也 **不是** 系统自带 OpenSSH 的简单图形壳。

---

## 定位差异：和别的终端工具有什么不同？

市面上终端很多，但侧重点不同。AnchorTerm 的差异可以概括为：

| 类型 | 代表 | 它们强在哪 | AnchorTerm 的取舍 |
|------|------|------------|-------------------|
| **系统终端 + OpenSSH** | Windows Terminal、`ssh.exe` | 轻、标准、随系统 | 连接即会话：断线后上下文基本清空；无「草稿 / 按 host 记目录」等客户端层能力 |
| **全能商业 SSH 客户端** | Xshell、SecureCRT、MobaXterm | 会话树、SFTP、隧道、宏、串口等一站式 | AnchorTerm **刻意不做** Jump/SFTP/分屏全家桶，把复杂度压在 **重连与上下文锚定** |
| **跨平台云终端** | Termius 等 | 同步、多端、协作 | 本地优先、Windows 深耕；不绑账号同步；密钥/密码走本机凭据与本地配置 |
| **通用 IDE 内置终端** | VS Code Remote 等 | 与工程编辑一体 | 独立 SSH 客户端，适合「纯运维 / 多机巡检 / 服务器 shell」场景 |

**一句话对比：**

- 别人比的是「能连多少协议、面板多丰富」  
- AnchorTerm 比的是：**网络不稳时，你是否还站在原来的目录前、未发送的命令还在不在**

因此核心能力不是「更多菜单项」，而是：

1. **意外断线 → 自动重连**（网络失败继续重试）  
2. **重连后尽量静默回到上次工作目录**（含关 tab / 再连同一 host+用户）  
3. **草稿与连接生命周期解耦**（打到一半不会跟 socket 一起死）  
4. **真 VT 终端**（需要时仍可 TUI 直通跑 `vim` / `htop`）

> **Anchor** = 断线后把你「锚定」回工作上下文；  
> **Term** = 真正的终端模拟器，不是命令日志回放器。

---

## 适合什么场景？

### 很适合

- **网络不稳定的日常 SSH**：笔记本 Wi‑Fi、合盖唤醒、公司 VPN 抖动、机房链路偶发抖动  
- **长期泡在某台机器的固定目录**：例如一直在 `/data/app/logs` 看日志、改配置；断线后不想每次 `cd` 回去  
- **运维多主机并行**：多标签连测试 / 预发 / 生产，每台独立草稿与目录记忆  
- **Windows 上用加密私钥登录 Linux**：不想被 askpass / 无窗口 OpenSSH 坑  
- **命令以 shell 为主、偶尔开 TUI**：默认草稿模式高效输入；需要时一切换直通  

### 不太适合（请先看清边界）

- 需要 **SFTP 拖文件、端口转发面板、跳板机向导** 的「全家桶」远程工作站 → 现阶段请用 Xshell / Moba 等  
- 需要 **断线后原样恢复 vim 未保存内容、后台任务、临时环境变量** → 重连是 **新 login shell**，只保证目录与草稿级上下文  
- 需要 **macOS / Linux 一等公民客户端** → 当前主目标是 **Windows**  
- 只想要系统自带极简 `ssh`、零额外概念 → 直接用 Windows Terminal 即可  

---

## 目录

- [定位差异：和别的终端工具有什么不同？](#定位差异和别的终端工具有什么不同)
- [适合什么场景？](#适合什么场景)
- [核心痛点（展开）](#核心痛点展开)
- [特性](#特性)
- [截图](#截图)
- [快速开始](#快速开始)
- [使用说明](#使用说明)
- [从源码构建](#从源码构建)
- [架构概览](#架构概览)
- [配置与数据](#配置与数据)
- [已知限制](#已知限制)
- [文档](#文档)
- [开发与贡献](#开发与贡献)
- [路线图](#路线图)
- [致谢](#致谢)
- [许可证](#许可证)

---

## 核心痛点（展开）

日常用 Xshell / SecureCRT / Windows Terminal + OpenSSH 时，最烦的往往不是「能不能连上」，而是 **连上之后会断、断了之后很惨**：

| 痛点 | 常见现象 | AnchorTerm 的回应 |
|------|----------|-------------------|
| **断线 = 丢失上下文** | Wi‑Fi 闪断、笔记本合盖、机房抖动 → 会话没了，回到登录目录 | 意外断线 **自动重连**，并尽量 **静默 `cd` 回上次工作目录** |
| **草稿跟着 socket 一起死** | 命令打了一半还没 Enter，断线后全没了 | **草稿框与连接解耦**，重连后草稿还在，可改完再发 |
| **关标签 = 一切归零** | 关掉 tab 再连同一台机器，目录与习惯都要从头来 | 按 **host + 用户** 记忆 **上次 cwd** 与 **命令历史**，再连可恢复 |
| **重连噪音刷屏** | Timeout、MOTD、欢迎横幅反复刷终端 | 重连过程 **静默** 超时与登录 banner，状态在 UI 上提示 |
| **加密私钥在 Windows 上难用** | 口令私钥 + 无窗口 OpenSSH 导致 askpass 失败 | 应用内解密后写 **临时受保护 PEM**，会话结束自动删除 |
| **多主机切换成本高** | 多个窗口/会话互相干扰 | **多标签**，每 tab 独立 PTY、草稿、cwd、重连状态 |

---

## 特性

### 连接与认证

- 密码登录（可选写入 **Windows 凭据管理器**，配置文件不存明文密码）
- OpenSSH 私钥登录（含 **加密 PEM / DES-EDE3-CBC** 等常见格式）
- 私钥口令：**连接前弹窗**，不写入 `profiles.json`
- 依赖系统 **OpenSSH 客户端**（`ssh.exe`）做交互会话

### 终端与输入

- **xterm.js** 真终端（支持 `top` / `vim` 等全屏程序）
- **Shell 模式**（默认）：底部草稿输入，Enter 发送；Tab 远端补全；↑↓ 本地历史
- **TUI 直通**：按键直接进远端 PTY，适合交互式程序
- 界面内提供模式说明（模式按钮旁 **?**）

### 会话韧性（核心能力）

- 意外断线：**自动重连**（指数退避）；网络类失败持续重试，真实认证失败停止
- 重连后：**尽量恢复绝对工作目录**（路径仍存在时）
- 手动断开后再连（同 host + 用户）：同样可恢复目录
- **关闭标签 / 重启应用后再连同一 host + 用户**：从本地记忆恢复上次目录
- 重连时抑制 Timeout / MOTD 刷屏
- 断线期间 **草稿不丢**

### 多会话 UI

- 多标签同时连接多台主机
- 菜单化布局（新建会话、会话管理器、属性、操作日志目录）
- 无常驻左侧连接栏，终端区域更大

### 诊断

- 操作日志：开发环境写仓库 `操作日志\`，安装/便携版写 `%APPDATA%\AnchorTerm\logs\`
- 菜单 **打开操作日志目录** 可直接打开资源管理器

---

## 截图

> 欢迎贡献截图 PR。可在此放置：主界面多标签、Shell 草稿、重连遮罩等。

```
（截图待补充）
```

---

## 快速开始

### 最终用户（推荐）

无需安装 Node / Rust。使用发布包即可：

1. 下载 `AnchorTerm-*-setup.exe`（或便携版 zip）
2. 安装或解压后启动
3. 系统需具备：
   - **OpenSSH 客户端**（PowerShell 中执行 `ssh -V` 可检查）
   - **WebView2**（Win10/11 一般已自带）

详细说明见 **[docs/INSTALL.md](docs/INSTALL.md)**。  
发布者打包流程见 **[docs/RELEASE.md](docs/RELEASE.md)**。

### 从源码跑起来

```powershell
# 环境：Windows 10/11 · Node.js 18+ · Rust stable · VS2022 C++ 桌面开发 · 系统 OpenSSH
git clone <本仓库 URL>
cd AnchorTerm
npm install
npm run tauri:dev
```

---

## 使用说明

1. **文件 → 新建会话**（`Ctrl+N`），或 **会话管理器**（`Ctrl+O`）打开已保存主机  
2. 填写主机 / 端口 / 用户名；私钥请填本机绝对路径  
3. 加密私钥：连接前输入口令（**不会**写入主机配置文件）  
4. 默认 **Shell + 草稿**：在底部输入命令，Enter 发送，Tab 补全，↑↓ 历史  
5. 需要 `vim` / `htop` 等：点 **Shell 模式 / TUI 直通** 切换（旁有 **?** 说明）  
6. **断开**：保留标签与草稿；**关闭标签**：销毁会话（cwd 会按 host+用户记住）  
7. 意外断线：该标签自动重连并尽量 `cd` 回原目录  

**快捷键：** `Ctrl+N` 新建 · `Ctrl+O` 管理器 · `Ctrl+W` 关标签 · `Ctrl+Tab` 切标签 · `Ctrl+Shift+C/V` 复制/粘贴  

提高 cwd 精度（推荐）：远端安装 OSC 7 shell integration，见 [docs/shell-integration.md](docs/shell-integration.md)。

---

## 从源码构建

### 环境要求

| 依赖 | 说明 |
|------|------|
| Windows 10/11 | 当前主目标平台 |
| Node.js 18+ | 前端构建 |
| Rust stable | `rustup default stable` |
| VS 2022 | 「使用 C++ 的桌面开发」工作负载 |
| OpenSSH 客户端 | `C:\Windows\System32\OpenSSH\ssh.exe` 等 |

### 常用命令

```powershell
# 开发
npm run tauri:dev

# 单元测试（Rust）
cd src-tauri
cargo test -p anchorterm --lib

# 安装包（NSIS）
npm run tauri:build
# 产物：src-tauri\target\release\bundle\nsis\*_x64-setup.exe

# 便携包
npm run pack:portable
# 或：npm run release:win
# 产物：dist-release\AnchorTerm-*-windows-x64-portable.zip
```

---

## 架构概览

```
┌─────────────────────────────────────────────────────────┐
│  前端  menubar + tab bar + Map<sessionId, SessionView>   │
│        xterm.js · 草稿 · 历史 · 补全 UI                   │
└───────────────────────┬─────────────────────────────────┘
                        │ Tauri IPC / Events（强制 session_id）
┌───────────────────────▼─────────────────────────────────┐
│  AppState.sessions: HashMap<sid, SessionRuntime>         │
│  session/   连接状态机 · 重连 · cwd 恢复剧本 · submit     │
│  ssh/       交互：系统 OpenSSH `ssh -tt`（按 sid 泵数据） │
│  cwd/       OSC 7 + cd 解析 + 失败回滚                    │
│  config/    主机配置 · last_cwd 持久化                     │
│  ops_log/   诊断日志（启动清空 *.log，禁密码明文）         │
└─────────────────────────────────────────────────────────┘
```

**为何交互层用系统 OpenSSH，而不是纯库实现 PTY？**  
早期纯 russh 路径曾出现「写入成功但无 shell 回显」等问题；**交互会话统一走 `ssh -tt`**，与系统工具链行为一致。密钥加载/导出仍用 russh 相关能力；Tab 补全走 **侧信道 exec**，避免污染交互 PTY。

更细的设计见 [DESIGN.md](DESIGN.md)、[docs/UI-MULTI-TAB.md](docs/UI-MULTI-TAB.md)。

---

## 配置与数据

| 数据 | 位置 | 说明 |
|------|------|------|
| 主机列表 | `%APPDATA%\AnchorTerm\profiles.json` | **无**明文密码 / 私钥口令 |
| 登录密码 | Windows 凭据管理器（服务名 `AnchorTerm`） | 用户可选保存 |
| 上次工作目录 | `%APPDATA%\AnchorTerm\last_cwd.json` | 按 `用户@主机` 记忆 |
| 草稿命令历史 | 前端 `localStorage` | 按 host+用户，关 tab 后仍可用 |
| 操作日志（开发） | 仓库 `操作日志\` | 每次启动清空 `*.log` |
| 操作日志（安装/便携） | `%APPDATA%\AnchorTerm\logs\` | 可用环境变量 `ANCHORTERM_LOG_DIR` 覆盖 |

---

## 已知限制

请在提 Issue 前了解边界，避免预期错位：

1. **重连 = 新的 login shell**  
   保证尽量恢复 **工作目录** 与 **草稿**；  
   **不保证**：未保存的 vim 缓冲、REPL 内存、临时 `export`、后台任务、自动重跑脚本。
2. **主机密钥**：当前实现偏向易用（known_hosts 策略仍在完善中，生产环境请注意安全风险）。
3. **主平台为 Windows**；其他 OS 未作为一等目标。
4. 阶段目标内 **不做** Jump Host / SFTP / 分屏 / 会话树（见路线图）。

验收清单：[docs/ACCEPTANCE.md](docs/ACCEPTANCE.md)。

---

## 文档

| 文档 | 说明 |
|------|------|
| [docs/INSTALL.md](docs/INSTALL.md) | 最终用户安装与排错 |
| [docs/RELEASE.md](docs/RELEASE.md) | 发版与产物 |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | 部署方案（便携 / 安装 / CI） |
| [DESIGN.md](DESIGN.md) | 产品与架构设计 |
| [docs/PHASE1.md](docs/PHASE1.md) | 第一阶段方案 |
| [docs/UI-MULTI-TAB.md](docs/UI-MULTI-TAB.md) | 多标签与菜单 UI |
| [docs/ROADMAP-NEXT.md](docs/ROADMAP-NEXT.md) | 后续可选方向 |
| [docs/mcp-user-guide.md](docs/mcp-user-guide.md) | **MCP**：AI 工具复用已连接 SSH 会话 |
| [docs/shell-integration.md](docs/shell-integration.md) | 远端 OSC 7 安装 |
| [docs/ACCEPTANCE.md](docs/ACCEPTANCE.md) | 验收清单 |
| [操作日志/README.md](操作日志/README.md) | 诊断日志约定 |

---

## 开发与贡献

欢迎 Issue / PR。建议流程：

1. Fork 并创建分支  
2. 本地 `npm run tauri:dev` 验证  
3. 涉及 Rust 逻辑时运行 `cargo test -p anchorterm --lib`  
4. 按 [docs/ACCEPTANCE.md](docs/ACCEPTANCE.md) 做最小回归（连接、cd、重连、多 tab）  
5. PR 说明：**改了什么、为什么、如何验证**

### 给贡献者的实现备忘（易踩坑）

- `session_id` 由前端生成 UUID，**connect 前**插入后端 map 再发事件  
- 事件 payload 带 `session_id`；后端 **无**「当前焦点会话」全局单例  
- `disconnect` ≠ `close_session`：断开保留 Runtime；关标签才销毁并删临时 key  
- `std::sync::Mutex` **不可重入**：持锁时禁止再调会二次加锁的 `snapshot()` 路径  
- 操作日志 **禁止** 密码 / passphrase 明文  

更完整的模块说明与历史坑位见仓库内设计文档与 `docs/reviews/`。

---

## 路线图

已完成（阶段 1 主线）：

- [x] 密码 / 私钥登录与多标签  
- [x] 自动重连 + cwd 恢复 + 草稿  
- [x] Tab 远端补全、Shell / TUI 模式  
- [x] host+用户级 cwd / 命令历史记忆  
- [x] Windows 安装包 / 便携包链路  
- [x] **MCP**：复用已连接会话（HTTP 工具 + `mcp-stdio` 桥，见 [docs/mcp-user-guide.md](docs/mcp-user-guide.md)）  

规划中 / 可选（详见 [docs/ROADMAP-NEXT.md](docs/ROADMAP-NEXT.md)）：

- [ ] known_hosts 策略与主机密钥管理  
- [ ] Jump Host / ProxyJump  
- [ ] SFTP / 文件传输  
- [ ] 分屏、会话树  
- [ ] 更完善的跨平台  

---

## 致谢

- [Tauri](https://tauri.app/) — 桌面壳与 IPC  
- [xterm.js](https://xtermjs.org/) — 终端模拟  
- [OpenSSH](https://www.openssh.com/) — 交互传输  
- [russh](https://github.com/Eugeny/russh) — 密钥相关能力  
- 以及 Xshell、SecureCRT、Termius 等产品在交互体验上的启发  

---

## 许可证

本项目采用 [MIT License](LICENSE)。

```
Copyright (c) 2026 zengshangchun
```

你可以自由使用、修改、分发本软件，详见根目录 [LICENSE](LICENSE) 全文。

---

**AnchorTerm** — 让 SSH 断线之后，你还站在原来的目录前。
