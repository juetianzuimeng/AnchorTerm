# AnchorTerm

Windows SSH 终端客户端：真终端（xterm.js）+ **多标签多主机** + 菜单化连接管理 + 密码/私钥登录 + **断线/再连恢复工作目录** + **草稿保留** + **Tab 远端补全**。

| 文档 | 说明 |
|------|------|
| [DESIGN.md](DESIGN.md) | 产品与架构设计（中文） |
| [docs/PHASE1.md](docs/PHASE1.md) | 第一阶段开工方案 |
| [docs/UI-MULTI-TAB.md](docs/UI-MULTI-TAB.md) | 多标签 / 菜单化布局功能设计（PR1–PR5） |
| [docs/ROADMAP-NEXT.md](docs/ROADMAP-NEXT.md) | **下一阶段可选方向**（Jump / 分屏 / SFTP / TUI 恢复等） |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | **部署与安装方案**（安装包 / 便携包 / CI 发版） |
| [docs/CODE-REVIEW-PLAN.md](docs/CODE-REVIEW-PLAN.md) | 分阶段代码评审方案与进度 |
| [docs/shell-integration.md](docs/shell-integration.md) | 远端 OSC 7 安装（提高 cwd 精度） |
| [docs/ACCEPTANCE.md](docs/ACCEPTANCE.md) | 阶段 1 + 多会话验收清单 |
| [操作日志/README.md](操作日志/README.md) | 诊断日志约定（含 `sid=`） |

---

## 会话沉淀（给后续 AI / 开发者）

> 以下内容来自 2026-07 人工联调通过后的实现总结（含多标签 PR1–PR5）。**新会话请先读本节再改代码。**

### 当前能力（已人工验收）

| 能力 | 状态 | 说明 |
|------|------|------|
| 私钥登录（含加密 PEM） | ✅ | 含 **DES-EDE3-CBC**；连接前可弹出口令框（**不落盘**） |
| 密码登录 | ✅ | 可弹窗补密；亦可走凭据库；SSH_ASKPASS 仅密码路径 |
| 交互 shell（cd/pwd/ls/tail…） | ✅ | 草稿发送 → 远端回显正常 |
| Tab 远端补全 | ✅ | 侧信道 `ssh host 'compgen…'`，不污染交互 PTY |
| Shell / TUI 直通 | ✅ | 默认草稿；菜单或按钮切 TUI |
| **多标签同时连多主机** | ✅ | 每 tab 独立 PTY / xterm / 草稿 / cwd / 重连 |
| 意外断线自动重连 + 恢复 cwd | ✅ | per-session `restore_target` + 交互 PTY 静默 `cd` |
| **手动断开后再连接恢复 cwd** | ✅ | 同 tab 复用 `session_id` + 同 host/user |
| 菜单 + 会话属性/管理器 | ✅ | 无常驻左侧连接栏（PR4） |
| 操作日志 | ✅ | `操作日志\`；**每次启动清空全部 `*.log`**；多会话带 **`sid=`** |

### 架构要点（必读）

```
UI menubar + tab bar + Map<sessionId, SessionView>
    ↕ Tauri IPC（命令强制 sessionId）/ events（payload 含 session_id）
AppState.sessions: HashMap<session_id, Arc<SessionRuntime>>
session/          按 sid 的状态机、重连、cwd 恢复、submit_line、complete、close_session
ssh/transport     ActiveTransport 门面
ssh/openssh       ★ 交互：系统 OpenSSH `ssh -tt`；读泵/finish 绑定 sid
ssh/key_loader    解密私钥（含 legacy DES-EDE3-CBC）→ 临时无口令 PEM
ssh/complete      Tab 补全 + 侧信道 exec
cwd/              OSC 7 + cd 解析 + **失败回滚**（bash: cd: … No such file）
ops_log/          操作日志（启动清空；禁密码明文）
```

**为何交互不用纯 russh PTY？**  
早期 russh 路径出现「`write ok` 但 shell 无回显/无提示符」。**交互会话统一走系统 OpenSSH**（`ssh -tt`）。  
russh 仍用于密钥类型/导出；**补全走 OpenSSH 侧信道**。

**多会话硬约束**

- `session_id` = 前端 UUID，**connect 前**生成并 `get_or_insert` 后再 emit。  
- 事件：`session://data` 为 `{ session_id, data_b64 }`（非裸 string）。  
- **disconnect ≠ close_session**：断开保留 Runtime/tab；关标签才 remove + Drop 临时 key。  
- 后端**无** UI 焦点 / current_session。  
- Tauri 2 命令参数顶层用 **camelCase**（`sessionId`）；`req` 内字段 snake_case。
### 认证与私钥（踩坑已修）

1. **加密私钥不能直接 `-i` + SSH_ASKPASS**  
   Windows 上 `CREATE_NO_WINDOW` 导致 askpass 失败：  
   `CreateProcessW failed error:2` / `ssh_askpass: posix_spawnp: No such file`  
   → 表现为 `Permission denied (publickey,password)`，即使用户证书有效。

2. **正确做法**（`openssh::prepare_secure_key`）  
   - `key_loader::load_private_key(path, passphrase)` 在应用内解密  
   - `PrivateKey::to_openssh` 导出**无口令** OpenSSH PEM 到临时文件  
   - `icacls` 收紧 ACL（OpenSSH 拒绝「权限过宽」的 key）  
   - `ssh -i 临时文件`，公钥认证**不再设 SSH_ASKPASS**  
   - 会话结束 Drop 删除临时 key

3. 用户证书示例路径：`C:\zengshangchun\证书\id_rsa_2048(火箭用)`（含中文与括号，Rust `fs` 可读；OpenSSH 用的是临时路径）。

### 输入模型

| 模式 | 用途 |
|------|------|
| **Shell（默认）** | 键盘进底部草稿；Enter → `submit_line` → PTY 写 `line+\r`；Tab → 远端补全 |
| **TUI 直通** | 键盘全部进 PTY（`top`/`vim`/密码提示） |
| Shell 下控制键 | Ctrl+C/D/Z 等仍转发到远端，便于中断卡住的命令 |

**不要**在 Shell 模式对发送的命令做本地 echo（会双行）；依赖远端回显。

### CWD 跟踪与恢复

- **来源**  
  1. 提交行解析 `cd`/`pushd`（乐观更新；**远端失败则回滚**）  
  2. OSC 7（需 shell-integration，最准）  
  3. 首连：侧信道 `pwd -P` 仅 seed **登录 $HOME**

- **`restore_target`**：per-session 绝对路径冻结。  
- **恢复剧本**：交互 PTY 静默 `cd`；**禁止**侧信道 `pwd` 校验交互 cwd。  
- **同 tab 再连**：复用同一 `session_id`；同 host+user → restore。  
- 失败 `cd` 不回滚会导致路径污染（如 `/home/u/tg/tg1`），日志关键字：`cd_rollback`。

### 严重 bug：Mutex 死锁（已修，勿回归）

`std::sync::Mutex` **不可重入**。持 `cwd`/`meta` 时**禁止**调用会再锁的 `snapshot()`/`emit` 路径。  
锁顺序：短持 `sessions` 取 `Arc` → 再操作 Runtime；Runtime 内先放 cwd 再 snapshot。

### Tab 补全

- `complete_draft(sessionId, …)` → 侧信道 `openssh_exec` + `compgen`  
- 不向交互 PTY 注入补全噪声  

### 操作日志

- 目录：`操作日志\`；启动清空 `*.log`  
- 优先 `latest.log`  
- 多会话：消息中带 **`sid=` 前 8 位**；事件路由失败：`event_route_miss`  
- **禁止** password / passphrase 明文  

### 后端模块地图

```
src-tauri/src/
  lib.rs              启动、ops_log、命令注册
  app_state.rs        SessionRuntime + sessions map（无 default shim）
  session/mod.rs      connect/disconnect/close_session/list_sessions/…
  ssh/openssh.rs      ssh -tt、读泵/finish 绑定 sid、临时 key
  ssh/key_loader.rs   DES-EDE3-CBC 等
  ssh/complete.rs     Tab 补全
  ssh/transport.rs    ActiveTransport
  cwd/mod.rs          OSC7 + cd + 失败回滚
  ops_log.rs          启动清空
  auth/ config/
```

前端：`src/main.ts`（menubar、对话框、`SessionView` Map、快捷键）、`index.html`、`styles.css`。

### 刻意不做 / 已知限制

- 重连 = **新 login shell**（不恢复 vim/临时 env/后台任务）  
- **不**自动重跑 TUI；**不** Jump / SFTP / 分屏 / 会话树  
- **不**持久化私钥口令；连接前弹窗  
- 忽略 `HostProfile.reconnect_enabled`（仅手动断 vs 意外断）  
- 无 OSC 7 时 cwd 主要靠 `cd` 解析 + 回滚 + restore 冻结  

---

## 环境要求

- Windows 10/11  
- Node.js 18+  
- Rust stable（`rustup default stable`）  
- Visual Studio 2022，「使用 C++ 的桌面开发」  
- Windows OpenSSH 客户端（`ssh.exe`，一般系统自带）

## 快速开始

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cd C:\zengshangchun\AnchorTerm
npm install
npm run tauri:dev
```

单元测试：

```powershell
cd src-tauri
cargo test -p anchorterm --lib
```

## 使用说明

1. **文件 → 新建会话**（或 Ctrl+N）打开属性对话框；或 **会话管理器**（Ctrl+O）打开已保存配置  
2. 填写主机 / 端口 / 用户名；私钥填本机绝对路径  
3. **加密私钥**：连接前会弹出「私钥口令」框（也可在对话框中预填）；口令**不保存到配置**  
4. 可选：保存配置；登录密码可进 Windows 凭据管理器  
5. 默认 **Shell + 草稿**；菜单 **查看 → 切换输入模式** 或标签内按钮切 TUI  
6. **多标签**：再「新建/打开」= 新 tab；`+` / Ctrl+N；Ctrl+Tab 切换；Ctrl+W 关闭（连接中会确认）  
7. **断开**保留标签与草稿；**关闭标签**才销毁会话  
8. 意外断线 → 该 tab 自动重连并尽量恢复 cwd；手动断开后同 tab 再连同账号 → 恢复 cwd  
9. 草稿 **Tab** = 远端补全  

快捷键：`Ctrl+N` 新建 · `Ctrl+O` 管理器 · `Ctrl+W` 关标签 · `Ctrl+Tab` 切标签 · `Ctrl+Shift+C/V` 复制/粘贴  

提高 cwd 精度：远端安装 OSC 7，见 [docs/shell-integration.md](docs/shell-integration.md)。

## 配置位置

| 数据 | 位置 |
|------|------|
| 主机列表 | `%APPDATA%\AnchorTerm\profiles.json`（无密码） |
| 密码 | Windows 凭据管理器，服务名 `AnchorTerm` |
| 操作日志 | `C:\zengshangchun\AnchorTerm\操作日志\` |

## 阶段 1 承诺边界

重连 / 再连接 = **新的 login shell**。保证：

1. 工作目录尽量恢复（绝对路径可知且远端仍存在）  
2. 草稿输入不丢  

**不保证：** 未保存 vim、REPL 内存、临时 export、后台任务、自动重跑脚本。

## 技术栈

- Tauri 2 + Vite + TypeScript  
- xterm.js + FitAddon  
- 交互传输：系统 **OpenSSH**（`ssh -tt`）  
- 密钥：russh / ssh-key（`ring` 后端，避免 Windows NASM/aws-lc）  
- keyring（系统凭据库）  
- 自研 DES-EDE3-CBC PEM 解密（`des` + `cbc` + `md-5`）

## 联调检查清单（回归）

- [ ] 加密私钥 + 连接前口令弹窗可登录  
- [ ] `cd` → `pwd` → `ls` 有远端回显  
- [ ] 故意 `cd` 失败后再 `cd` 正确目录（无路径污染）  
- [ ] Tab 补全  
- [ ] 断网自动重连 + 手动断再连恢复 cwd  
- [ ] 双 tab 不同目录互不串流；关标签 A 不影响 B  
- [ ] 无左侧连接栏；菜单/Ctrl+N/W/Tab 可用  
- [ ] 重启后 `操作日志` 仅有本次日志；含 `sid=`  
- [ ] 连续 `cd` 无「第一条后全哑」（死锁回归）  

完整清单见 [docs/ACCEPTANCE.md](docs/ACCEPTANCE.md)。评审记录见 `docs/reviews/`。
