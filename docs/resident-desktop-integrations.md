# phelper 常驻桌面集成架构

状态：已审查，Phase A～E 已实现；实机 HIL 待完成
日期：2026-08-28
> 历史设计文档：自启、OMEN 键映射和悬浮窗实现已从当前精简桌面端移除。
> 本文仅保留设计证据，不代表当前产品具备这些功能；重新引入必须重新评审和验收。

适用平台：Windows 11，reference platform = OMEN 16-wf0032TX / board 8BAB

本文设计三项用户能力：

1. 开机自启；
2. OMEN 实体键的特殊映射；
3. 不打开主窗口即可查看关键数据的悬浮窗。

本文是实现和验收契约。实现必须满足本文的状态、权限、单实例、失败和验收约束。

## 1. 产品目标

phelper 应该是一个常驻的性能控制层：

```text
Windows 登录
    ↓
phelper 在托盘运行
    ├── core 继续拥有硬件控制与遥测
    ├── OMEN 键触发一个明确的用户动作
    └── 悬浮窗按需显示关键状态
```

用户不需要理解 WMI、心跳、EPP、风扇控制器或事件转发。界面只表达：

- 自启是否开启；
- OMEN 键会做什么；
- 悬浮窗是否显示；
- 当前 profile 和关键实时数据。

不把内部实现状态写进正常 UI。错误只在确实影响用户动作时展示。

## 2. 非目标

本次不做：

- 游戏识别、PresentMon、帧率和帧时间；
- 读取所谓“硬件自己的温度-风扇曲线”；
- 通过 OMEN 键直接构造任意 WMI/EC payload；
- 全局低级键盘钩子拦截所有键；
- OGH DLL 伪装、注入或复制 OmenSuperHub 代码；
- 让 OMEN 键直接绕过 `ControlCoordinator` 写硬件；
- 把 phelper 拆成 UI、硬件守护进程和事件进程三个常驻进程；
- 首版加入复杂宏录制、任意程序自动化或可编程脚本。

## 3. 当前基础与缺口

### 3.1 已有基础

当前仓库已经具备：

- `AppHandle::start_fast()`：引擎在 app-pump 后台启动，UI 可以先显示结构；
- `AppHandle::state()`：所有页面读取同一个 `AppState` 快照；
- `TelemetrySnapshot`：CPU、GPU、风扇、电源和 Windows 指标已经有统一来源；
- `ControlCoordinator`：所有硬件写操作的单写者；
- profile 应用路径：OMEN 键切 profile 必须复用这条路径；
- Windows 单实例、管理员权限和托盘生命周期；
- `%LOCALAPPDATA%\phelper\settings.toml`：现有 UI 设置持久化入口。

### 3.2 本次新增前的缺口（已补齐）

本次实现前桌面端还没有：

- Task Scheduler 自启安装、查询和删除；
- OMEN 事件源能力探测和事件转发；
- OMEN 键动作解析器；
- 独立的 topmost / no-activate / click-through 浮窗；
- 这三项能力的持久化设置和最小 UI。

## 4. 开源参考的结论

### 4.1 OmenSuperHub

OmenSuperHub 的自启使用 Task Scheduler，代码创建了系统启动任务和用户登录任务，并设置最高运行级别、允许电池供电启动和不设执行时限。[Program.Config.cs](https://github.com/breadeding/OmenSuperHub/blob/master/Program.Config.cs#L198-L265)

它的 OMEN 键动作不是普通虚拟键监听，而是由外部事件路径触发，再通过 `NamedPipeServerStream` 通知主程序；动作包括浮窗、应用、快捷键、profile 和禁用。[Program.OmenKey.cs](https://github.com/breadeding/OmenSuperHub/blob/master/Program.OmenKey.cs#L105-L133) [Program.OmenKey.cs](https://github.com/breadeding/OmenSuperHub/blob/master/Program.OmenKey.cs#L868-L889)

它的 FloatingForm 使用无边框、置顶、不出现在任务栏、透明分层窗口和 `WS_EX_NOACTIVATE`，并将内容锚定到屏幕工作区。[FloatingForm.cs](https://github.com/breadeding/OmenSuperHub/blob/master/FloatingForm.cs#L20-L33) [FloatingForm.cs](https://github.com/breadeding/OmenSuperHub/blob/master/FloatingForm.cs#L174-L227)

### 4.2 OmenHwCtl

OmenHwCtl 的公开说明把 OMEN 键描述为持久化的 WMI event filter + Task Scheduler 任务，而不是普通键盘输入；它也要求以管理员运行。[OmenHwCtl README](https://github.com/GeographicCone/OmenHwCtl/blob/master/README.md)

### 4.3 对 phelper 的取舍

参考项目证明了行为方向，但不直接复制实现：

| 能力 | 参考行为 | phelper 决定 |
|---|---|---|
| 自启 | Task Scheduler | 保留；使用当前用户可见会话的登录任务 |
| OMEN 键 | HP 事件 → 任务/pipe → 主程序 | 保留事件思路；先探测再安装，不假设事件存在 |
| 悬浮窗 | 独立透明置顶窗口 | 保留；数据来自现有 `AppState`，不重复采集 |
| 硬件写入 | 参考项目各自实现 | 仍只允许进入 phelper 的 `ControlCoordinator` |
| 配置 | 注册表和较多全局状态 | 使用现有 TOML，保持职责分组和可迁移 |

OmenSuperHub 是 GPLv3 项目；本仓库只参考公开行为、协议事实和交互边界，不复制代码或类结构。[OmenSuperHub](https://github.com/breadeding/OmenSuperHub)

## 5. 总体架构

```text
                         Windows / HP event source
                                    │
                             Resident event bridge
                         (capability probe + debounce)
                                    │
                         ResidentEvent::OmenKeyPressed
                                    │
                  ┌─────────────────┴─────────────────┐
                  │                                   │
             Action resolver                     Overlay controller
                  │                                   │
       ┌──────────┼──────────┐                 AppState snapshot
       │          │          │                       │
  ToggleOverlay  NextProfile  SendShortcut      GPUI/Win32 popup
       │          │          │
       │          └── AppHandle::dispatch(ApplyProfile)
       └───────────── overlay command
```

硬件写入路径不能从事件桥直接进入平台适配器：

```text
OMEN key
  → event bridge
  → action resolver
  → AppHandle / application command
  → ControlCoordinator
  → HP WMI / Windows PPM / NVIDIA
```

悬浮窗只读 `AppState`，不拥有 WMI、PawnIO、NVAPI 或控制句柄。

## 6. 领域模型与职责

### 6.1 `phelper-domain`

增加纯数据类型：

```rust
pub enum OmenKeyAction {
    /// 不安装 phelper 自定义事件桥接，保留系统/固件默认行为。
    Default,
    ToggleOverlay,
    NextProfile,
    SendShortcut,
}

pub enum OverlayPosition {
    TopLeft,
    TopRight,
}

pub struct ResidentSettings {
    pub autostart: bool,
    pub omen_key: OmenKeySettings,
    pub overlay: OverlaySettings,
}
```

领域层不出现 HWND、Task Scheduler COM、WMI event query 或 GPUI 类型。

### 6.2 `phelper-core`

负责：

- 常驻设置的读取、校验和默认值；
- OMEN 键动作解析；
- profile 循环候选的确定；
- 将 `NextProfile` 转换成已有 `ApplyProfile` 命令；
- 对外提供 `ResidentSnapshot`，表示能力和当前配置；
- 将事件桥报告的能力错误转换成可展示的只读状态。

core 不负责创建窗口，也不直接操作用户界面。

### 6.3 `phelper-core::platform` / desktop Windows 适配

负责 Windows 实现：

- Task Scheduler 自启任务；
- HP OMEN 事件源探测和安装/卸载；
- named pipe 或本地事件的安全通信；
- HWND 样式、屏幕工作区和浮窗位置。

### 6.4 `apps/desktop`

负责：

- 设置页的三个最小入口；
- 托盘菜单的“显示悬浮窗”；
- 悬浮窗绘制和状态刷新；
- 将 OMEN 键动作映射到 `AppHandle` 或 overlay controller。

页面不得自己枚举进程、打开硬件句柄或写 Windows/HP 参数。

## 7. 开机自启

### 7.1 用户语义

设置只有一个用户可理解的开关：

```text
开机启动：关 / 开
```

开启后，phelper 在用户登录后以 `--background` 参数驻留托盘，不自动弹出主窗口。
悬浮窗是否显示由悬浮窗自己的设置和 OMEN 键动作决定。

新安装默认关闭，避免用户尚未验证硬件控制时就自动启动。

### 7.2 任务定义

首版只创建一个当前用户登录任务：

```text
Task name       phelperUserLogon
Trigger         At logon, current user
Run level       Highest
Logon type      Interactive token
Action          absolute path to phelper-desktop.exe --background
Working dir     not required; data paths are absolute
Power/timeout   no phelper-specific restriction; verify on reference machine
```

不照搬 OmenSuperHub 的 SYSTEM boot task。原因是当前 phelper 是 UI + core 单进程：

- SYSTEM 任务可能运行在 Session 0，主窗口和浮窗对当前用户不可见；
- boot task 和 logon task 会引入两个进程，和现有单实例/单写者模型冲突；
- phelper 目前没有独立硬件 service，不需要登录前操作硬件。

如果未来拆出硬件 service，再单独设计 boot service；本任务不预留隐式行为。

### 7.3 幂等与清理

启用时：

1. 只操作固定的 `phelperUserLogon`；
2. 通过 `schtasks /Create /F` 幂等地创建或更新自己的任务；
3. 创建/删除命令返回失败时，将原因写入 `ResidentSnapshot`；
4. 不扫描、不修改其他任务；
5. 将结果写入 `ResidentSnapshot`。

禁用时只删除 `phelperUserLogon`，绝不删除其他任务、注册表启动项或 HP 任务。

exe 路径必须使用当前正在运行的绝对路径，不能写入相对路径、临时目录或开发构建目录的猜测值。

OMEN 事件 consumer 由 `LocalSystem` 执行，因此它比普通自启有更高的安装约束：只有位于
已存在且规范化的 `Program Files` 子目录中的 exe 才允许安装事件桥。开发目录、桌面目录和
临时目录会被拒绝；这不是自启失败，普通自启仍可使用当前 exe 的绝对路径。

任务创建失败不影响正常手动启动，UI 只显示“无法设置开机启动”及可执行的原因。

### 7.4 启动顺序

```text
解析启动参数（普通 / `--background` / signal-only）
  →
ensure_elevated
  → single_instance_guard
  → load UI/resident settings
  → start_fast app-pump
  → install tray
  → asynchronously reconcile autostart and OMEN event bridge
  → 普通启动创建主窗口；background 启动创建隐藏窗口维持消息泵
```

自启设置和事件桥不能阻塞首屏，也不能在 UI 线程同步等待 WMI/Task Scheduler。

## 8. OMEN 键特殊映射

### 8.1 先探测，不猜测

OMEN 键在不同机型上可能由不同的 HP System Event / WMI provider 负责。普通 `RegisterHotKey` 或键盘钩子不能证明笔记本实体 OMEN 键可用。

因此能力状态必须是：

```text
Unknown → Probing → Supported / Unsupported / Error
```

只有确认当前设备存在可用事件源后，设置页才允许启用特殊映射。无法确认时显示“此设备未检测到 OMEN 键事件”，不显示假开关。

### 8.2 事件桥

事件桥分为两个阶段：

1. **只读探测**：查询可用的 HP event provider 及其事件字段，不修改系统；
2. **明确启用**：用户选择非 `Default` 动作后，才安装或更新 phelper 自己的事件转发任务。

在 OmenSuperHub 的 reference 实现中，事件过滤器监听 `root\wmi` 的
`hpqBEvnt`，条件是 OMEN 键对应的 `eventData=8613` 和 `eventId=29`，再由
`root\subscription` 下的永久 WMI consumer 触发 pipe 通知。这个查询只能作为
8BAB 的待验证候选，不能直接推广到其他 board。[OmenHardware.cs](https://github.com/breadeding/OmenSuperHub/blob/master/OmenHardware.cs)

事件任务只负责发送一个无参数事件：

```text
OmenKeyPressed
```

它不能携带 WMI command、EC 地址、任意程序参数或硬件 payload。

主进程通过受 ACL 保护的本地 IPC 接收事件。实现不能照搬参考项目的
`cmd /c echo ... > pipe` consumer，而应使用固定参数的 signal-only 入口；该入口
只连接 pipe 后退出，绝不初始化 core、提权或写硬件。

WMI 订阅使用 phelper 自己的唯一名称，例如 `phelper-8bab-OmenKeyFilter` 和
`phelper-8bab-OmenKeyConsumer`。启用前先检查是否已有 OGH、OmenSuperHub 或其他
同类订阅；遇到未知 owner 时拒绝安装，禁用时只删除 phelper 自己创建并能核验
归属的 filter、consumer 和 binding。事件桥启动、停止或断开时，不得写风扇、功耗
或 profile。

动作恢复为 `Default` 时，如果 provider 可用，先清理 phelper 自己可能遗留的
filter、consumer 和 binding；清理失败只报告常驻能力错误，不继续安装任何桥接。

桥接 pipe 只授予 `SYSTEM` 和当前用户，拒绝远程客户端，不向所有交互式用户开放。pipe
断开或创建失败时，worker 会在保留 WMI 订阅的前提下重建 pipe；停止时先停止接受事件，
再删除 phelper 自己的 binding、filter 和 consumer。这样临时断线不会变成永久失效，
也不会留下无法解释的系统级订阅。

### 8.3 动作集合

首版保留真正有用的动作：

| 动作 | 行为 |
|---|---|
| 默认 | 不接管 OMEN 键，让 HP 默认行为继续 |
| 显示/隐藏悬浮窗 | 切换轻量数据浮窗 |
| 下一个 profile | 在用户选定的 profile 列表中循环，并走 `ApplyProfile` |
| 发送快捷键 | 发送用户明确录入且经过校验的标准快捷键 |

默认配置为 `Default`。用户启用特殊映射后才改变硬件事件的处理路径；本项目不
承诺通过自定义事件桥接禁用 HP 自己的默认按键行为。

### 8.4 快捷键校验与去抖

- 只允许有限的修饰键 + 一个主键；
- 禁止空快捷键、重复修饰键和未识别键码；
- `SendInput` 失败要记录 Win32 错误，但不重试造成按键连发；
- 相同 OMEN 事件在 300 ms 内只接受一次；
- pipe 断线自动重连，重连不重复执行上一次动作；
- profile 切换失败只产生普通控制结果，不改变 OMEN 键配置。

### 8.5 兼容性降级

首版不把 F24、低级键盘钩子作为笔记本 OMEN 键的默认 fallback。它们只能在后续明确证明适用于某类设备后，作为独立 capability 加入；否则会产生误拦截普通快捷键的风险。

## 9. 悬浮窗

### 9.1 用户语义

悬浮窗是“看一眼就走”的状态面板，不是第二个主界面。默认隐藏，可通过：

- 托盘菜单；
- OMEN 键映射；
- 后续明确设计的快捷方式；

打开和关闭。

### 9.2 窗口约束

悬浮窗必须满足：

```text
Topmost             true
ShowInTaskbar       false
Activate            false
Accept keyboard     false
Click-through       true
Border              none
Background          transparent / theme-compatible
Position            primary-screen top-left or top-right
```

它不能抢前台窗口焦点，不能阻断游戏或编辑器输入，也不能在任务栏生成第二个应用入口。

GPUI 若无法直接提供全部窗口样式，则由 Windows 适配层在窗口创建后设置 HWND 样式；这不改变页面和 core 的职责边界。

### 9.3 内容边界

固定显示以下信息：

```text
当前 profile
CPU  温度 / 功耗 / 利用率
GPU  温度 / 功耗 / 利用率
风扇 当前转速
电源 AC / 电池与电量
```

不显示：

- EPP、P 核/E 核响应、WMI command、provider 名称；
- 心跳、保持/恢复、journal、验证细节；
- 折线图、诊断列表和内部错误码；
- 没有实际数据时的“处理中”“引擎运行中”等占位文案。

单个数据缺失显示 `—`；只有影响整体可用性的错误才显示简短的状态提示。

颜色必须有明确区分：CPU 使用暖橙色，GPU 使用紫色或蓝紫色，风扇使用青绿色；颜色不承载“是否安全”的唯一语义，数值和单位始终保留。

### 9.4 数据路径与刷新

浮窗不创建自己的硬件采集器：

```text
TelemetryCoordinator
        ↓
TelemetrySnapshot
        ↓
AppState
        ↓
OverlayView
```

实现要求：

- CPU/GPU 显示使用已有 snapshot 的最新样本；
- 风扇更新服从既有低频采样，不为了浮窗提高 WMI 访问频率；
- 浮窗最多按 250 ms 刷新界面；
- snapshot 未变化时不触发浮窗重绘；
- 首次打开没有有效数据时显示结构和 `—`，数据到达后局部更新；
- 浮窗关闭后不触发自己的 UI 重绘，但不停止 core 遥测。

### 9.5 持久化

首版只持久化必要设置：

```toml
[resident]
autostart = false

[resident.omen_key]
action = "default"
shortcut = ""
profile_cycle = []

[resident.overlay]
visible_on_start = false
position = "top_left"
screen = "primary"
```

不首版提供字体、透明度、每个指标独立开关、背景模糊、动态颜色等设置。这些选项会把一个查看窗口变成新的配置系统，当前没有必要。

配置缺失使用默认值；未知字段按现有设置策略告警并回退，不执行未知动作。

## 10. 设置页与托盘入口

设置页只增加一个“常驻”卡片，三组即可：

```text
开机启动       [开关]
OMEN 键        [动作选择]
悬浮窗         [启动时显示/隐藏]  [左上 / 右上]
```

悬浮窗当前是否显示不做成常驻大卡片；托盘菜单直接提供“显示/隐藏悬浮窗”。

OMEN 键动作选择只有动作名和当前值，不解释 WMI、pipe、Task Scheduler 等实现细节。能力不可用时整行禁用，并给出一句真正有用的原因。

## 11. 生命周期与安全

### 11.1 正常启动

```text
解析启动参数（普通 / `--background` / signal-only）
  →
提权/单实例
  → 读取设置
  → 快速启动 app-pump
  → 创建托盘
  → 普通启动创建主窗口；background 启动创建隐藏窗口维持消息泵
  → 后台探测并同步自启/OMEN 事件桥
  → 第一份 snapshot 到达后允许浮窗显示数据
```

自启任务或 OMEN 事件桥的慢操作不能挡住主窗口的骨架渲染。

### 11.2 正常退出

```text
隐藏并释放浮窗
  → 停止 OMEN 事件桥
  → AppHandle::shutdown()
  → ControlCoordinator / KeepAliveService 恢复安全状态
  → 进程退出
```

自启任务不会在退出时删除；用户关闭自启才删除。退出恢复仍由现有 AR-12 路径负责。

### 11.3 崩溃与断线

- 事件桥崩溃：OMEN 键变为不可用，core 和硬件恢复路径不受影响；
- 浮窗崩溃：主窗口和 core 不应随之退出；
- autostart 查询失败：不重复创建任务，不修改硬件；
- pipe 收到未知消息：丢弃并记录，不执行命令；
- 任何外部事件都不能直接写 HP WMI、Windows PPM 或 EC。

### 11.4 权限与 IPC

- Task Scheduler、事件订阅和 OMEN provider 修改只允许管理员执行；
- pipe 使用本机限定名称和显式 ACL，只接受当前用户/当前 elevated phelper；
- IPC 协议是固定事件枚举，不接受任意命令字符串；
- 快捷键发送只允许经过校验的虚拟键集合；
- 禁止通过配置文件注入 exe 路径、WMI payload 或 EC 地址。

## 12. 实现阶段与状态

### Phase A — 只读探测与领域模型（已实现）

- 增加 `ResidentSettings`、`OmenKeyAction`、`OverlaySettings`；
- 为配置补充 round-trip、未知动作和默认值测试；
- 实现 OMEN event provider 的只读 capability probe；
- 记录当前 8BAB 的真实结果，不因参考项目存在就标记支持。

### Phase B — 开机自启（已实现）

- 实现只属于 phelper 的 Task Scheduler 任务创建/更新/删除；
- 接入启动后的异步 reconcile；
- 增加启动参数和“任务不存在不误判权限错误”的单元测试；
- 登录后无重复进程、无额外 UAC 弹窗仍待真实机器验证。

### Phase C — 悬浮窗（已实现）

- 先做独立窗口生命周期和 HWND 样式；
- 复用 `AppState`，不增加硬件采集路径；
- 只展示 §9.3 的必要数据；
- DPI、多屏、全屏程序、焦点和点击穿透仍待 HIL 验证。

### Phase D — OMEN 键事件桥（已实现；实机 HIL 待完成）

- 在 capability probe 有证据后安装事件转发；
- 通过固定 `OmenKeyPressed` 事件接入 action resolver；
- 先实现 ToggleOverlay，再实现 NextProfile 和 SendShortcut；
- 对未支持设备隐藏/禁用配置，不做假成功。

### Phase E — UI 与整机验收（代码已实现；HIL 待完成）

- 设置页只接入三个必要入口；
- 托盘菜单接入浮窗显示；
- 做冷启动、登录、自启关闭、OMEN 键、profile、退出恢复的 HIL 测试；
- 更新 README 和当前能力表。

## 13. 验收标准

### 自启

- 开启后重启并登录，phelper 只出现一个进程；
- 以足够权限运行，不重复弹 UAC；
- 主窗口默认不抢前台，托盘可显示；
- 关闭“开机启动”后任务被删除，其他任务不受影响；
- exe 移动或升级后再次启用能修正任务路径。

### OMEN 键

- 当前机器只有在真实事件能力探测成功后才显示可用；
- 连按 20 次不会出现明显重复触发或漏触发；
- ToggleOverlay 不触碰硬件；
- NextProfile 只走现有 profile / coordinator 路径；
- `Default` 不会误发送快捷键；
- 事件桥停止后 core 仍能正常控制和退出恢复。

### 悬浮窗

- 打开速度不依赖重新初始化硬件 provider；
- 不抢焦点、不出现在任务栏、不拦截鼠标和键盘；
- CPU/GPU 颜色和单位可区分；
- 数据缺失显示 `—`，不出现无信息的“处理中”；
- 主窗口关闭/最小化到托盘时浮窗行为符合设置；
- phelper 退出时浮窗消失，硬件仍走现有恢复流程。

## 14. 回滚方案

任何一项出现不确定行为时，按以下顺序关闭：

1. 将 OMEN 键动作恢复为 `Default`；
2. 卸载 phelper 自己的事件任务/IPC bridge；
3. 关闭悬浮窗；
4. 删除 phelper 自己的登录任务；
5. 保留现有硬件控制和 AR-12 退出恢复，不回滚其他 profile 或 Windows 策略。

回滚操作不调用任意 WMI 命令，也不删除 HP/OGH 的系统任务。

## 15. 实现审查结论

在开始写代码前必须确认：

- 自启是 desktop lifecycle，不是硬件 control；
- OMEN 键事件是外部输入，动作解析后才进入 application command；
- profile 切换仍只有一个硬件写者；
- 悬浮窗只消费 `AppState`，不产生新的 provider；
- 当前架构继续保持单进程，避免 SYSTEM Session 0 和双写者问题；
- OMEN event capability 的真实存在性必须先通过只读探测确认；
- 任何未验证的事件 provider、键码或任务触发条件都只能是 `Unknown/Unsupported`。

以下内容记录当时的验收目标，不代表当前代码状态；物理 OMEN 键行为仍保持为
待 HIL 结论，不在文档或 UI 中假报成功。
