# 操作日志目录

AnchorTerm 会把排查用日志写到约定目录（应用启动时自动创建并清空旧 `*.log`）。

## 日志目录如何选择（阶段 B）

优先级：

| 优先级 | 条件 | 目录 |
|--------|------|------|
| 1 | 环境变量 `ANCHORTERM_LOG_DIR` 非空 | 该路径 |
| 2 | 从源码树运行（可执行文件在仓库 `target/…` 下） | 仓库内 **`操作日志\`**（本目录） |
| 3 | 安装版 / 便携版 | **`%APPDATA%\AnchorTerm\logs\`** |

应用内菜单 **工具 → 打开操作日志目录** 会打开**实际解析到的**路径（不硬编码本仓库）。

覆盖示例（PowerShell）：

```powershell
$env:ANCHORTERM_LOG_DIR = "D:\tmp\anchorterm-logs"
```

## 重要：重启即清空

**每次应用启动都会删除当前日志目录下全部 `*.log` 文件**（保留本 README 等非 log 文件），再写入新的本次运行日志。  
因此分析问题时，请直接看当前的 `latest.log`，其中只包含**最后一次启动**后的操作。

## 文件

| 文件 | 说明 |
|------|------|
| `latest.log` | **本次启动**的完整日志 |
| `session-YYYYMMDD-HHMMSS.log` | 同一次启动的独立副本 |
| `ops-YYYY-MM-DD.log` | 本次启动按日命名的副本（启动时旧文件已删） |

## 类别标签

| 标签 | 含义 |
|------|------|
| `SYS` | 应用启停、日志路径、ssh 检测 |
| `UI` | 界面点击、模式切换、菜单/对话框、错误展示 |
| `CMD` | 草稿命令发送（输入内容） |
| `SSH` | 连接、进程、stdin 写入、临时密钥 prepare/remove |
| `ECHO` | 远端回显字节（文本预览 + hex） |
| `SEP` | **命令后分割线** 诊断（提交 → 回显过滤 → 泵数据 → UI 接收） |
| `STATE` | 连接状态机变化；**备用屏** `alt_screen_enter` / `alt_screen_exit` |
| `CWD` | 工作目录跟踪（含 `from_cd_parse` / `cd_rollback` / restore） |
| `ERR` | 错误（含 `event_route_miss`） |

### 命令后分割线（SEP）排查

开启「查看 → 命令后显示分割线」后执行会卡住时，在 `latest.log` 里搜 **`[SEP]`**：

| 日志关键字 | 含义 |
|-----------|------|
| `ui_sep_begin` / `begin gen=` | 前端/后端开始跟踪本次带分割线的提交 |
| `suppress_arm` | 已武装行回显过滤（去掉 `;printf…` 后缀） |
| `suppress_match_disarm` | 回显已匹配并**立即解除**过滤（大输出不应再走 filter） |
| `suppress_ttl_flush` | 3s 内未匹配到回显，TTL 到期冲刷 residual |
| `suppress_pass` | 过滤仍在、尚未匹配（若大量出现说明回显形态与 pattern 不一致） |
| `pump_chunk` | stdout 泵读到数据；含 `on_data_ms`（处理耗时） |
| `emit` / `emit_slow` | 已向 UI 推送；`emit_ms` 过大说明事件层阻塞 |
| `on_data_empty_after_*` | 过滤/改写后为空被丢弃 |
| `rewrite_marker` / `marker_injected_done` | 已把 OSC 标记换成绿色分割线（命令真正结束） |
| `ui_receive_progress` | 前端收到数据累计 |
| `ui_term_write_slow` | xterm 写入偏慢 |
| `pump_eof_while_pending` | 命令未结束标记就断流 |

正常大 `tail` 期望顺序大致为：`begin` → `suppress_arm` → `suppress_match_disarm` → 多条 `pump_chunk`/`ui_receive_progress` → `rewrite_marker` → `marker_injected_done`。  
若停在 `suppress_pass` 或 `pump_chunk` 后无后续，把该段 `[SEP]` 日志发出来即可。

**注意**：`session://data` 热路径上禁止同步 `ops_log` IPC（会用 `opsLogDeferred`）。若日志里 `post_separator:false`，说明本次未开启分割线，卡死应归类为「大输出路径」而非 SEP。

### 全屏 TUI / 备用屏相关日志

远端 `vim` / `htop` / `less` 等会切换 **alternate screen**（CSI `?1049h` / `?1047h` / `?47h`）。前端检测后锁定 Shell 草稿发送。

| 日志 | 含义 |
|------|------|
| `STATE` `alt_screen_enter` | 检测到进入备用屏；`reason` 为 `csi_private_mode` 或 `xterm_buffer_change` |
| `STATE` `alt_screen_exit` | 退出备用屏或会话重置（`reason=session_state_reset`） |
| `STATE` `editor_submode` | vim 子模式变化：`normal` / `insert` / `replace`（含 `from`/`to`/`reason`） |
| `UI` `editor_key_ins` | 本地按下 Ins 键 |
| `UI` `alt_screen_ui_shown` / `alt_screen_ui_cleared` | 底部黄条 / 状态徽章已刷新 |
| `CMD` `draft_send_blocked_alt_screen` | 用户在备用屏期间尝试 Shell 发送，已被拦截 |
| `CMD` `flush_blocked_alt_screen` | 内部 flush 路径同样拦截 |
| `UI` `draft_send_attempt` | 现含字段 `alt_screen` / `alt_seq` |
| `UI` `input_mode_toggle` | 现含 `alt_screen` / `from` |

排查「直通下 vi 无响应 / 命令打进编辑器」时，按时间搜上述关键字即可。

## 多会话与 `sid=`

- 多标签下日志常带 **`sid=` 前 8 位**（完整 UUID 的前缀），用于区分 tab。  
- 前端事件若无法匹配 map：`event_route_miss`。  
- 关标签成功：`close_session removed sid=…`。  
- 连接前口令弹窗：`prompt_passphrase_before_connect` / `prompt_password_before_connect`（**不**记录口令内容）。

## 注意

- **不会**记录密码 / passphrase 明文。  
- 分析问题时优先把 **`latest.log`** 整份发出来即可。  
- 若终端命令“卡住”，可先点一下终端区域再按 **Ctrl+C** 尝试中断（shell 模式也会转发该控制键）。
