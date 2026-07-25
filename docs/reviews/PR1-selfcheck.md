# PR1 开发者自检

- **日期：** 2026-07-25  
- **对照：** [CODE-REVIEW-PLAN.md](../CODE-REVIEW-PLAN.md) §PR1、[UI-MULTI-TAB.md](../UI-MULTI-TAB.md)  
- **状态：** 编码完成，待正式评审  

## 范围

| 项 | 结果 |
|----|------|
| 结构抽取 `SessionRuntime` + `AppState.sessions` map | ✅ |
| 仅 `"default"`；拒绝其他 session_id | ✅ |
| TS 命令签名未改 | ✅（前端未改） |
| 事件 payload 形状未改（data 仍为裸 base64） | ✅ |
| 无真多 tab UI | ✅ |

## §4.3.1 迁移

| 符号 | 状态 |
|------|------|
| AppState → SessionRuntime + map | ✅ |
| snapshot / emit_state / set_state | ✅ per-runtime |
| connect_inner / disconnect_inner | ✅ |
| schedule_seed_login_pwd | ✅ 捕获 `Arc<SessionRuntime>` |
| reconnect_loop | ✅ 按 runtime + gen |
| run_restore_playbook / freeze | ✅ |
| submit_line / write / complete / resize | ✅ |
| on_data / finish_session / wait task | ✅ 绑定 session_id |
| connect_openssh / connect_session | ✅ 传 session_id |
| last_stty 上 runtime | ✅ 删除 static |

## 测试

```
cargo test -p anchorterm --lib
```

结果：**20 passed**（含 UT-1 类：`snapshot_includes_default_session_id`、`reject_second_session_id`）

## 建议手工冒烟（阶段 1 回归）

- [ ] 私钥登录  
- [ ] `cd` → `pwd` → `ls` 有回显  
- [ ] Tab 补全  
- [ ] 手动断开再连 cwd 恢复  
- [ ] 连续 cd 后无「第一条后全哑」  

## 允许进入 PR2

待代码评审通过后。
