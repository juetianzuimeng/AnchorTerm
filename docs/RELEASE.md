# AnchorTerm — 发布者构建与发版说明

面向需要产出 **Windows 安装包** 的开发者 / 发布负责人。

最终用户安装步骤见 [INSTALL.md](INSTALL.md)。整体方案见 [DEPLOYMENT.md](DEPLOYMENT.md)。

---

## 1. 构建机环境

| 组件 | 要求 |
|------|------|
| 系统 | Windows 10/11 x64 |
| Node.js | 18+（建议 20 LTS） |
| Rust | `rustup` stable，`x86_64-pc-windows-msvc` |
| C++ 工具链 | Visual Studio 2022：**使用 C++ 的桌面开发** 工作负载（或 Build Tools 等价） |
| WebView2 | 开发机一般已有；打出的安装包依赖用户机 Runtime |
| 可选 | 代码签名证书（对外正式版强烈建议） |

确认工具在 PATH 中：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
node -v
npm -v
rustc -V
cargo -V
```

---

## 2. 版本号同步

发版前保持下列三处 **一致**（当前示例 `0.1.0`）：

| 文件 | 字段 |
|------|------|
| `package.json` | `"version"` |
| `src-tauri/tauri.conf.json` | `"version"` |
| `src-tauri/Cargo.toml` | `version` |

修改版本后建议：

```powershell
# 可选：确认三处相同
Select-String -Path package.json,src-tauri\tauri.conf.json,src-tauri\Cargo.toml -Pattern '"version"|version\s*='
```

产物命名约定（文档与分发）：`AnchorTerm-{version}-windows-x64-setup.exe`（可将 NSIS 默认文件名重命名后上传）。

---

## 3. 一条命令构建安装包

在仓库根目录：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cd C:\zengshangchun\AnchorTerm   # 换成你的克隆路径
npm ci                            # 或 npm install
npm run tauri:build
```

该命令会：

1. 执行 `beforeBuildCommand`（前端 `tsc` + `vite build`）  
2. 编译 Rust release  
3. 按 `tauri.conf.json` 的 `bundle` 打 **NSIS** 安装包  

### 3.1 bundle 配置（阶段 A）

`src-tauri/tauri.conf.json` 关键：

- `bundle.active`: `true`  
- `targets`: `["nsis"]`  
- `windows.nsis.installMode`: `currentUser`（免管理员）  
- `languages`: 简体中文 + English  

后续若需 MSI：将 `targets` 改为 `["nsis", "msi"]` 后重新构建。

### 3.2 便携 zip（阶段 B）

在已有 release 二进制后：

```powershell
npm run pack:portable
# 版本号自动读 package.json → dist-release\AnchorTerm-{version}-windows-x64-portable.zip
```

或一条龙（构建 NSIS + 便携包）：

```powershell
npm run release:win
```

脚本：`scripts/pack-portable.ps1`。

---

## 4. 产物检查

构建成功后典型路径：

```text
src-tauri\target\release\anchorterm.exe
src-tauri\target\release\bundle\nsis\AnchorTerm_*_x64-setup.exe
dist-release\AnchorTerm-*-windows-x64-portable.zip
```

| 检查项 | 期望 |
|--------|------|
| 退出码 | `npm run tauri:build` / `pack:portable` 为 0 |
| setup.exe 存在 | `src-tauri\target\release\bundle\nsis\` 下有安装包 |
| 便携 zip | `dist-release\` 下有 zip；解压后仅运行 exe 即可启动 |
| 体积 | 合理非空 |
| 本机安装冒烟 | 双击 setup → 开始菜单启动 → 窗口正常（不依赖源码树） |
| 日志路径 | 安装/便携运行日志在 `%APPDATA%\AnchorTerm\logs\` |
| 无 ssh | 启动弹出「需要 OpenSSH 客户端」说明 |
| 有 SSH 时 | 私钥/密码登录测试机，行为与 `tauri:dev` 一致 |

快速列产物：

```powershell
Get-ChildItem -Recurse src-tauri\target\release\bundle -Filter *.exe
Get-ChildItem src-tauri\target\release\anchorterm.exe
Get-ChildItem dist-release\*.zip -ErrorAction SilentlyContinue
```

---

## 5. 分发清单（建议）

| 项 | 说明 |
|----|------|
| 安装包 | `*_x64-setup.exe`（或重命名为约定名） |
| 校验 | SHA256（`Get-FileHash -Algorithm SHA256`） |
| 说明 | 指向 [INSTALL.md](INSTALL.md)（SmartScreen、OpenSSH、WebView2） |
| 变更 | Release notes 或 CHANGELOG 摘要 |

```powershell
Get-FileHash -Algorithm SHA256 src-tauri\target\release\bundle\nsis\*.exe
```

---

## 6. 代码签名（非阶段 A 阻断）

未签名时用户易遇 SmartScreen「未知发布者」——INSTALL 已写绕过步骤。

对外正式版：

1. 准备 OV/EV 代码签名证书  
2. 使用 `signtool` 或配置 Tauri `bundle.windows.certificateThumbprint` 等字段  
3. CI 中证书放 secrets，签名后再上传 Release  

详见 [DEPLOYMENT.md](DEPLOYMENT.md) §3.4。

---

## 7. 发版流程（当前：本机；后续：CI）

### 7.1 本机发版（阶段 A）

1. 同步版本号（§2）  
2. `git status` 确认发布点干净或 intentional  
3. `npm run tauri:build`  
4. 产物检查 + 干净环境安装冒烟  
5. 上传 setup.exe + 哈希 + 安装说明链接  
6. （可选）打 tag `v0.1.0` 并写 Release notes  

### 7.2 CI 自动发版（阶段 C，未实施）

计划：`tag v*` → GitHub Actions `windows-latest` → 上传 Release 资产。骨架见 DEPLOYMENT §4 阶段 C。

---

## 8. 硬约束（发版勿破坏）

- 交互仍依赖系统 **OpenSSH**；**不要**把用户私钥打进安装包。  
- 配置在 `%APPDATA%\AnchorTerm`；密码走系统 keyring。  
- 多会话语义（`session_id`、disconnect ≠ close 等）见 README「会话沉淀」。  
- 操作日志：开发为仓库旁 `操作日志\`；安装/便携为 `%APPDATA%\AnchorTerm\logs\`（`ANCHORTERM_LOG_DIR` 可覆盖）。

---

## 9. 故障排查

| 现象 | 可能原因 | 处理 |
|------|----------|------|
| `link.exe` / MSVC 错误 | 未装 C++ 工作负载 | 安装 VS「使用 C++ 的桌面开发」 |
| `cargo` / `rustc` 找不到 | PATH 未含 cargo | `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"` |
| 前端 `tsc` 失败 | TS 错误 | 先 `npm run build` 单独修 |
| NSIS 打包失败 | 图标缺失 / 工具链 | 确认 `src-tauri/icons/` 齐全；更新 `@tauri-apps/cli` |
| 安装后无 ssh | 用户机未装 OpenSSH | INSTALL §1.1 |

---

## 10. 相关文档

| 文档 | 用途 |
|------|------|
| [INSTALL.md](INSTALL.md) | 最终用户安装 |
| [DEPLOYMENT.md](DEPLOYMENT.md) | 方案、阶段 B/C/D |
| [README.md](../README.md) | 产品与开发入门 |
