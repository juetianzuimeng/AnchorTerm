# AnchorTerm

Windows SSH 终端客户端：真终端（xterm.js）+ 密码/私钥登录 + **自动/手动重连后恢复工作目录** + **草稿保留** + **Tab 远端补全**。

| 文档 | 说明 |
|------|------|
| [DESIGN.md](DESIGN.md) | 产品与架构设计（中文） |
| [docs/PHASE1.md](docs/PHASE1.md) | 第一阶段开工方案 |
| [docs/shell-integration.md](docs/shell-integration.md) | 远端 OSC 7 安装（提高 cwd 精度） |
| [docs/ACCEPTANCE.md](docs/ACCEPTANCE.md) | 验收清单 |
| [操作日志/README.md](操作日志/README.md) | 诊断日志约定 |

---

## 会话沉淀（给后续 AI / 开发者）

> 以下内容来自 2026-07 人工联调通过后的实现总结。**新会话请先读本节再改代码。**

### 当前能力（已人工验收）

| 能力 | 状态 | 说明 |
|------|------|------|
| 私钥登录（含加密 PEM） | ✅ | 含 **DES-EDE3-CBC** 旧式 OpenSSL PEM；Xshell 能用的证书本应用也能用 |
| 密码登录 | ✅ | 走 SSH_ASKPASS（无窗口进程下可能不稳；公钥路径已不依赖 askpass） |
| 交互 shell（cd/pwd/ls/tail…） | ✅ | 草稿发送 → 远端回显正常 |
| Tab 远端补全 | ✅ | 侧信道 `ssh host 'compgen…'`，不污染交互 PTY |
| Shell / TUI 直通 | ✅ | 默认草稿模式；`top`/`vim` 切 TUI 直通 |
| 意外断线自动重连 + 恢复 cwd | ✅ | `restore_target` + 交互 PTY 静默 `cd` |
| **手动断开后再连接恢复 cwd** | ✅ | 同一 host+user 时恢复断线前绝对路径 |
| 操作日志 | ✅ | `操作日志\`；**每次启动清空全部 `*.log`** |

### 架构要点（必读）

```
UI (xterm.js + 草稿框)
    ↕ Tauri IPC / events
session/          连接状态机、重连、cwd 恢复剧本、submit_line、complete_draft
ssh/transport     ActiveTransport 门面
ssh/openssh       ★ 交互会话：系统 OpenSSH `ssh -tt`（Windows 管道）
ssh/key_loader    解密私钥（含 legacy DES-EDE3-CBC）→ 内存 PrivateKey
ssh/complete      Tab 补全逻辑 + 侧信道 exec 脚本
cwd/              OSC 7 + cd 行解析
ops_log/          操作日志（启动清空）
```

**为何交互不用纯 russh PTY？**  
早期 russh 路径出现「`write ok` 但 shell 无回显/无提示符」类问题。当前 **交互会话统一走系统 OpenSSH**（`C:\Windows\System32\OpenSSH\ssh.exe` 等），`-tt` 强制远端 PTY。  
russh 仍用于密钥类型/导出（`PrivateKey::to_openssh`）及历史 complete 辅助代码；**补全已改为 OpenSSH 侧信道**。

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
  1. 提交行解析 `cd`/`pushd`（乐观更新 UI）  
  2. OSC 7（需远端 shell-integration，最准）  
  3. 首连：侧信道 `ssh … pwd -P` 仅用于 seed **登录 $HOME**（不能代表交互 PTY 的 cwd）

- **`restore_target`**  
  意外断开 / 手动断开时冻结**绝对路径**。

- **恢复剧本** `run_restore_playbook`  
  交互 PTY 发送一次 `cd -- 'path'\r`（可选 OSC 7 校验后重试一次）。  
  **禁止**用侧信道 `exec pwd` 校验交互 cwd（新进程永远在 $HOME）。

- **手动断开 → 再连接**  
  若 **host + username 与上次相同** 且有绝对 `restore_target`，走恢复剧本；否则清空 cwd、seed 登录目录。

### 严重 bug：Mutex 死锁（已修，勿回归）

`std::sync::Mutex` **不可重入**。

错误写法（会导致：**第一条 `cd` 成功，之后 `pwd`/`ls` 无任何 ECHO**）：

```text
持有 cwd 锁 → emit_state() → snapshot() → 再次 cwd.lock() → 死锁
→ stdout 读泵卡在 on_data → 后续命令写成功但无回显
```

正确：先更新并 **释放** `cwd`，再 `emit` / `snapshot()`。  
`AppState::snapshot` 注释中已标明锁顺序。

### Tab 补全

- 入口：`complete_draft` → `remote_complete_with_exec`  
- 执行：缓存凭据 + `openssh_exec` 跑 base64 包裹的 bash 脚本（`compgen`）  
- 不向交互 PTY 注入补全噪声  
- 失败时看日志：`complete_draft` / `complete side-channel exec`

### 操作日志

- 目录：`C:\zengshangchun\AnchorTerm\操作日志\`  
- **每次应用启动删除目录内全部 `*.log`**（保留 README.md），只保留本次运行  
- 优先分析 `latest.log`  
- 标签：`SYS` `UI` `CMD` `SSH` `ECHO` `STATE` `CWD` `ERR`  
- **禁止**记录 password / passphrase 明文  

### 后端模块地图

```
src-tauri/src/
  lib.rs              启动、ops_log::init、命令注册
  session/mod.rs      connect/disconnect/submit_line/complete/resize/重连/恢复
  ssh/openssh.rs      ssh -tt 交互、密钥导出、askpass(密码)、openssh_exec
  ssh/key_loader.rs   DES-EDE3-CBC 等私钥加载
  ssh/complete.rs     Tab 补全纯逻辑 + 远端脚本
  ssh/transport.rs    ActiveTransport、write_stdin
  cwd/mod.rs          OSC7 + cd 解析
  ops_log.rs          日志（启动清空）
  app_state.rs        状态、snapshot（勿持 cwd 锁调用）
  auth/               AuthMethod、keyring 密码
  config/             HostProfile
```

前端：`src/main.ts`（xterm、草稿、模式切换、事件）、`index.html`、`styles.css`。

### 刻意不做 / 已知限制

- 重连 = **新 login shell**：不恢复 vim 未保存内容、临时 env、后台任务  
- 阶段 1 **不**自动重跑 TUI 应用  
- 密码 + SSH_ASKPASS 在无控制台进程下仍可能脆弱；优先公钥  
- 远端未装 OSC 7 时，cwd 主要靠 `cd` 解析 + 恢复路径冻结  

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

1. 填写主机、端口、用户名  
2. **私钥**：填本机绝对路径；**加密私钥必须填 passphrase**（与 Xshell 相同）  
3. 可选：保存 HostProfile / 密码进 Windows 凭据管理器  
4. 连接后默认 **Shell + 草稿**；`top`/`vim` 点 **TUI 直通**  
5. 草稿 **Tab** = 远端补全（需已连接且 cwd 可知更佳）  
6. 意外断线 → 自动重连并尽量 `cd` 回原目录  
7. **手动断开后再连同一账号** → 同样恢复工作目录  

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

- [ ] 加密私钥 + passphrase 可登录  
- [ ] `cd tg1` → `pwd` → `ls` 均有远端回显  
- [ ] `cd lo` + Tab 可补全到 `logs/` 等  
- [ ] 断网/杀 ssh 后自动重连并回到原目录  
- [ ] 手动断开 → 再连接同一 host/user → `pwd` 仍为断线前目录  
- [ ] 重启应用后 `操作日志` 仅有本次日志  
- [ ] 连续 `cd` 后仍可继续执行命令（无「第一条后全哑」——死锁回归）
