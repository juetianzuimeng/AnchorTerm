# Shell Integration（OSC 7）安装说明

AnchorTerm 优先通过 **OSC 7** 获取远端工作目录，以便断线重连后静默 `cd`。  
若未安装 integration，客户端仍会尝试解析你提交的 `cd` / `pushd` 命令（能力较弱）。

## Bash

将下列片段加入远端 `~/.bashrc`（或 `~/.bash_profile`）：

```bash
# AnchorTerm: report cwd via OSC 7
__anchorterm_cwd() {
  printf '\e]7;file://%s%s\e\\' "${HOSTNAME:-localhost}" "$PWD"
}
# Preserve existing PROMPT_COMMAND
if [[ -n "${PROMPT_COMMAND:-}" ]]; then
  PROMPT_COMMAND="__anchorterm_cwd;${PROMPT_COMMAND}"
else
  PROMPT_COMMAND="__anchorterm_cwd"
fi
```

使配置生效：

```bash
source ~/.bashrc
```

## Zsh

加入 `~/.zshrc`：

```zsh
# AnchorTerm: report cwd via OSC 7
autoload -Uz add-zsh-hook
__anchorterm_cwd() {
  printf '\e]7;file://%s%s\e\\' "${HOST:-localhost}" "$PWD"
}
add-zsh-hook precmd __anchorterm_cwd
```

```zsh
source ~/.zshrc
```

## 验证

在 AnchorTerm 中连接后执行：

```bash
cd /tmp
```

状态栏应出现 cwd `/tmp`（或你的路径）。  
断网重连后执行 `pwd`，应回到该目录（目录仍存在时）。

## 注意

- 阶段 1 **不会**自动修改远端 rc 文件，需手动安装。
- OSC 7 使用 UTF-8 路径；中文目录名已支持。
- 子 shell / `sudo -i` 等若未加载 integration，cwd 可能暂时不准。
