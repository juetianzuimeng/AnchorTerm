# AnchorTerm 验收清单

> 在真实 Linux SSH 主机上勾选。密码/私钥请使用测试账号。  
> **多会话 UI**（菜单 / 多 tab）见下文 MS-\*；单会话能力仍适用 **active 标签**。

## 认证

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| AUTH-1 | 密码正确连接 | 进入 shell | ☐ |
| AUTH-2 | 密码错误 | 失败提示，不崩溃 | ☐ |
| AUTH-3 | ed25519/RSA 私钥无口令 | 登录成功 | ☐ |
| AUTH-4 | 加密私钥 + 正确 passphrase（连接前弹窗或表单） | 登录成功 | ☐ |
| AUTH-5 | 加密私钥 + 错误 passphrase | 失败提示 | ☐ |
| AUTH-6 | 会话管理器「打开」加密证书配置 | **先弹口令框**再连接；口令不进 profiles.json | ☐ |

## 终端

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| TERM-1 | `ls` / 中文文件名 | 显示正常 | ☐ |
| TERM-2 | 运行 `top` 再 `q` | 全屏刷新与退出正常 | ☐ |
| TERM-3 | 调整窗口大小 | 远端 `stty size` 与 UI 大致一致 | ☐ |

## 草稿

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| DRAFT-1 | 草稿输入未发送 → 断网 → 重连 | 草稿文本仍在 | ☐ |
| DRAFT-2 | 重连中改草稿 → 恢复后 Enter | 发送修改后内容 | ☐ |

## CWD

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| CWD-1 | `cd /tmp` → 断网重连 → `pwd` | `/tmp`（路径存在时） | ☐ |
| CWD-2 | `cd` 到含空格路径 → 重连 | `pwd` 正确 | ☐ |
| CWD-3 | 无 cwd 信息时重连 | 不乱发 `cd` | ☐ |
| CWD-4 | 中文路径目录 `cd` 后重连 | `pwd` 正确 | ☐ |
| CWD-5 | 目录已删除后重连 | 不崩溃；提示不可用并可回登录目录 | ☐ |
| CWD-6 | 故意 `cd` 不存在路径后再 `cd` 正确路径 | 无路径污染（日志可有 `cd_rollback`） | ☐ |

## 重连策略

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| RC-1 | 非手动断线（如 kill 会话） | 自动重连直至成功 | ☐ |
| RC-2 | 会话 → 断开 / 断开按钮 | **不**自动重连 | ☐ |
| RC-3 | 重连时 UI | 该 tab 遮罩 +「重连中」 | ☐ |

## 安全 / 日志

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| SEC-1 | 查看 `%APPDATA%\AnchorTerm\profiles.json` | 无明文密码、无 passphrase | ☐ |
| SEC-2 | 应用日志/控制台 | 无密码/passphrase 明文 | ☐ |
| SEC-3 | 操作日志 | 多会话行含 `sid=` 前缀；启动后仅本次 `*.log` | ☐ |

## 单元测试

```powershell
cd src-tauri
cargo test -p anchorterm --lib
```

期望：`cwd`（含 rollback）、`app_state` lifecycle、`complete`、`key_loader` 等通过。

## 输入模式

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| IN-1 | 默认 Shell，草稿发 `echo hi` | 命令只出现一次；终端区敲键不进 SSH | ☐ |
| IN-2 | 切换 TUI 直通后 `top` 再 `q` | 键盘直通，可正常退出 | ☐ |
| IN-3 | 重连后（有 OSC 7） | 一般只看到一次 `cd -- '...'` | ☐ |

## 已知限制（非阻塞）

- 主机密钥一律接受（未实现 known_hosts）
- 重连是新 login shell：未保存 vim、临时 env、后台任务会丢失
- 不自动重跑 `top`/脚本（后续阶段）
- Shell Integration 需手动安装（见 `docs/shell-integration.md`）
- 侧信道 `exec pwd` 无法读取交互 shell 的 cwd
- 不实现 Jump / SFTP / 分屏 / 会话树；忽略 `reconnect_enabled`

---

## 多会话与 UI（MS-\*）

> 后端：`session_id` + 事件 payload（PR2）。前端：多 `SessionView` + 标签栏（PR3）+ 菜单/去侧栏（PR4）。

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| MS-API | 连接后看操作日志 | 含 `sid=`；connect 带 session_id | ☐ |
| MS-1 | 两 tab（可同主机两次新建）分别 `cd` 不同目录并 `pwd` | 输出/cwd **不串流**；后台 tab 仍有 ECHO | ☐ |
| MS-2 | A 意外断线重连 | B 的 cwd/输出/state 不变 | ☐ |
| MS-3 | A 手动断开 | A 不自动重连；B 正常 | ☐ |
| MS-4 | A 断开后**同一标签**再连同 host+user | A 恢复 `restore_target` 路径 | ☐ |
| MS-5 | 关标签 A（×，确认后） | A 从 UI 消失；日志 `close_session removed`；B 正常 | ☐ |
| MS-TAB | 已有 live tab 时再新建/打开 | **新**标签，不替换 active 的 transport | ☐ |
| MS-6 | 连接中 / Failed tab 可关 | 无幽灵 tab / 无幽灵 reconnect | ☐ |
| MS-7 | 尝试第 17 个 tab | 拒绝并提示（上限 16） | ☐ |
| MS-KEY | 公钥会话 close 后 | 临时 key 删除（日志 `secure key temp removed`；无 key 内容） | ☐ |
| MS-UI | 无左侧连接侧栏；菜单可新建/打开/断开/关闭 | 主区全终端 | ☐ |
| MS-KEYS | Ctrl+N / Ctrl+W / Ctrl+Tab | 新建 / 关标签 / 切标签 | ☐ |
| MS-PASS | 管理器打开加密私钥配置 | 连接前弹口令；填正确口令可登录 | ☐ |

### 单 tab 回归（兼容）

| ID | 步骤 | 期望 | 结果 |
|----|------|------|------|
| PR2-1 | 连接 → 草稿命令有回显 | 正常 | ☐ |
| PR2-2 | 故意 `cd` 失败后再 `cd` 真目录 | cwd 正确；可再连恢复 | ☐ |
| PR2-3 | 手动断开 → 同 tab 再连接 | 恢复 cwd | ☐ |

---

## 文档与评审索引

| 文档 | 用途 |
|------|------|
| [UI-MULTI-TAB.md](UI-MULTI-TAB.md) | 多标签功能设计 |
| [CODE-REVIEW-PLAN.md](CODE-REVIEW-PLAN.md) | 分阶段评审清单与进度 |
| [reviews/](reviews/) | PR1–PR5 评审 / 自检记录 |
| [../README.md](../README.md) | 会话沉淀（实现真相源） |
