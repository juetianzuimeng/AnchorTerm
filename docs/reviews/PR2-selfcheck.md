# PR2 开发者自检

- **日期：** 2026-07-25  
- **依赖：** PR1 评审通过  
- **对照：** UI-MULTI-TAB PR2、CODE-REVIEW-PLAN §PR2  

## 已完成

| 项 | 状态 |
|----|------|
| 删除 DEFAULT_SESSION_ID 业务路径 | ✅ map 默认空；任意 client UUID |
| `ConnectRequest.session_id` 必填；`ConnectResponse` 回显 | ✅ |
| 命令强制 sid：disconnect/submit/write/complete/resize/snapshot | ✅ |
| `close_session` / `list_sessions` | ✅ |
| `SessionNotFound` | ✅ |
| 事件 payload 对象化 data/cwd/state/error | ✅ |
| 前端 UUID 先于 connect；事件按 sid 路由 | ✅ |
| disconnect 保留 map；close 移除 | ✅ |
| UT：snapshot id、insert/remove、disconnect 保留、gen bump、last_stty | ✅ |
| ACCEPTANCE MS-\* 草稿 | ✅ |

## 测试

```
cargo test -p anchorterm --lib
```

## 手工（PR2 单 tab）

- [x] PR2-1 连接 + 草稿回显  
- [x] PR2-2 cd 失败回滚 + 再连（日志 `cd_rollback` + restore `/home/tguser/tg1`）  
- [x] PR2-3 手动断再连 cwd  

**正式评审：** 通过 — 见 [PR2-review.md](PR2-review.md)（2026-07-25）。  

## 注意

**不可只升后端**：事件 data 已为 `{session_id,data_b64}`，旧前端会解码失败。
