# 新会话提示词：落实部署安装方案

> **用法：** 在新 AI / 开发会话中整段复制下方「提示词正文」即可开工。  
> **规格主文档：** [docs/DEPLOYMENT.md](../DEPLOYMENT.md)

---

## 提示词正文

```markdown
# 任务：落实 AnchorTerm 部署安装方案（阶段 A 起）

## 项目与基线
- 仓库：`C:\zengshangchun\AnchorTerm`（Windows SSH 终端客户端）
- 技术栈：Tauri 2 + Vite + TypeScript + Rust；交互会话走系统 OpenSSH `ssh -tt`
- 多标签 / 菜单化功能已在 commit `c9baabe` 合入 master（未必要 push）
- **请先阅读：**
  1. `docs/DEPLOYMENT.md`（本任务主规格）
  2. `README.md` 的「会话沉淀」（实现硬约束，勿回归）
  3. 需要时再看 `docs/ROADMAP-NEXT.md`（与本任务正交，不要做 Jump/SFTP 等）

## 目标
让**没有 Node/Rust 的最终用户**能安装并运行 AnchorTerm；让发布者能稳定打出安装包。

成功标准（DEPLOYMENT §2.1）：
- D1：无开发环境用户约 10 分钟内装好并打开
- D2：至少产出 **NSIS setup.exe**；可选 MSI + 便携 zip
- D3：用户向 INSTALL + 发布者说明
- D4：本机（后续可 CI）一条命令产出安装包
- D5：安装后不依赖源码树

## 建议实施顺序（严格按阶段，先做 A）
### 阶段 A（本会话优先完成 MVP）
1. 配置 `src-tauri/tauri.conf.json` 的 bundle（建议 targets 含 `nsis`，中文 NSIS 等，见 DEPLOYMENT §3）
2. 本机执行 `npm run tauri:build`，修复构建/打包问题
3. 确认产物路径（如 `src-tauri/target/release/bundle/nsis/`）
4. 编写 `docs/INSTALL.md`（最终用户：下载、SmartScreen、OpenSSH、WebView2、首次连接）
5. 编写 `docs/RELEASE.md`（发布者：版本号、构建命令、产物检查）
6. 更新 `README.md` 增加「安装使用」入口，保留「从源码开发」
7. 若有条件：在「无完整工具链」环境做安装冒烟（能启动即可；有 SSH 则测登录）

### 阶段 B（A 完成后再做，可本会话延续或下会话）
- 便携 zip
- 启动检测 `ssh.exe`（缺失时明确提示）
- **安装版操作日志目录改到 AppData**（现状写源码旁 `操作日志\`，安装版不合适，见 DEPLOYMENT §5.2）
- 菜单「打开操作日志目录」跟随新路径

### 阶段 C / D
- GitHub Actions 打 tag 发 Release
- 代码签名、winget、静默安装——有明确需求再做

## 硬约束（勿破坏）
- 交互仍用系统 OpenSSH；不把用户私钥打进安装包
- profiles 在 `%APPDATA%\AnchorTerm`（无密码明文）；凭据用系统 keyring
- 多会话：`session_id`、disconnect ≠ close、cwd 失败回滚、日志禁 password/passphrase
- 不顺便做 Jump / 分屏 / SFTP / 大 UI 重构（范围蔓延禁止）
- commit message 用中文；未要求则不要 push

## 验收对照（阶段 A）
- [ ] `npm run tauri:build` 成功生成 setup.exe
- [ ] 干净机或模拟最终用户：安装 → 启动（不依赖仓库路径）
- [ ] INSTALL.md + RELEASE.md + README 入口齐全
- [ ] DEPLOYMENT 中 DEP-1～DEP-3 可勾选说明

## 交付物
- 代码/配置改动 + 文档
- 本机构建成功时注明产物完整路径
- 可选：一个中文 commit（说明目标与实现）

请从阅读 `docs/DEPLOYMENT.md` 与当前 `tauri.conf.json` 开始，先给出阶段 A 的具体改动清单，再动手实现。
```

---

## 可选附加说明（粘贴时按需追加）

| 场景 | 追加一句 |
|------|----------|
| 本会话只做安装包 | 「本会话仅做阶段 A，B 的日志路径改动单独开任务。」 |
| 已有 GitHub 仓库 | 「阶段 C 的远程仓库为：`<org/repo>`。」 |
| 需要代码签名 | 「阶段 A 可不签名；签名证书与 thumbprint 由发布者提供后再做。」 |

---

## 相关文档

| 文档 | 说明 |
|------|------|
| [../DEPLOYMENT.md](../DEPLOYMENT.md) | 部署安装完整方案 |
| [../ROADMAP-NEXT.md](../ROADMAP-NEXT.md) | 功能向下一阶段（与部署正交） |
| [../../README.md](../../README.md) | 会话沉淀与开发说明 |
