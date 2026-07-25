# AnchorTerm — 第一阶段开工方案

**文档状态：** Sprint A–D 已完成实现  
**平台：** Windows  

## 阶段目标

单会话 SSH 真终端：密码/私钥登录、自动重连、cwd 静默恢复、草稿不丢。

## Sprint 状态

| Sprint | 状态 |
|--------|------|
| A 骨架 + 密码 PTY | ✅ |
| B 私钥 + HostProfile + 凭据库 | ✅ |
| C 重连 + 草稿 + CWD + overlay | ✅ |
| D 脱敏 + 中文路径测试 + 文档验收 | ✅ |

## 本地运行

见 [README.md](../README.md)。

## 验收

见 [ACCEPTANCE.md](ACCEPTANCE.md) 与 [shell-integration.md](shell-integration.md)。

## 实现索引

| 能力 | 位置 |
|------|------|
| SSH PTY / keepalive | `src-tauri/src/ssh/transport.rs` |
| 自动重连 / 恢复剧本 | `src-tauri/src/session/mod.rs` |
| CwdTracker + 单测 | `src-tauri/src/cwd/mod.rs` |
| 草稿框 UI | `src/main.ts` + `index.html` |
| 密码 keyring | `src-tauri/src/auth/credentials.rs` |
