# phelper

phelper 是一个面向特定 HP OMEN 笔记本的轻量性能控制与硬件遥测工具，使用 Rust 和 GPUI 构建。

它只做三件事：展示必要的 CPU、GPU 和风扇状态，通过少量经过验证的配置档切换性能策略，以及在系统托盘中可靠常驻。它不是 OMEN Gaming Hub 的完整替代品，也不包含账号、商城、云服务、RGB、游戏库等功能。

> [!WARNING]
> 当前产品只支持 **OMEN Gaming Laptop 16-wf0032TX（SKU 81L09PA，board 8BAB）**。桌面端和控制 Engine 会拒绝未知主板；只读探测命令能够运行，不代表设备受到支持。

## 当前界面

桌面端刻意保持最小化，目前只有三个页面：

- **概览**：CPU 温度、功率、利用率，GPU 温度、功率、利用率，以及左/右风扇转速和当前风扇模式。
- **配置档**：应用四个内置配置档。它是桌面端唯一的写入入口。
- **设置**：只包含开机启动开关。

风扇按机身物理位置标为左、右。8BAB 固件会分别校准两个通道，因此同型号风扇的实时转速不必完全相同。

| 配置档 | 用途 |
| --- | --- |
| `silent` | 安静省电，低速风扇曲线 |
| `balanced` | 均衡性能与散热 |
| `gaming` | 游戏优先，使用性能风扇曲线 |
| `cpu-max` | 持续性能优先，风扇全速运行 |

桌面端设置页只保留“开机启动”。托盘只提供“显示/隐藏 phelper”和“退出”；主题设置、OMEN 键映射、悬浮窗、应用调度页面、精细参数页面和诊断页面不属于当前桌面产品。

## 常驻与开机启动

- 点击窗口关闭按钮只会隐藏主窗口，遥测、控制心跳和安全保护继续工作。
- 左键单击托盘图标会重新显示主窗口；托盘菜单也可显式显示或隐藏。
- 只有托盘菜单中的“退出”才会停止程序，并走完整的硬件安全恢复流程。
- 在设置页开启“开机启动”后，phelper 使用当前用户的 Windows 登录任务，以最高运行级别和 `--background` 参数启动；登录时不弹主窗口，也不会出现登录后的 UAC 确认框。
- 再次运行程序只会唤醒已有窗口，不会启动第二个硬件控制实例。

开机启动默认关闭。它只操作名为 `phelper-user-logon` 的任务计划；关闭开关或卸载程序时会删除这个任务，不修改其他软件或 HP 的任务。

## 支持平台

完整验证环境：

```text
Windows 11
OMEN Gaming Laptop 16-wf0032TX
SKU 81L09PA / board 8BAB
Intel Core i9-13900HX
NVIDIA GeForce RTX 4060 Laptop GPU
```

其他 OMEN/Victus 型号目前不在支持范围内。不要仅根据品牌、CPU 或 GPU 型号推断兼容性。

## 构建与运行

需要：

- 仓库中 `rust-toolchain.toml` 指定的 Rust 工具链
- Visual Studio C++ Build Tools（MSVC）
- Windows 11
- 正常安装的 NVIDIA 驱动

在仓库根目录执行：

```powershell
cargo build -p phelper-desktop --release
.\target\release\phelper-desktop.exe
```

首次手动启动时，桌面端会通过 UAC 请求管理员权限。启用开机启动后，Windows 任务计划程序会在当前用户登录时直接以最高权限后台启动。命名互斥量始终阻止多个实例同时控制硬件。

PawnIO 的 `IntelMSR` 和 `IntelMCHBAR` 模块已经嵌入可执行文件，无需在程序旁复制 `assets` 目录。

## 安全模型

UI 不是安全边界。所有 HP 固件和 Windows PPM 性能写入都必须经过 core：

- 启动后先探测设备身份和能力，未知能力视为不支持。
- `ControlCoordinator` 是唯一硬件性能写入者，所有命令串行执行。
- 配置档在写入前完成整体验证，某一步失败后不会继续执行后续步骤。
- 请求状态、实际读回状态和实时遥测彼此独立；API 返回成功不等于硬件验证成功。
- 手动风扇和软件风扇曲线依赖心跳维持；托盘退出、Windows 注销或系统关闭时恢复固件控制。关闭主窗口只是隐藏，不会触发恢复。
- 温度数据冻结、心跳连续失败或验证不确定时按 fail-closed 处理。
- 不提供任意 EC、MSR 或 MCHBAR 写入接口；PawnIO 只用于受限的只读遥测。
- 每次控制及恢复结果写入本地 JSONL 日志。

正常 CLI 同样不能跳过退出恢复。需要心跳的命令必须使用大于零的 `--hold`；Ctrl+C 或保持时间结束都会进入优雅退出路径。

## 工程 CLI

`phelper-cli` 是开发、诊断和硬件验证工具，不是产品 UI。

第一次检查设备时，优先使用只读命令：

```powershell
cargo run -p phelper-cli -- probe
cargo run -p phelper-cli -- telemetry --duration 30
cargo run -p phelper-cli -- control status
```

配置档管理：

```powershell
cargo run -p phelper-cli -- control profile list
cargo run -p phelper-cli -- control profile show balanced
cargo run -p phelper-cli -- control profile export balanced
cargo run -p phelper-cli -- control profile apply balanced --hold 120
```

core 和 CLI 还保留 Windows PPM 参数、风扇/Thermal Mode、进程与线程调度以及电源感知自动调度等工程能力。这些能力不会自动出现在精简桌面端。使用前请阅读：

- [Windows 电源策略](docs/windows-power-policy.md)
- [Windows 进程与线程调度](docs/windows-os-policy.md)
- [自动调度架构](docs/automatic-scheduling-architecture.md)

实验性 `0x29` CPU 功耗限制只存在于显式启用 feature 的 CLI 构建中：

```powershell
cargo run -p phelper-cli --features experimental -- control power-limits --pl1 45 --pl2 90 --hold 120
```

这不是推荐的日常入口，也不会进入内置配置档。

## 自定义配置档

桌面端只展示内置配置档。自定义 TOML 配置档由 core 和 CLI 从以下目录加载：

```text
%LOCALAPPDATA%\phelper\profiles\*.toml
```

可以导出内置配置档作为模板：

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\phelper\profiles" | Out-Null
cargo run -p phelper-cli -- control profile export gaming > "$env:LOCALAPPDATA\phelper\profiles\my-gaming.toml"
```

解析采用严格模式：未知字段会被拒绝，损坏的文件会产生警告但不会阻止 Engine 加载其他配置档。自定义文件不能覆盖同名内置配置档。

## 用户数据

默认目录为 `%LOCALAPPDATA%\phelper`：

| 路径 | 内容 |
| --- | --- |
| `profiles\*.toml` | CLI 可用的自定义配置档 |
| `state\fan_curve.toml` | 最近一次明确应用的软件风扇曲线，仅用于后续编辑 |
| `state\control-journal.jsonl` | 控制、校验和恢复记录 |
| `logs\phelper-desktop.log` | 桌面端运行日志 |

卸载或删除可执行文件不会自动删除这些数据。

## 项目结构

```text
crates/phelper-domain/   与平台无关的领域模型、命令、状态和端口
crates/phelper-core/     Engine、能力探测、遥测、控制、安全和平台适配
crates/phelper-cli/      开发与验证 CLI
apps/desktop/            只包含概览和配置档的 GPUI 桌面端
docs/                    专项设计、调研和硬件验证记录
architecture.md          架构基线与安全不变量
```

依赖方向为：

```text
phelper-domain <- phelper-core <- desktop / CLI
```

`phelper-domain` 不依赖 Win32、WMI、PawnIO、NVIDIA API 或 GPUI。桌面端只消费 `AppState` 并提交领域命令，不直接访问硬件。

## 验证

提交前运行：

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo build -p phelper-desktop --release
```

单元测试和静态检查不能替代真实硬件验证。任何新的硬件写入都必须依次经过只读探测、开发命令、8BAB 实机读回验证和安全恢复验证。

## 明确不做

- 不承诺支持 8BAB 之外的设备。
- 不直接写 EC。
- 不提供 MUX 显卡模式切换。
- 不追踪游戏进程，不采集 FPS 或帧时间。
- 不把实验性功耗限制包装成稳定功能。
- 不在没有明确需求时加入主题、热键、悬浮窗或更多控制页面；设置页只保留开机启动，托盘只保留显示/隐藏和退出。

完整架构和硬件证据见 [architecture.md](architecture.md) 与 [8BAB 可行性调研](docs/feasibility-16-wf0032TX.md)。

## License

项目代码使用 MIT 或 Apache-2.0 双许可证。仓库内嵌的第三方 PawnIO 模块及其许可证声明以 `assets/pawnio/COPYING` 为准。
