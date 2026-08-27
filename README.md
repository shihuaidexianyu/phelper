# phelper

面向 HP OMEN 笔记本的轻量性能控制与硬件遥测工具。

phelper 只关注性能、功耗、散热和可观测性：把 CPU/GPU/风扇等状态集中展示，并通过能力探测、单写入者、校验、心跳和退出恢复控制硬件状态。它不是 OMEN Gaming Hub 的完整替代品，也不包含商城、账号、云服务或 RGB 编辑器。

> 当前只对 **OMEN Gaming Laptop 16-wf0032TX（SKU 81L09PA，board 8BAB）** 做过完整验证。其他 OMEN/Victus 机型不在承诺支持范围内；未知 board 默认保持只读。

## 功能

- CPU：温度、功率、有效频率、利用率、EPP（AC/DC）、E-core EPP、最大频率和 Boost 策略
- GPU：温度、功率、利用率、频率、显存和功耗上限
- 散热：CPU/GPU 风扇转速、HP Thermal Mode、最大风扇、手动风扇和软件风扇曲线
- 配置档：内置 `silent`、`balanced`、`gaming`、`cpu-max`，以及用户 TOML 配置档
- 帧遥测：可选 PresentMon provider，提供 FPS、1% Low、帧时间和显示延迟
- 桌面端：Rust + GPUI，单实例、管理员权限、最小化到托盘、快速启动
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

PresentMon 是可选 provider，不影响其他遥测和控制路径。只有显式设置目标进程 PID 时才会附加：

```powershell
$env:PHELPER_PRESENTMON_PID = "12345"
$env:PHELPER_PRESENTMON_DLL = "C:\path\to\PresentMonAPI2.dll" # 非标准安装位置时才需要
```

具体限制见 [`docs/presentmon-integration.md`](docs/presentmon-integration.md)。

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

## 项目结构

```text
crates/phelper-domain/   与平台无关的策略、命令、状态和端口
crates/phelper-core/     Engine、遥测、控制、能力探测和持久化
crates/phelper-cli/      开发/验证 CLI
apps/desktop/            GPUI 桌面端
docs/                    专项调研和 provider 说明
architecture.md          架构基线与安全不变量
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
- PresentMon 仍需要用户提供 PID；自动进程选择、游戏生命周期跟踪和 benchmark 导出尚未完成
- MUX 显卡模式切换暂不提供：它需要重启，且不影响当前性能控制闭环；只保留必要的只读状态/能力记录
- `0x29` CPU 功耗限制仍属于实验性能力，不包含在内置配置档中
