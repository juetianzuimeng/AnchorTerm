# AnchorTerm MCP 用户指南

让 Cursor、Claude Desktop 等 **MCP Client** 使用 AnchorTerm **已经连接** 的 SSH 会话。

| 字段 | 内容 |
|------|------|
| **版本** | PR-M5（stdio 桥 + HTTP JSON-RPC 工具） |
| **配置文件** | `%APPDATA%\AnchorTerm\mcp.json` |
| **运行时端口** | `%APPDATA%\AnchorTerm\mcp.runtime.json`（MCP 启用时自动写入） |

---

## 1. 前置条件

1. 已安装并启动 **AnchorTerm**（图形界面进程必须在运行）。
2. 系统已安装 **OpenSSH 客户端**（`ssh -V` 可用）—— 与日常 SSH 相同。
3. 在 AnchorTerm 中 **已连接** 至少一个 SSH 会话（若要用 `session_exec`）。

---

## 2. 在 AnchorTerm 中启用 MCP

1. 菜单 **工具 → MCP 服务器…**
2. 勾选 **启用 MCP 服务器**
3. （可选）修改首选端口，默认 `39201`
4. 点击 **应用**
5. 状态栏出现绿色徽章 **`MCP :端口`** 即表示 HTTP 服务在监听

安全默认：

- **默认关闭**
- 仅绑定 **`127.0.0.1`**
- 请求需 **Bearer Token**（stdio 桥会自动从 `mcp.json` 读取，无需手写进 Client）

---

## 3. 推荐接入：stdio（Cursor / Claude Desktop）

stdio 模式由 Client 拉起子进程：

```text
anchorterm.exe mcp-stdio
```

该进程 **不打开窗口**，把标准输入上的 JSON-RPC 转发到本机已运行的 HTTP MCP。

### 3.1 一键复制

在 **MCP 服务器** 对话框中点击 **复制 stdio 配置**，粘贴到 Client 的 MCP 配置文件。

示例（路径以你本机为准）：

```json
{
  "mcpServers": {
    "anchorterm": {
      "command": "C:\\Users\\你\\AppData\\Local\\AnchorTerm\\anchorterm.exe",
      "args": ["mcp-stdio"]
    }
  }
}
```

开发模式（`npm run tauri:dev`）下 `command` 可能是：

```text
C:\zengshangchun\AnchorTerm\src-tauri\target\debug\anchorterm.exe
```

对话框里的 **exe 路径** 会随当前进程自动填好。

### 3.2 Cursor

1. 打开 Cursor MCP 设置（或编辑 `mcp.json`）。
2. 粘贴上述 `mcpServers.anchorterm` 段。
3. **先** 启动 AnchorTerm 并启用 MCP，**再** 重启 Cursor 的 MCP / 窗口。
4. 在对话中让模型调用 `sessions_list` 验证。

### 3.3 Claude Desktop

编辑 Claude Desktop 配置（Windows 常见路径）：

`%APPDATA%\Claude\claude_desktop_config.json`

加入同样的 `mcpServers.anchorterm` 后重启 Claude Desktop。

### 3.4 失败时

| 现象 | 处理 |
|------|------|
| `MCP 未启用` | 在 AnchorTerm 勾选启用并应用 |
| `无法连接 … MCP HTTP 服务` | 确认主程序在跑；看状态栏 `MCP :端口`；看 `mcp.runtime.json` |
| `unauthorized` | 在对话框 **重新生成 Token** 后重启 MCP Client |
| 工具列表为空 / 无响应 | 确认用的是支持 stdio MCP 的 Client；查看 Client 日志 |

命令行自检（主程序已启用 MCP 时）：

```powershell
# 将路径换成你的 anchorterm.exe
& "C:\Path\to\anchorterm.exe" mcp-help
```

---

## 4. 备选接入：HTTP

部分 Client 支持带 Header 的 HTTP MCP：

```json
{
  "mcpServers": {
    "anchorterm": {
      "url": "http://127.0.0.1:39201/mcp",
      "headers": {
        "Authorization": "Bearer <token>"
      }
    }
  }
}
```

Token 在对话框中 **显示 / 复制**。端口以状态栏实际端口为准（冲突时可能自动 +1）。

REST 调试（PowerShell）：

```powershell
$H = @{ Authorization = "Bearer YOUR_TOKEN" }
Invoke-RestMethod http://127.0.0.1:39201/health -Headers $H
Invoke-RestMethod http://127.0.0.1:39201/tools/call -Headers $H -Method Post `
  -ContentType "application/json" `
  -Body '{"name":"sessions_list","arguments":{}}'
```

---

## 5. 可用工具

| 工具 | 作用 |
|------|------|
| `sessions_list` | 列出会话（`session_id` / host / user / state / cwd） |
| `session_get` | 单个会话快照 |
| `session_exec` | **侧信道** 执行命令（不占用交互终端输入） |
| `session_read_output` | 读取近期终端画面（与用户可见输出一致的 ring buffer） |
| `session_file_upload` | **上传**：content 小文件 或 `local_path` rsync/scp（默认**异步**） |
| `session_file_download` | **下载**：content 小文件 或 `local_path` rsync/scp（默认**异步**） |
| `session_file_transfer_status` | 查询异步传输 `job_id` 状态 |
| `session_file_transfer_cancel` | 取消异步 rsync/scp |
| `session_file_transfer_list` | 列出近期传输任务 |

### `session_exec` 语义

- 使用与 Tab 补全相同的侧信道 SSH，**不写入**交互 PTY。
- 会尽量 `cd` 到 AnchorTerm 跟踪到的工作目录。
- **不会**共享交互 shell 里未导出的环境变量、后台任务或 vim 缓冲。
- 参数：`session_id`（可省略若配置了 default）、`command`、`timeout_secs`、`cwd`。

### 文件上传 / 下载

均走**侧信道**（`ssh` stdin/stdout 或系统 `rsync`/`scp`），**不占用**交互终端。

| 场景 | 用法 | 超时 | 说明 |
|------|------|------|------|
| 小配置/脚本（≤4 MiB） | `content` + `encoding` | 1–120s | **同步**，结果里直接有字节数；**不续传** |
| **大文件 / 500MB+** | **`local_path` + rsync/scp** | 默认 **3600s**，最大 **7200s** | 默认 **`async: true`**，立即返回 `job_id`；优先 rsync |
| 同步传输（不推荐长传） | `local_path` + `"async": false` | 同上 | 工具调用会挂到传完，Client 可能先超时 |

异步流程：

1. `session_file_upload` / `session_file_download`（带 `local_path`）→ `{ job_id, status: "queued"|"running", ... }`
2. 轮询 `session_file_transfer_status`（`job_id`）直到 `succeeded` / `failed` / `cancelled`
3. 需要中止时用 `session_file_transfer_cancel`

传输使用更宽松的 keepalive（`ServerAliveInterval=30`，`CountMax=6`），避免大文件传输被误判断线。

#### 续传边界

| 情况 | 是否续传 |
|------|----------|
| 本机有 rsync，且 `prefer_rsync` 为 true（`mcp.json` 默认 true） | **同一次 job** 遇到可恢复错误时，rsync `--partial` 可接着传（`transfer_max_retries` 默认 2） |
| 无 rsync，回退 scp | **否**，整文件重传 |
| `content` 内联 | **否** |
| `session_file_transfer_cancel` 或失败后再调 upload/download | **默认否**：新 `job_id` 对应新的 staging 路径；`cleanup_on_fail` 默认还会删半截文件 |

单文件写入 `目标.anchorterm-part-<job_id>` 再 `mv`。跨任务无法对齐上次临时文件，因此**不是**「中断后再调一次从断点接着传」。断线时尽量让**同一个 job** 自动重试，不要先 cancel 再新开。

#### 原子写入 · 校验 · 失败清理

| 能力 | 默认 | 说明 |
|------|------|------|
| **原子写入** | 开 | 上传：写到 `remote.anchorterm-part-<id>` 再 `mv` 到目标；下载：写到同目录 `.anchorterm-part-<id>-name` 再 rename |
| **`verify`** | `size` | `size` 比对字节数；`sha256` 比对哈希（远端无 `sha256sum`/`shasum`/`openssl` 时回退 size）；`none` 跳过 |
| **`cleanup_on_fail`** | `true` | 失败/取消时删除 staging 临时文件，避免半截文件占坑。设 `false` 也**不能**保证下次新 job 续传（staging 文件名绑定 `job_id`） |

成功结果含 `verified` / `verify_mode` / 可选 `sha256`；异步 job 的 status 同样带这些字段，失败时还有 `cleaned_up`。

#### 传输进度（异步 job）

`session_file_transfer_status` 在 `running` 期间会更新进度（约每 **2s** 采样 staging 文件大小）：

| 字段 | 含义 |
|------|------|
| `phase` | `queued` → `preparing` → `transferring` → `verifying` → `finalizing` → `done` / `failed` / `cancelled` |
| `bytes_done` / `bytes_transferred` | 已写入 staging 的字节数 |
| `bytes_total` | 目标总大小（上传=本地文件；下载=远端 stat） |
| `percent` | 0–100（有 total 时） |
| `bytes_per_sec` | 相邻两次采样估算的吞吐 |
| `progress_source` | `remote_stage_stat`（上传）或 `local_stage_stat`（下载） |
| `updated_unix_ms` | 上次进度更新时间 |

建议轮询间隔 **2–5 秒**，避免过于频繁。进度为 best-effort：staging 尚未出现时可能短暂为 0。

#### 传输诊断日志（操作日志）

类别 **`MCP`**，在 `操作日志\` 中检索 `transfer`：

| 日志片段 | 含义 |
|----------|------|
| `transfer tool=session_file_upload/download` | 工具入口（mode / verify / sandbox / async） |
| `transfer sandbox=off/allow/deny` | 沙箱关闭或路径放行/拒绝 |
| `transfer job spawn` | 异步任务创建 |
| `transfer path begin` | path 流水线开始 |
| `transfer transport try/ok/fail=rsync\|scp` | 协议选择与结果 |
| `transfer transport retry` | 自动重试 |
| `transfer upload/download verify ok/fail` | 校验 |
| `transfer upload/download done` / `transfer job=… status=` | 结束 |
| `transfer cancel` | 取消 |

示例（REST）：

```powershell
# 小文件内联上传
Invoke-RestMethod http://127.0.0.1:39201/tools/call -Headers $H -Method Post `
  -ContentType "application/json" `
  -Body '{"name":"session_file_upload","arguments":{"remote_path":"/tmp/hello.sh","content":"#!/bin/sh\necho hi\n","create_dirs":true}}'

# 大文件异步上传（默认 async=true）
Invoke-RestMethod http://127.0.0.1:39201/tools/call -Headers $H -Method Post `
  -ContentType "application/json" `
  -Body '{"name":"session_file_upload","arguments":{"remote_path":"/data/big.bin","local_path":"C:\\\\data\\\\big.bin","create_dirs":true}}'
# → 记下 job_id，再调 session_file_transfer_status

# 小文件下载为文本
Invoke-RestMethod http://127.0.0.1:39201/tools/call -Headers $H -Method Post `
  -ContentType "application/json" `
  -Body '{"name":"session_file_download","arguments":{"remote_path":"/etc/hostname"}}'
```

### 建议工作流

1. `sessions_list`（`connected_only: true`）拿到 `session_id`
2. `session_get` 确认 cwd
3. `session_exec` 执行巡检命令
4. 小文件用 content 模式；**大文件用 `local_path` + status 轮询**
5. 需要对照人类操作时用 `session_read_output`

---

## 6. 安全说明

- Token 等同于「本机远程 shell 能力」的钥匙，勿提交到 Git 或发给不可信方。
- **重新生成 Token** 会使旧 Client 配置立刻失效。
- 可选：在 `mcp.json` 设置 `allowed_session_ids` 白名单、`default_session_id`。
- 凭证（密码/私钥）**绝不会**通过 MCP 返回。

---

## 7. 配置字段（`mcp.json`）

| 字段 | 默认 | 说明 |
|------|------|------|
| `enabled` | `false` | 总开关 |
| `bind_host` | `127.0.0.1` | 非 loopback 会被拒绝/改写 |
| `port` | `39201` | 首选端口；占用时自动尝试后续端口 |
| `token` | 自动生成 | Bearer 密钥 |
| `exec_timeout_secs` | `30` | `session_exec` 默认超时 |
| `exec_max_output_bytes` | `262144` | 输出截断上限 |
| `default_session_id` | `null` | 工具省略 sid 时的默认 |
| `allowed_session_ids` | `null` | 非 null 时仅允许列表中的会话 |
| `allow_pty_tools` | `false` | 预留；交互 PTY 工具未在 V1 开放 |
| `transfer_timeout_secs` | `3600` | path/scp 默认超时 |
| `transfer_max_timeout_secs` | `7200` | path 超时上限 |
| `transfer_progress_poll_secs` | `2` | 异步进度采样间隔 |
| `transfer_default_verify` | `size` | 默认校验：none/size/sha256 |
| `transfer_max_retries` | `2` | 可恢复错误自动重试次数 |
| `transfer_retry_backoff_secs` | `3` | 重试间隔 |
| `transfer_sandbox_enabled` | `false` | 限制 local_path 到沙箱前缀（默认关闭，需显式开启） |
| `transfer_allow_local_prefixes` | 默认 Downloads/AnchorTerm 等 | 沙箱白名单 |
| `transfer_prefer_rsync` | `true` | 优先 rsync（--partial，同 job 自动重试可续传），否则 scp |
| `transfer_max_async_jobs` | `4` | 并发异步传输上限 |

---

## 8. 相关文档

| 文档 | 说明 |
|------|------|
| [design-mcp-ssh-sessions.md](design-mcp-ssh-sessions.md) | 功能设计全文 |
| [../README.md](../README.md) | 产品总览 |
| [INSTALL.md](INSTALL.md) | 安装与依赖 |

---

*若 MCP Client 与本指南行为不一致，以 Client 文档为准；stdio 桥与 HTTP 工具面以仓库实现为准。*
