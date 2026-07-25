# AnchorTerm — 最终用户安装指南

面向**没有 Node / Rust 开发环境**的用户。装好后即可连接 SSH 主机。

| 项 | 说明 |
|----|------|
| 平台 | Windows 10（1803+）或 Windows 11，**64 位** |
| 安装包 | `AnchorTerm-*-setup.exe`（NSIS 安装程序） |
| 配置位置 | `%APPDATA%\AnchorTerm\`（升级不丢；卸载默认保留） |

---

## 1. 安装前准备

### 1.1 系统依赖

| 依赖 | 是否必需 | 说明 |
|------|----------|------|
| **WebView2 Runtime** | 是 | Tauri 界面依赖。Windows 11 与多数 Win10 已自带；缺失时安装器或系统可能提示安装。也可从 [Microsoft WebView2](https://developer.microsoft.com/microsoft-edge/webview2/) 下载 Evergreen Runtime。 |
| **OpenSSH 客户端**（`ssh.exe`） | 是 | 交互终端走系统 `ssh`，**安装包不内置** OpenSSH。多数 Win10/11 已启用。 |

**检查 OpenSSH 是否可用（可选）：** 打开 PowerShell 或 CMD，执行：

```powershell
ssh -V
```

若提示找不到命令，请安装「OpenSSH 客户端」：

1. **设置** → **应用** → **可选功能** → **添加功能** → 搜索 **OpenSSH 客户端** → 安装  
2. 或以管理员 PowerShell：

```powershell
Add-WindowsCapability -Online -Name OpenSSH.Client~~~~0.0.1.0
```

安装后重新打开终端，再执行 `ssh -V` 确认。

### 1.2 私钥与密码

- 私钥文件放在本机任意可读路径（安装包**不会**附带你的密钥）。  
- 加密私钥在连接时会弹出口令框；口令**不写入**配置文件。  
- 登录密码可选用 Windows 凭据管理器保存（服务名 `AnchorTerm`）。

---

## 2. 下载与安装

1. 从发布渠道获取安装包，例如：  
   `AnchorTerm-0.1.0-setup.exe`  
   （文件名以实际发布为准，通常位于 GitHub Release 或内部分发目录。）
2. 双击运行安装程序。  
3. 按向导完成安装（默认**当前用户**安装，一般**不需要管理员权限**）。  
4. 从**开始菜单**启动 **AnchorTerm**。

安装程序会创建开始菜单快捷方式，并可按向导选择是否创建桌面图标。程序默认安装在当前用户目录下（常见为 `%LOCALAPPDATA%\AnchorTerm\`，以本机实际路径为准）。

---

## 3. SmartScreen /「未知发布者」提示

内测或未代码签名的安装包可能被 Windows 拦截：

| 情况 | 处理 |
|------|------|
| **Windows 已保护你的电脑** | 点击 **更多信息** → **仍要运行** |
| 文件被标记为来自网络 | 右键安装包 → **属性** → 勾选 **解除锁定** → 确定后再运行 |
| 杀毒软件误报 | 以发布方提供的哈希/渠道为准；内测可临时放行 |

正式对外版本建议由发布者使用代码签名证书，以减少上述步骤。

---

## 4. 首次连接

1. 菜单 **文件 → 新建会话**（或 `Ctrl+N`）。  
2. 填写 **主机**、**端口**（默认 22）、**用户名**。  
3. 认证方式：  
   - **私钥**：填写本机私钥的**绝对路径**；若密钥有口令，连接前会弹出输入框。  
   - **密码**：可在连接时输入，或保存到系统凭据库。  
4. 可选：保存为配置文件（主机列表在 `%APPDATA%\AnchorTerm\profiles.json`，**不含密码明文**）。  
5. 连接成功后即可在终端中操作；默认 **Shell + 草稿** 模式，可用菜单切换 TUI 直通。

更多操作（多标签、断线重连、快捷键）见仓库 [README.md](../README.md) 的「使用说明」。

---

## 5. 配置、卸载与数据

| 数据 | 位置 | 卸载时 |
|------|------|--------|
| 主机配置 | `%APPDATA%\AnchorTerm\profiles.json` | **默认保留** |
| 登录密码（若曾保存） | Windows 凭据管理器，服务名 `AnchorTerm` | 需手动删除 |
| 操作日志 | `%APPDATA%\AnchorTerm\logs\` | 默认保留；可用菜单「工具 → 打开操作日志目录」 |
| 程序本体 | 安装目录（常见 `%LOCALAPPDATA%\AnchorTerm\`） | 卸载程序移除 |

### 便携版（zip）

若获得的是 `AnchorTerm-*-windows-x64-portable.zip`：

1. 解压到任意目录  
2. 双击 `anchorterm.exe`（无需安装向导）  
3. 配置与日志仍在 `%APPDATA%\AnchorTerm\`（与安装版相同，换机器解压不会带走配置）

**卸载：** 设置 → 应用 → 已安装的应用 → AnchorTerm → 卸载；或使用安装时创建的卸载入口。

**彻底清除配置（可选）：**

```powershell
Remove-Item -Recurse -Force "$env:APPDATA\AnchorTerm"
# 凭据管理器中搜索 AnchorTerm 并删除相关条目
```

---

## 6. 常见问题

### 6.1 安装后打不开 / 白屏

- 确认已安装 **WebView2 Runtime**。  
- 重启后再试；仍失败可将现象反馈给发布者（版本号、Win 版本、是否有杀软拦截）。

### 6.2 启动提示「需要 OpenSSH 客户端」或连接失败找不到 ssh

- 应用启动时会检测 `ssh.exe`；缺失会弹出说明对话框（含安装步骤与帮助链接）。  
- 确认 `ssh -V` 可用（见 §1.1），然后**重新打开** AnchorTerm。  
- 确认主机、端口、用户名与密钥路径正确。  
- 加密私钥须在弹窗中输入正确口令。

### 6.3 升级版本后配置还在吗？

- 主机列表与凭据在用户目录/系统凭据库，**升级安装一般不删除**。  
- 建议覆盖安装同一产品；勿手动删除 `%APPDATA%\AnchorTerm` 除非要重置。

### 6.4 是否需要 Node.js / Rust？

- **不需要。** 安装包是独立可执行程序，仅依赖 WebView2 与系统 OpenSSH。

---

## 7. 相关文档

| 文档 | 读者 |
|------|------|
| [README.md](../README.md) | 产品能力与使用说明 |
| [RELEASE.md](RELEASE.md) | 发布者：如何打出安装包 |
| [DEPLOYMENT.md](DEPLOYMENT.md) | 部署方案与阶段规划 |

---

*若从源码自行构建，请改看 [RELEASE.md](RELEASE.md) 与 README「从源码开发」。*
