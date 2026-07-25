# AnchorTerm — 部署与安装方案

| 字段 | 内容 |
|------|------|
| **文档标题** | 让 AnchorTerm 更容易部署安装 |
| **日期** | 2026-07-25 |
| **状态** | 可行方案（待实施） |
| **目标平台** | Windows 10/11 x64（与现产品一致） |
| **技术底座** | Tauri 2 + Vite + Rust；`tauri.conf.json` 已 `"bundle.active": true` |
| **读者** | 负责人、发布者、后续开发 / AI 会话 |

---

## 1. 问题与现状

### 1.1 现在怎么「装」

| 角色 | 当前方式 | 成本 |
|------|----------|------|
| **开发者** | 装 Node + Rust + VS C++ 工作负载 → `npm install` → `npm run tauri:dev` | 高（数 GB 工具链） |
| **最终用户** | 无官方安装包；只能自己搭环境或拷贝调试产物 | **极高 / 基本不可行** |

现有脚本：

```text
npm run tauri:dev    # 开发
npm run tauri:build  # 已预留，会触发 beforeBuildCommand + 打包
```

`src-tauri/tauri.conf.json` 中：

```json
"bundle": {
  "active": true,
  "targets": "all",
  "icon": [ ... ]
}
```

说明：**打包能力已开启**，但尚未形成「一键安装给同事用」的发布流程与用户文档。

### 1.2 用户真正需要什么

1. **下载一个安装包**（或绿色压缩包）  
2. **双击安装 / 解压运行**  
3. 系统已有或安装包顺带解决 **WebView2**、**OpenSSH 客户端** 依赖  
4. 开始菜单有快捷方式，卸载干净  
5. （进阶）更新版本时不丢配置（`%APPDATA%\AnchorTerm`）

### 1.3 约束（与产品绑定）

| 约束 | 含义 |
|------|------|
| 仅 Windows 桌面 | 不做 macOS/Linux 首期 |
| 依赖系统 `ssh.exe` | 安装说明 / 检测 / 可选引导安装 OpenSSH 客户端 |
| 依赖 WebView2 | Tauri 2 标准依赖；Win10/11 多数已带 |
| 配置与日志在用户目录 | 安装程序**不应**删除 `%APPDATA%\AnchorTerm` 与项目外日志策略需写清 |
| 不打包用户私钥 | 安装包零密钥；证书仍由用户自备路径 |

---

## 2. 目标

### 2.1 成功标准

| ID | 标准 |
|----|------|
| D1 | 无 Rust/Node 的用户，**10 分钟内**装好并打开 AnchorTerm |
| D2 | 提供至少一种：**NSIS 安装包（.exe）** 或 **MSI** + 一种 **便携 zip（可选）** |
| D3 | README / 单独 INSTALL 文档有「最终用户」与「发布者」两套路径 |
| D4 | 发布者可在干净 CI 或本机一条命令产出安装包 |
| D5 | 安装后不依赖源码树；操作日志默认仍可写到约定目录（见 §5.4） |

### 2.2 非目标（首期）

| 项 | 说明 |
|----|------|
| 应用商店（Microsoft Store） | 签名与商店审核后置 |
| 自动更新（Sparkle/自研） | 可作为二期 |
| 绿色版完全免安装 + 便携配置 | 二期可选 |
| 交叉编译非 x64 | 首期仅 `x86_64-pc-windows-msvc` |
| 把 OpenSSH 静态编进安装包 | 体量大且与系统策略冲突；优先检测系统 OpenSSH |

---

## 3. 方案选型

### 3.1 交付物形态（推荐组合）

| 形态 | 说明 | 推荐 |
|------|------|------|
| **NSIS Setup `.exe`** | Tauri 默认 Windows 安装器；双击安装、开始菜单、卸载 | **主推** |
| **MSI** | 企业 GPO / 静默安装友好 | **可选并行**（`bundle.targets`） |
| **portable `.zip`** | 解压即运行 `AnchorTerm.exe`，无写注册表 | **建议同时产出**（内测/U 盘） |

Tauri 2 配置思路（实施时写入 `tauri.conf.json`）：

```json
"bundle": {
  "active": true,
  "targets": ["nsis", "msi"],
  "windows": {
    "nsis": {
      "installMode": "currentUser",
      "languages": ["SimpChinese", "English"],
      "displayLanguageSelector": false
    }
  }
}
```

- **currentUser** 安装：无需管理员，降低「装不上」概率。  
- 企业若强制「所有用户」再提供 `perMachine` 变体。

### 3.2 构建命令（发布者）

```powershell
# 环境：Node 18+、Rust stable、VS Build Tools（C++）、WebView2 开发机可选
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cd C:\zengshangchun\AnchorTerm
npm ci
npm run tauri:build
```

典型产出位置（Tauri 2）：

```text
src-tauri/target/release/bundle/nsis/AnchorTerm_*_x64-setup.exe
src-tauri/target/release/bundle/msi/AnchorTerm_*_x64_en-US.msi
src-tauri/target/release/AnchorTerm.exe   # 亦可打进 zip
```

### 3.3 依赖策略

| 依赖 | 策略 |
|------|------|
| **WebView2 Runtime** | 文档写明；安装器可用 Tauri/webview2 bootstrapper 或检测缺失时打开下载页。Win11 一般已有。 |
| **OpenSSH Client** | 安装后首次启动可检测 `ssh.exe`；缺失则提示「设置 → 可选功能 → OpenSSH 客户端」或 `Add-WindowsCapability`。 |
| **VC++ 运行库** | 以 Tauri/Rust 静态链接策略为准；若动态依赖则安装包附带或文档说明。 |

### 3.4 代码签名（强烈建议，非首期阻断）

未签名的 `.exe` 在 Windows 上易触发 **SmartScreen「未知发布者」**，用户不敢装。

| 阶段 | 做法 |
|------|------|
| 内测 | 可不签名；文档说明「允许运行」步骤 |
| 对外 | 购买代码签名证书（OV/EV）；`signtool` / Tauri `windows.certificateThumbprint` |
| CI | 证书放在 secrets；流水线签名后再上传 Release |

无签名时，INSTALL 文档必须写清：属性 → 解除锁定 / 更多信息 → 仍要运行。

### 3.5 版本与产物命名

| 项 | 约定 |
|----|------|
| 版本源 | `package.json` 与 `tauri.conf.json` / `Cargo.toml` **保持一致**（可用脚本同步） |
| 命名 | `AnchorTerm-{version}-windows-x64-setup.exe` |
| 变更说明 | GitHub Release notes 或 `CHANGELOG.md` |

---

## 4. 分阶段实施计划

### 阶段 A — 本机可重复打出安装包（1–2 天）

**目标：** 发布者在开发机执行 `npm run tauri:build` 得到 NSIS 安装包，另一台无开发环境的 Windows 能装能开。

| 任务 | 说明 |
|------|------|
| A1 | 收紧 `bundle.targets`（如 `nsis` 或 `["nsis","msi"]`），配置中文 NSIS 文案 |
| A2 | 核对 `productName` / `identifier` / 图标 |
| A3 | 本机 release 构建，修复缺图标、路径、WebView2 等问题 |
| A4 | 干净虚拟机或同事机：**安装 → 启动 → 私钥登录冒烟** |
| A5 | 编写 [INSTALL.md](INSTALL.md)（用户向）+ README 增加「安装」入口 |

**验收：** 干净机安装后可连接测试 SSH，无需 Node/Rust。

---

### 阶段 B — 便携包 + 依赖检测（2–3 天）

| 任务 | 说明 |
|------|------|
| B1 | 打包脚本：release `AnchorTerm.exe` + 必要 dll（若有）→ zip |
| B2 | 启动时检测 `ssh.exe`：缺失则 UI 明确错误 + 打开帮助链接（不静默失败） |
| B3 | （可选）WebView2 缺失时引导安装 |
| B4 | 安装包与 zip 的版本号自动从 conf 读取 |

**验收：** 无 OpenSSH 时有清晰提示；有 OpenSSH 时 zip 解压即用。

---

### 阶段 C — CI 自动发布（3–5 天）

| 任务 | 说明 |
|------|------|
| C1 | GitHub Actions：`windows-latest` 上 `tauri-apps/tauri-action` 或自建 steps |
| C2 | tag `v*` 触发：构建 → 上传 Release 资产 |
| C3 | （可选）签名步骤 |
| C4 | 失败通知、缓存 `cargo`/`npm` 加速 |

示例流水线骨架（实施时落盘 `.github/workflows/release.yml`）：

```yaml
# 示意 — 以 tauri-action 官方模板为准微调
on:
  push:
    tags: ["v*"]
jobs:
  release:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: actions/setup-node@v4
        with: { node-version: "20" }
      - run: npm ci
      - uses: tauri-apps/tauri-action@v0
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        with:
          tagName: ${{ github.ref_name }}
          releaseName: AnchorTerm ${{ github.ref_name }}
          releaseDraft: true
```

**验收：** 打 tag 后 Release 页出现 setup.exe（及 msi/zip）。

---

### 阶段 D — 安装体验打磨（按需）

| 任务 | 说明 |
|------|------|
| D1 | 静默安装参数文档（企业）：`/S` 等 NSIS 参数 |
| D2 | winget manifest（社区或私有源） |
| D3 | 应用内「检查更新」或仅「打开下载页」 |
| D4 | 卸载是否保留配置：默认**保留** `%APPDATA%\AnchorTerm`，文档说明如何手动清除 |

---

## 5. 运行时路径与日志策略

### 5.1 安装后布局（典型）

```text
%LOCALAPPDATA%\Programs\AnchorTerm\   # 或安装器选择的目录
  AnchorTerm.exe
  (webview / 依赖)
%APPDATA%\AnchorTerm\
  profiles.json                       # 主机配置（无密码）
```

### 5.2 操作日志（现状与建议）

**现状：** 日志写到**源码旁** `操作日志\`（开发友好，安装版不合适）。

**安装版建议（阶段 B 实施）：**

| 模式 | 日志目录 |
|------|----------|
| 开发（`tauri dev` 或 env） | 仓库内 `操作日志\`（保持现状） |
| 安装/便携正式运行 | `%APPDATA%\AnchorTerm\logs\` 或 `%LOCALAPPDATA%\AnchorTerm\logs\` |

启动时：

1. 若存在 env `ANCHORTERM_LOG_DIR` → 用其路径  
2. 否则若检测到「安装目录运行」→ AppData logs  
3. 否则开发树旁 `操作日志\`

菜单「打开操作日志目录」已用 opener，路径随配置走即可。

> 此条属于**小代码改动**，对「可安装」体验很关键，建议放在阶段 B，并同步 [操作日志/README.md](../操作日志/README.md)。

### 5.3 配置迁移

- 安装升级：**不删** `profiles.json` 与凭据管理器条目。  
- 卸载：默认保留用户数据；高级选项或文档说明手动删除路径。

---

## 6. 文档体系

### 6.1 建议新增 / 修改

| 文档 | 内容 |
|------|------|
| **[docs/INSTALL.md](INSTALL.md)**（新建） | 最终用户：下载、安装、SmartScreen、OpenSSH、首次连接 |
| **docs/RELEASE.md**（新建，发布者） | 版本 bump、构建命令、产物检查、打 tag、签名 |
| **README.md** | 「安装使用」链到 INSTALL；「从源码开发」保留现状 |
| **操作日志/README.md** | 安装版日志路径说明 |

### 6.2 用户安装步骤（INSTALL 正文草稿）

1. 确认 Windows 10 1803+ 或 Windows 11，64 位。  
2. 下载 `AnchorTerm-*-setup.exe`。  
3. 若 SmartScreen 拦截 →「更多信息」→「仍要运行」（签名后可省略）。  
4. 安装向导 → 完成 → 开始菜单启动。  
5. 若提示找不到 SSH：安装「OpenSSH 客户端」可选功能。  
6. 菜单新建会话 / 打开配置 → 连接（加密钥会弹口令）。

---

## 7. 风险与缓解

| 风险 | 严重度 | 缓解 |
|------|--------|------|
| SmartScreen 拦截未签名包 | 高 | 内测说明；对外签名 |
| 用户无 OpenSSH | 高 | 启动检测 + 文档 + 可选脚本 |
| 用户无 WebView2 | 中 | 安装器/引导下载 Evergreen Runtime |
| 构建机缺 VS Build Tools | 中 | RELEASE.md 写清；CI 用 windows-latest |
| 安装版日志仍写源码路径失败 | 中 | §5.2 改 AppData |
| 版本号三处不一致 | 低 | 单一来源脚本同步 |
| 杀软误报 | 中 | 签名 + 固定发布渠道 + 哈希校验 |

---

## 8. 工作量粗估

| 阶段 | 工作量（1 人熟悉 Tauri） | 产出 |
|------|--------------------------|------|
| A 本机安装包 + 文档 | 1–2 人日 | setup.exe + INSTALL.md |
| B 便携包 + 依赖检测 + 日志路径 | 2–3 人日 | zip + 启动检测 |
| C CI Release | 2–4 人日 | tag 自动产物 |
| D 签名 / winget / 静默 | 按需 | 企业级分发 |

---

## 9. 建议实施顺序（结论）

```text
立即（阶段 A）
  1. 本机跑通 tauri build → NSIS
  2. 干净环境安装冒烟
  3. 写 INSTALL.md + README 入口

短期（阶段 B）
  4. 安装版日志改 AppData
  5. 启动检测 ssh.exe
  6. 可选 portable zip

中期（阶段 C）
  7. GitHub Actions 发版
  8. 有预算则代码签名

按需（阶段 D）
  9. MSI 静默 / winget / 应用内更新入口
```

**最小可行交付（MVP）：**  
**阶段 A 完成** 即达到「同事不用装 Rust 也能用」——这是「更容易部署安装」的第一拐点。

---

## 10. 验收清单（部署专项）

| ID | 步骤 | 期望 |
|----|------|------|
| DEP-1 | 开发机 `npm run tauri:build` | 生成 setup.exe，退出码 0 |
| DEP-2 | 无 Node/Rust 的 Windows 安装 setup | 安装成功，开始菜单可启动 |
| DEP-3 | 安装后公钥/加密钥连接测试机 | 与 dev 行为一致 |
| DEP-4 | 卸载（若 NSIS） | 程序移除；AppData 配置仍在（按约定） |
| DEP-5 | 无 ssh.exe 环境启动 | 明确错误，不白屏崩溃 |
| DEP-6 | （CI）打 tag | Release 出现安装包资产 |

---

## 11. 相关文件（实施时会动）

| 路径 | 变更 |
|------|------|
| `src-tauri/tauri.conf.json` | bundle targets、nsis、签名相关 |
| `package.json` | 可选 `version` 脚本、`release` 脚本 |
| `src-tauri/src/ops_log.rs` | 安装版日志目录解析 |
| `src/main.ts` | 可选：启动检测 ssh（可调后端 command） |
| `docs/INSTALL.md` / `docs/RELEASE.md` | 新建 |
| `.github/workflows/release.yml` | 阶段 C |
| `README.md` | 安装入口 |

---

## 12. 与路线图关系

- 本方案**正交于** [ROADMAP-NEXT.md](ROADMAP-NEXT.md) 中的 Jump/SFTP/TUI 恢复。  
- **建议优先做部署**：功能再强，无法安装则无法推广。  
- 部署稳定后，再开 Jump / 会话树等产品项更合适。

---

*方案结束。实施时可从阶段 A 开 PR：`build: Windows NSIS installer + INSTALL.md`。*
