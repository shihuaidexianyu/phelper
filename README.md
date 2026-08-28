# phelper

面向 HP OMEN 笔记本的轻量性能控制与硬件遥测工具。

phelper 只关注性能、功耗、散热和可观测性：把 CPU/GPU/风扇等状态集中展示，并通过能力探测、单写入者、校验、心跳和退出恢复控制硬件状态。它不是 OMEN Gaming Hub 的完整替代品，也不包含商城、账号、云服务或 RGB 编辑器。

> 当前只对 **OMEN Gaming Laptop 16-wf0032TX（SKU 81L09PA，board 8BAB）** 做过完整验证。其他 OMEN/Victus 机型不在承诺支持范围内；未知 board 默认保持只读。

## 功能

- CPU：温度、功率、有效频率、利用率，以及 Windows PPM 的 EPP、E-core EPP、频率上限、性能上下限和 AC/DC Boost
- GPU：温度、功率、利用率、频率、显存和功耗上限
- 散热：CPU/GPU 风扇转速、HP Thermal Mode、最大风扇、手动风扇和软件风扇曲线
- 应用调度：按进程/线程控制 P/E 核 CPU Sets、Affinity、QoS、优先级、内存优先级、理想处理器和下次启动的 GPU 首选项
- 自动调度：可选的 BatteryEfficiency；确认电池供电后，对安全过滤的当前用户进程使用 E-core CPU Sets + EcoQoS，并在交流/退出时恢复
- 配置档：内置 `silent`、`balanced`、`gaming`、`cpu-max`，以及用户 TOML 配置档
- 桌面端：Rust + GPUI，单实例、管理员权限、最小化到托盘、快速启动；可选当前用户登录自启、OMEN 键映射和轻量悬浮窗
- 开发/验证 CLI：能力探测、实时遥测、控制命令和硬件验证工具

## 安全边界

phelper 的控制 core 不把 UI 当作安全边界。所有写入都会经过能力检查和参数校验，并由单一 `ControlCoordinator` 串行执行。

- 启动阶段默认只读，不会因为加载上次曲线而接管风扇
- 只有本会话实际写过风扇或 Thermal Mode，退出时才会尝试恢复
- 手动风扇和软件曲线由 phelper 的心跳维持；停止程序后由固件接管
- 恢复失败会保留失败状态，不会伪报“已恢复”
- 风扇曲线是软件策略，不是假装从硬件读回的固件曲线
- 不提供任意 EC 写入；实验性 CPU 功耗限制始终受编译开关和运行时能力双重限制
- 控制日志写入本地 JSONL，便于定位实际写入和恢复结果

在本机上，固件自动模式可能在低温空闲时让风扇停转。这是固件行为；phelper 不会在“只查看”或“未接管退出”时主动写入 `0,0` 造成额外停转。

## 环境

- Windows 11
- Rust toolchain：仓库中的 `rust-toolchain.toml`（当前为 Rust 1.98.0）
- 支持 MSVC 的 Visual Studio C++ Build Tools
- NVIDIA GPU 遥测需要正常安装 NVIDIA 驱动
- 需要管理员权限：桌面端会通过 UAC 自动提权

当前支持范围锁定在：

```text
OMEN Gaming Laptop 16-wf0032TX / SKU 81L09PA
Intel Core i9-13900HX
NVIDIA GeForce RTX 4060 Laptop GPU
board 8BAB
```

## 构建和运行

在仓库根目录执行：

```powershell
# 桌面端，包含已验证的实验性抽屉
cargo build -p phelper-desktop --release --features experimental
.\target\release\phelper-desktop.exe
```

不编译实验性 CPU 功耗限制功能：

```powershell
cargo build -p phelper-desktop --release --no-default-features
```

构建产物为 `target\release\phelper-desktop.exe`。PawnIO 的 `IntelMSR` 和 `IntelMCHBAR` 模块已经作为编译期资源嵌入 core，不需要把仓库中的 `assets` 目录复制到 exe 旁边。

构建 Windows 安装包（需要 Inno Setup 6 的 `ISCC.exe`）：

```powershell
.\installer\build-installer.ps1
```

安装包输出到 `dist\phelper-Setup-0.1.0.exe`，默认安装到 `Program Files\phelper`，并提供开始菜单入口和可选桌面快捷方式。安装包只部署桌面 exe；用户配置、配置档、日志和控制日志保留在 `%LOCALAPPDATA%\phelper`，卸载时不会删除。

## CLI

CLI 是开发和硬件验证工具，不是产品 UI。建议第一次先做只读探测：

```powershell
cargo run -p phelper-cli -- probe
cargo run -p phelper-cli -- telemetry --duration 30
cargo run -p phelper-cli -- control status
```

配置档操作：

```powershell
cargo run -p phelper-cli -- control profile list
cargo run -p phelper-cli -- control profile show balanced
cargo run -p phelper-cli -- control profile export balanced
cargo run -p phelper-cli -- control profile apply balanced --hold 120
```

Windows 软件策略也可以逐项调整。`--ac` 和 `--dc` 只修改对应电源轨，未指定的一侧保持不变：

```powershell
cargo run -p phelper-cli -- control epp --ac 20 --dc 60
cargo run -p phelper-cli -- control min-perf --ac 20 --dc 5
cargo run -p phelper-cli -- control max-perf --ac 100 --dc 80
cargo run -p phelper-cli -- control boost --ac aggressive --dc efficient-enabled
```

OS 级应用调度不启动硬件控制引擎，目标明确写成 PID 或 TID，并在 CLI 进程结束前自动恢复：

```powershell
cargo run -p phelper-cli -- os topology
cargo run -p phelper-cli -- os processes
cargo run -p phelper-cli -- os apply --pid 1234 --cpu performance --qos high --process-priority above-normal --hold 120
```

电源感知自动调度默认关闭。先用只读状态确认供电上下文；实机验证时使用有边界的
保持时间，退出会恢复自动接管的进程策略：

```powershell
cargo run -p phelper-cli -- os auto status
cargo run -p phelper-cli -- os auto battery --hold 120
```

桌面端对应的是“应用”页。常用的 P/E 核、QoS、优先级、内存和 GPU 首选项直接可选，Affinity、CPU Set ID 和理想处理器放在“高级”。完整边界见 [`docs/windows-os-policy.md`](docs/windows-os-policy.md)。

这里的“软件策略”不是一个模式按钮：EPP 是偏好，性能上下限是 PPM 范围约束，
频率上限是 MHz 天花板，Boost 是睿频策略。它们写入当前 Windows 活动电源计划的
AC/DC 索引，并在写后立即读回校验。Windows 设置中的“节能/均衡/性能”高层选择和
实际生效模式只做只读显示，不会被 phelper 静默切换；这几层可能同时存在不同值。
具体分层、边界和官方 API 见 [`docs/windows-power-policy.md`](docs/windows-power-policy.md)。

风扇和 Thermal Mode 控制会占用硬件控制权，请确认目标机器和参数后再执行：

```powershell
cargo run -p phelper-cli -- control fan manual --cpu 3000 --gpu 3000 --hold 120
cargo run -p phelper-cli -- control fan max --on --hold 120
cargo run -p phelper-cli -- control thermal performance --hold 120
cargo run -p phelper-cli -- control fan auto
```

实验性命令需要显式启用 feature：

```powershell
cargo run -p phelper-cli --features experimental -- control power-limits --pl1 45 --pl2 90 --hold 120
```

`--hold 120` 用于保持心跳；正常 Ctrl+C 或到期会走优雅退出路径。硬件验证 spike 命令只应按项目 runbook 在 reference machine 上执行。

## 用户数据

默认目录为 `%LOCALAPPDATA%\phelper`：

| 路径 | 内容 |
| --- | --- |
| `profiles\*.toml` | 用户配置档 |
| `state\fan_curve.toml` | 最近一次明确应用的曲线，仅作为下次编辑来源 |
| `state\control-journal.jsonl` | 控制写入和恢复日志 |
| `settings.toml` | UI 设置 |
| `logs\phelper-desktop.log` | 桌面端运行日志 |
| `reports\` | 诊断报告导出 |

用户配置档使用严格 TOML 解析，未知字段会被拒绝。可以先导出内置配置档作为模板：

```powershell
cargo run -p phelper-cli -- control profile export gaming > "$env:LOCALAPPDATA\phelper\profiles\my-gaming.toml"
```

然后按需修改。配置档中的字段都是“希望改变的值”，没有填写的字段保持当前状态；实验性 `power_limits` 不会绕过 core 的安全门。
带有 `[os_policy]` 的配置档必须配合明确的 PID/TID 使用；硬件 `profile apply` 不会猜测应用目标。

## 项目结构

```text
crates/phelper-domain/   与平台无关的策略、命令、状态和端口
crates/phelper-core/     Engine、遥测、控制、能力探测和持久化
crates/phelper-cli/      开发/验证 CLI
apps/desktop/            GPUI 桌面端
docs/                    专项调研和 provider 说明
architecture.md          架构基线与安全不变量
docs/automatic-scheduling-architecture.md
                         自动调度专项架构与 Phase 1 实现边界
docs/resident-desktop-integrations.md
                         自启、OMEN 键映射和悬浮窗的边界与验收
```

core 不依赖 GPUI。桌面端只是读取 `AppState` 并提交命令，硬件访问、控制顺序、验证、心跳和恢复都由 core 负责。

## 验证

提交前建议运行：

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-features
cargo clippy --workspace --all-features -- -D warnings
cargo test --workspace --all-features
cargo build -p phelper-desktop --release --features experimental
```

完整架构、安全不变量和硬件证据记录在 [`architecture.md`](architecture.md)；reference machine 的可行性边界见 [`docs/feasibility-16-wf0032TX.md`](docs/feasibility-16-wf0032TX.md)。

## 当前边界

- 目前不是通用 OMEN/Victus 控制器，只服务于 8BAB reference platform
- 风扇当前读回的是实时 RPM/level，不是硬件内部的温度到转速曲线
- Windows PPM 的细粒度参数已经进入 core、CLI 和性能页；Boost 仍可通过 profile/CLI
  设置，界面只在“更多参数”中展示不常用的性能上下限，避免把主页面做成参数表
- Windows OS 级应用调度已经进入 core、CLI 和“应用”页；CPU Sets 用 Windows 拓扑的
  efficiency class 区分 P/E 核，Affinity 只作为显式高级选项
- 电源感知自动调度已实现 `Off` / `BatteryEfficiency` 第一版；默认关闭，当前不持久化，
  仍需完成 reference machine 的功耗和兼容性 A/B/HIL 验证；设计边界见
  [`docs/automatic-scheduling-architecture.md`](docs/automatic-scheduling-architecture.md)
- 不追踪游戏进程，也不提供帧率/帧时间采集；监控范围限定为硬件和 Windows 系统指标
- MUX 显卡模式切换暂不提供：它需要重启，且不影响当前性能控制闭环；只保留必要的只读状态/能力记录
- `0x29` CPU 功耗限制仍属于实验性能力，不包含在内置配置档中
- 常驻桌面集成已实现自启、OMEN 键事件桥和悬浮窗；OMEN 实体键仍需在 reference machine
  上完成实际按键 HIL 验证，未验证的设备不会假报支持
