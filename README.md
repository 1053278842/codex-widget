# Codex Widget

一个 100px 宽的 Windows 桌面悬浮控件：**看一眼就知道 Codex 还在跑、跑了多久、什么时候跑完了。**

它常驻在屏幕右下角、默认悬停于所有窗口之上，空闲时完全不可见。Codex 开始一轮任务时它浮出来，用一颗缓慢呼吸的琥珀色圆点 + 等宽计时器表示"还在干活"；任务结束时胶囊横向展开成 `完成 3:12`，亮一次绿色光环，同时弹出 Windows 通知，8 秒后自己收回、重新消失。

![Codex Widget 的五种状态](docs/preview.png)

```
运行中   ● 12:34          ← 琥珀点呼吸，数字每 0.5s 走一次
完成     ✓ 完成 3:12      ← 绿色，横向展开，8s 后收回并隐藏
中断     ● 中断 0:41      ← 红色，同样展开，等待你点一下
并发     ● 12:34  ×3      ← 三个线程同时在跑时，显示最早那个的时长
```

> 英文简介见文末 [English](#english)。

---

## 它解决什么

VSCode 里让 Codex 改一个跨文件的功能，然后你切去浏览器查资料。**问题从来不是"它跑完了没有"，而是"我不知道它跑完了没有"**——于是每隔两分钟切回编辑器看一眼，或者干脆一直盯着。

这个控件把"当前是否有任务在跑、跑了多久"变成余光就能扫到的信息：一颗会呼吸的点。你不需要切窗口，也不需要读任何文字。

## 状态一览

| 状态 | 视觉 | 停留 |
| --- | --- | --- |
| 运行中 | 琥珀色圆点，2.4s 呼吸一次（缩放 + 光圈），等宽字体计时 | 直到任务结束 |
| 完成 | 胶囊横向展开为 `完成 3:12`，绿色，一次性光环扩散；同时发 Windows 通知 | 8s，或点击立即收回 |
| 中断 | 同上但为红色 `中断 0:41` | 8s，或点击立即收回 |
| 并发 | 在时间后追加 `×N`，时间取**最早开始**的那个线程（= 机器被占用多久） | 同运行中 |
| 空闲 | **完全隐藏**，并且鼠标可穿透（不会挡住底下的窗口） | — |
| 疑似卡住 | 15 分钟没有任何新事件 → 圆点停止呼吸、颜色变暗 | 同运行中 |

## 工作原理

Codex 的 CLI / VSCode 扩展在 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` 里逐行追加结构化事件，控件只是**只读地跟随这些文件**，不注入、不 hook、不改任何东西：

| 事件 | 用途 |
| --- | --- |
| `session_meta` | 工作目录、模型、provider —— 用来判断"哪个工作区" |
| `event_msg / task_started` | 一轮任务的开始时间（unix 秒） |
| `event_msg / task_complete` | 结束时间，**自带 `completed_at`，所以耗时是精确的，不靠外部掐表** |
| `event_msg / turn_aborted` | 中断 |
| `token_usage_record`、`token_count` | 心跳：有新字节就说明这轮还活着 |
| `event_msg / user_message` | 取第一句作为任务的简短标题 |

两个工程细节值得写下来，因为它们都是踩出来的：

1. **按字节偏移增量读取，不看文件修改时间。** 实测中一个正在被追加的 rollout 文件能在 4 秒内增长 3160 字节，而它的 `LastWriteTime` 纹丝不动。所以"变了没有"只能靠 size/offset 判断。
2. **启动时从文件两端读。** 想要元数据（cwd / 模型 / 首句）得读**文件头**；想知道当前这轮是死是活，得读**文件尾**——而文件尾可能在几十 MB 之后。所以：头部读 64KB，尾部从末尾按 256KB 分块**往回扫**，直到撞上最近一个 `task_started` / `task_complete` / `turn_aborted`。

监控范围是**所有工作区的所有线程**：每次轮询扫描最近 4 天的日期目录，把每一轮的状态汇总成一个快照推给界面。

## 安装

从 [Releases](../../releases) 下载 `Codex Widget_x.y.z_x64-setup.exe`，双击安装（当前用户安装，无需管理员）。

首次启动后它会缩在右下角（任务栏上方），**空闲时不显示任何东西**——图标在系统托盘里，一颗琥珀色的点。

## 使用

- **拖动**：运行中的胶囊可以直接按住拖走，位置会记住到下次启动。*（位置记忆在 0.2 的计划里）*
- **点击**：完成 / 中断状态下点一下，立刻收回。
- **托盘右键**：
  - `显示控件` —— 关掉后任何任务都不再浮出（只保留托盘图标）
  - `完成时弹出 Windows 通知` —— 通知开关。关掉后只看颜色和动画
  - `开机自启` —— 写入当前用户的启动项
  - `退出`

控件本身没有面板、没有设置页、没有菜单——托盘右键就是全部设置。这是刻意的：一个 30px 高的东西不该有自己的窗口。

## 开发

前置：[Rust 工具链](https://rustup.rs)、Node.js 18+、MSVC 生成工具、WebView2 运行时（Win10/11 自带）。

```bash
npm install
npm run app:dev      # 开发模式（热更新）
npm run app:build    # 产出安装包：src-tauri/target/release/bundle/nsis/
```

### 不打扰真实 Codex 地调试

用假数据驱动，避免为了看动效而真的让 agent 跑一轮：

```bash
# 终端 A：生成一轮 12 秒后完成的任务
node tools/fake-session.mjs --dir .tmp/codex --run-seconds 12

# 终端 B：让控件读这个假目录
CODEX_WIDGET_HOME=.tmp/codex npm run app:dev
```

`--scenario abort` 演示中断态，`--scenario multi` 演示 `×N`。

### 项目结构

```
src/                 渲染层（原生 TS，无框架）
  main.ts            状态机：idle / run / done / aborted + 8s 收尾
  styles.css         全部视觉，含呼吸动画与 A′ 展开
src-tauri/
  src/main.rs        托盘、窗口机制、设置持久化、命令
  src/watcher.rs     rollout jsonl 的增量跟随与 turn 状态机
tools/
  fake-session.mjs   合成事件流，用于演示与测试
  make-icons.ps1     用 System.Drawing 生成 app / tray 图标
```

## 已知限制

- **不是官方 API。** 它读的是 Codex 自己写的日志，属于私有格式。已验证于 Codex CLI `0.169.0-alpha.11.1` + VSCode 扩展 `26.1111.21308`，并在 100 个真实会话、单个 27MB 的 rollout 文件上做过回归。上游改格式时这里会读不到——它会显式降级为"什么也不显示"，而不是乱猜。
- **"疑似卡住"是启发式。** 15 分钟无新事件即视为停滞。一个长思考、无工具调用的回合有可能被误判为暗色；事件一到就恢复呼吸。
- **只在 Windows 上验证过。** 代码里没有 Windows 特有的硬编码，但 macOS / Linux 的托盘、开机自启、通知行为未测试。
- **开发态运行通知会署名 "Windows PowerShell"**——因为进程没有注册的 AppUserModelID 兜底。用安装包装好后署名与图标都正确（`Codex Widget` + 自己的图标）。

## English

**Codex Widget** is a ~100px always-on-top capsule for Windows that tells you whether a Codex task is still running, how long it has been running, and when it finishes — without switching windows.

An amber dot breathes while a turn is in flight, a monospace timer ticks beside it, the capsule expands sideways into `完成 3:12` when the turn lands (with a one-shot green halo and an optional Windows toast), then collapses and disappears 8 seconds later. Idle means invisible *and* click-through.

It reads `~/.codex/sessions/**/rollout-*.jsonl` read-only, following the same `task_started` / `task_complete` / `turn_aborted` records the CLI writes — byte-offset incremental tailing, never file mtimes.

```bash
npm install && npm run app:dev
node tools/fake-session.mjs --dir .tmp/codex   # synthetic turn, no agent required
```

## License

MIT