<p align="center">
  <img src="apps/desktop/assets/phelper.svg" width="112" height="112" alt="phelper">
</p>

# phelper

phelper 是给 HP OMEN 16-wf0032TX 写的性能控制工具。它能看 CPU、GPU 和风扇状态，也能切换几组常用的性能配置。程序用 Rust 和 GPUI 编写，平时可以收在系统托盘里运行。

目前只支持下面这台机器：

```text
OMEN Gaming Laptop 16-wf0032TX
SKU 81L09PA / board 8BAB
Intel Core i9-13900HX
NVIDIA GeForce RTX 4060 Laptop GPU
Windows 11
```

其他 OMEN 或 Victus 型号没有验证过。桌面端会拒绝在未知主板上启用控制功能。

## 界面

现在有三个页面：

- 概览：CPU、GPU、左右风扇的实时状态
- 配置档：应用内置性能配置
- 设置：开机启动

内置配置档：

| 名称 | 作用 |
| --- | --- |
| `silent` | 安静、省电 |
| `balanced` | 恢复均衡设置 |
| `gaming` | 游戏和 GPU 性能优先 |
| `cpu-max` | CPU 持续性能优先，风扇全速 |

风扇按机身位置叫左风扇和右风扇。两个通道由固件分别校准，所以转速有一点差别是正常的。

点窗口关闭按钮只会把 phelper 收进托盘。要真正退出程序，请使用托盘菜单里的“退出”，这样风扇和相关控制会先恢复到安全状态。

## 安装

安装包生成在：

```text
dist\phelper-Setup-0.1.0.exe
```

安装到 Program Files 后，开始菜单会出现 phelper；桌面快捷方式是可选的。卸载程序会删除 phelper 自己的开机任务，但不会删除 `%LOCALAPPDATA%\phelper` 里的日志和配置。

更新或卸载前，先从托盘退出正在运行的 phelper。

安装包目前没有数字签名，Windows SmartScreen 可能会给出警告。

## 编译

需要 Windows 11、Rust 工具链、Visual Studio C++ Build Tools 和正常安装的 NVIDIA 驱动。

编译桌面程序：

```powershell
cargo build -p phelper-desktop --release
.\target\release\phelper-desktop.exe
```

编译安装包需要 Inno Setup 6：

```powershell
.\installer\build-installer.ps1 -Version 0.1.0
```

如果 Release exe 已经是最新的，可以跳过 Rust 编译：

```powershell
.\installer\build-installer.ps1 -SkipBuild -Version 0.1.0
```

应用图标、PawnIO 模块和运行资源都已嵌入 exe，不需要在程序旁边放额外的 assets 目录。

## CLI

`phelper-cli` 主要用于开发和硬件验证。第一次检查设备时先跑只读命令：

```powershell
cargo run -p phelper-cli -- probe
cargo run -p phelper-cli -- telemetry --duration 30
cargo run -p phelper-cli -- control status
```

配置档命令：

```powershell
cargo run -p phelper-cli -- control profile list
cargo run -p phelper-cli -- control profile show balanced
cargo run -p phelper-cli -- control profile apply balanced --hold 120
```

实验性的 `0x29` 功耗限制只存在于显式启用 feature 的 CLI 构建中：

```powershell
cargo run -p phelper-cli --features experimental -- control power-limits --pl1 45 --pl2 90 --hold 120
```

这项功能不会出现在内置配置档或桌面 UI 中。

## 自定义配置档

CLI 会从这里读取 TOML 配置档：

```text
%LOCALAPPDATA%\phelper\profiles\*.toml
```

可以先导出一个内置配置档作为模板：

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\phelper\profiles" | Out-Null
cargo run -p phelper-cli -- control profile export gaming > "$env:LOCALAPPDATA\phelper\profiles\my-gaming.toml"
```

桌面端目前只显示内置配置档。

## 数据目录

运行数据放在 `%LOCALAPPDATA%\phelper`：

| 路径 | 内容 |
| --- | --- |
| `profiles\*.toml` | 自定义配置档 |
| `state\fan_curve.toml` | 最近保存的软件风扇曲线 |
| `state\control-journal.jsonl` | 控制和恢复记录 |
| `logs\phelper-desktop.log` | 桌面端日志 |

## 开发

```text
crates/phelper-domain/   领域模型和端口
crates/phelper-core/     遥测、控制、安全检查和平台实现
crates/phelper-cli/      开发与验证命令行
apps/desktop/            GPUI 桌面程序
installer/               Inno Setup 安装包
docs/                    调研和实机验证记录
```

依赖方向是：

```text
phelper-domain <- phelper-core <- desktop / CLI
```

提交前运行：

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

这是硬件控制项目，单元测试不能代替实机验证。新的写入命令需要先完成只读探测，再在 8BAB 上检查读回和退出恢复。更详细的约束见 [architecture.md](architecture.md) 和 [8BAB 可行性调研](docs/feasibility-16-wf0032TX.md)。

## License

项目代码使用 MIT 或 Apache-2.0 双许可证。内嵌 PawnIO 模块的许可证见 `assets/pawnio/COPYING`。
