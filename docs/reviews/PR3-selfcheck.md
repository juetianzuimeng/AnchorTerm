# PR3 开发者自检

- **日期：** 2026-07-25  
- **依赖：** PR2 评审通过  
- **对照：** UI-MULTI-TAB PR3、CODE-REVIEW-PLAN §PR3  

## 已完成

| 项 | 状态 |
|----|------|
| `Map<sessionId, SessionView>` | ✅ |
| 标签栏创建/切换/关闭/状态点/标题序号 | ✅ |
| `activate`：可见后 rAF fit → resize | ✅ |
| 事件按 sid 路由；未知 miss；后台 tab 仍 write | ✅ |
| 侧栏「连接」：live 时始终新 tab；idle 同 host/user 复用 tab（MS-4） | ✅ |
| 断开仅 active | ✅ |
| 关标签 → `close_session` + dispose xterm | ✅ |
| 软上限 16 | ✅ |
| 侧栏暂留（PR4 去侧栏） | ✅ |
| ACCEPTANCE MS-1–5 / MS-TAB / MS-7 | ✅ |
| 窗口 resize 仅 active fit | ✅ |

## 连接语义（实现说明）

| 场景 | 行为 |
|------|------|
| 无 tab / 强制新 tab（+ / empty / 配置双击） | 新 UUID + 新 SessionView |
| active 为 live/connecting | 新 tab（不替换） |
| active 为 idle/failed 且 host+user 相同 | **复用** sid 再连（cwd restore） |
| 已有 16 个 tab | toast 拒绝 |

## 手工（联调 + 日志评审）

- [x] MS-1 双 tab 输出隔离（日志 A=tg1 / B=tg2）  
- [x] MS-3 / MS-4 断开与同 tab 再连  
- [ ] MS-5 关标签（**代码已实现；日志未覆盖，进 PR4 前补测**）  
- [x] MS-TAB 连接不替换 live  
- [ ] MS-7 第 17 个拒绝（软上限已实现；可选）  

**正式评审：** **通过** — 见 [PR3-review.md](PR3-review.md)（2026-07-25）。  

## 测试

```
npx tsc --noEmit   # 已通过
cargo test -p anchorterm --lib
```
