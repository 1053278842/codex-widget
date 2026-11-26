# 安装与构建

## 直接装（推荐）

从 [Releases](https://github.com/1053278842/codex-widget/releases) 下载 `Codex Widget_x.y.z_x64-setup.exe`，双击安装。

- 当前用户安装，**不需要管理员权限**，装在 `%LOCALAPPDATA%\Codex Widget\`；
- 装完不会出现桌面图标，只在系统托盘留一颗琥珀色的点；
- 卸载走系统「应用和功能」里的 `Codex Widget`。

## 从源码构建

前置：

| 依赖 | 版本 | 说明 |
| --- | --- | --- |
| [Rust](https://rustup.rs) | 1.77+ | `rustup-init` 装 stable 即可 |
| Node.js | 18+ | 只用它构建前端 |
| MSVC 生成工具 | VS 2022 Build Tools | Tauri 在 Windows 上要用它链接 |
| WebView2 运行时 | 任意 | Win10/11 自带，不用装 |

```bash
git clone https://github.com/1053278842/codex-widget.git
cd codex-widget
npm install

npm run app:dev      # 开发模式（前端热更新，改 Rust 会重新编译）
npm run app:build    # 产出安装包：src-tauri/target/release/bundle/nsis/
```

`npm run app:dev` 首次会编译 Rust 依赖，约 1–2 分钟；之后增量编译十几秒。

## 不打扰真实 Codex 地调试

想看动效又不想真的让 agent 跑一轮，用假数据驱动：

```bash
# 终端 A：生成一轮 12 秒后完成的任务
node tools/fake-session.mjs --dir .tmp/codex --run-seconds 12

# 终端 B：让控件读这个假目录
CODEX_WIDGET_HOME=.tmp/codex npm run app:dev
```

`--scenario abort` 演示中断态。

想立刻看到全屏提醒又不想真等十分钟，把阈值调短：

```bash
CODEX_WIDGET_LONG_SECS=5 npm run app:dev
```

DSH 那边同样可以注入假目录：`CODEX_WIDGET_DSH_HOME=<某个 .dsh 目录>`。

## 环境变量一览

| 变量 | 作用 |
| --- | --- |
| `CODEX_WIDGET_HOME` | 覆盖 Codex 数据目录（默认 `~/.codex`），用于假数据调试 |
| `CODEX_WIDGET_DSH_HOME` | 覆盖 DSH 数据目录（默认 `~/.dsh`） |
| `CODEX_WIDGET_LONG_SECS` | 覆盖全屏提醒阈值（秒），绕过托盘设置 |

## 数据与配置放在哪

| 路径 | 内容 |
| --- | --- |
| `%APPDATA%\io.github.codexwidget\settings.json` | 托盘里的开关、阈值、胶囊位置 |
| `%APPDATA%\io.github.codexwidget\whale.png` | 可选的鲸鱼图形（见 [usage](usage.md#装那头鲸鱼可选)） |
| `%LOCALAPPDATA%\Codex Widget\` | 程序本体 |