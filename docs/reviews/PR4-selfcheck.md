# PR4 开发者自检

- **日期：** 2026-07-25  
- **依赖：** PR3 评审通过  
- **对照：** UI-MULTI-TAB §1.1 / §6.9、CODE-REVIEW-PLAN §PR4  

## 已完成

| 项 | 状态 |
|----|------|
| 移除常驻左侧连接侧栏 | ✅ 全宽 menubar + tab + workspace |
| HTML 菜单：文件/编辑/查看/会话/工具/帮助 | ✅ 灰项 disabled |
| 属性对话框 mode 矩阵 | ✅ create / edit-profile / reconnect / edit-runtime |
| 会话管理器 打开/编辑/删除/新建 | ✅ 删配置不关 tab |
| empty-state 新建 + 打开管理器 | ✅ |
| Ctrl+N / Ctrl+O / Ctrl+W / Ctrl+Tab | ✅ |
| Ctrl+Shift+C/V/M | ✅ |
| 清屏仅 `term.clear()` | ✅ `clear-screen` 菜单 |
| 关闭确认 Connected/Connecting/Reconnecting | ✅ |
| 软上限 16 | ✅ |
| 退出确认 + 全 close_session | ✅ |
| 状态栏绑定 active（含 host 标题） | ✅ |
| 操作日志目录 opener | ✅ |
| 关于 / Shell Integration 说明 | ✅ |
| 忽略 reconnect_enabled | ✅ 未读取 |

## 建议手工

- [ ] 无侧栏；菜单新建 → 连接成功  
- [ ] 管理器打开配置 → 新 tab  
- [ ] 编辑/删除配置不杀已开 tab  
- [ ] Ctrl+N/W/Tab；清屏不扰动远端  
- [ ] 第 17 tab 拒绝  
- [ ] MS-5 关标签（补测）  
- [ ] 有连接时退出确认  

## 检查

```
npx tsc --noEmit
cargo test -p anchorterm --lib
```
