# 操作日志目录

AnchorTerm 会把排查用日志写到本目录（应用启动时自动创建）。

## 重要：重启即清空

**每次应用启动都会删除本目录下全部 `*.log` 文件**（保留本 README），再写入新的本次运行日志。  
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
| `SYS` | 应用启停、日志路径 |
| `UI` | 界面点击、模式切换、菜单/对话框、错误展示 |
| `CMD` | 草稿命令发送（输入内容） |
| `SSH` | 连接、进程、stdin 写入、临时密钥 prepare/remove |
| `ECHO` | 远端回显字节（文本预览 + hex） |
| `STATE` | 连接状态机变化 |
| `CWD` | 工作目录跟踪（含 `from_cd_parse` / `cd_rollback` / restore） |
| `ERR` | 错误（含 `event_route_miss`） |

## 多会话与 `sid=`

- 多标签下日志常带 **`sid=` 前 8 位**（完整 UUID 的前缀），用于区分 tab。  
- 前端事件若无法匹配 map：`event_route_miss`。  
- 关标签成功：`close_session removed sid=…`。  
- 连接前口令弹窗：`prompt_passphrase_before_connect` / `prompt_password_before_connect`（**不**记录口令内容）。

## 注意

- **不会**记录密码 / passphrase 明文。  
- 分析问题时优先把 **`latest.log`** 整份发出来即可。  
- 若终端命令“卡住”，可先点一下终端区域再按 **Ctrl+C** 尝试中断（shell 模式也会转发该控制键）。
