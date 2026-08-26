# 可行性调研报告 — HP OMEN Gaming Laptop 16-wf0032TX

> 日期：2026-08-25
> 调研方式：6 路并行一手资料核查（Linux hp-wmi 主线源码、OmenMon/OmenMon-Reborn/OmenHwCtl 源码与文档、OmenSuperHub/OmenCore 源码与 issue、PawnIO/PresentMon/NVAPI 官方资料、GPUI/Windows Rust 生态、HP 官方规格与社区实测记录）
> 原则：所有结论只针对 **OMEN Gaming Laptop 16-wf0032TX（SKU 81L09PA）**；不为其他型号做泛化。每条结论标注证据等级。

---

## 0. TL;DR

**项目可行。** 文档（architecture.md）的架构骨架与协议大方向经受住了一手资料检验：传输层、thermal V1 映射、风扇 WMI 路径、MUX、EPP、PawnIO MSR 遥测全部有多方独立验证，且**目标机型本身就是 OmenSuperHub 的开发平台、并被 Linux 内核主线明确支持（board 8BAB）**。

但有 **12 处文档内容需要修正**（§5），**1 个文档没有的架构级概念必须新增**（固件夺回控制权 → keep-alive 子系统，§4-R1），以及 **1 个只能实机回答的控制面**（0x29 功率限制，§3）。

---

## 1. 目标机型核实

| 项目 | 结论 | 证据等级 |
|---|---|---|
| SKU | OMEN Gaming Laptop 16-wf0032TX = 81L09PA | 高（HP 官方） |
| CPU | **i9-13900HX**（Raptor Lake-HX，24C/32T，CPUID model 0xB7） | 高（ZOL + 多家零售商一致；HP 论坛"i7"说法无出处，已排除） |
| GPU | **RTX 4060 Laptop 8GB（AD107）**，TGP ~120W（未见 SKU 级实测，评测推断） | 高 / TGP 中 |
| 内存/存储/屏幕 | 16GB DDR5（2×8）、1TB PCIe4、16.1" QHD 240Hz | 高 |
| MUX | 系列级确认支持 MUX + Advanced Optimus（OGH Graphics Switcher，Hybrid/Discrete，需重启） | 高（系列）/ 中（本 SKU） |
| **Board ID** | **`8BAB`**——三方独立证据：内核 patch 实机测试（bug 220639）、LKML 16-wf0008la dmidecode、OmenMon issue #37 | 高（仍建议实机复核，见 §6） |
| DMI 产品串 | "OMEN by HP Gaming Laptop 16-wf0xxx" | 高 |
| BIOS | F.xx 家族（F.21 经 Windows Update 推送确认）；OmenMon 在 8BAB 上显示过 "76.44"，未解释 | 中 |
| "OMEN 9" 命名 | 暗影精灵9 = 2023 OMEN 16（16-wf Intel / 16-xf AMD）= 中国市场份额名；文档用 "OMEN 9" 指代本机系列是准确的 | 高 |

**周边 board ID 警示**（防混淆，来自实测记录）：`8C77/8C78/8C76` = 16-wf**1**xxx（2024 刷新款）；`8BCA` = 16-**x**f0xxx（AMD 兄弟机型，ACPI 有缺陷）。**OmenCore 的 board→model 数据库对本系列标注有误**（8BAB 错标 wf1xxx、8BCA 错标 wf0xxx Intel，且 UserVerified=false）——不可作为我方 capability 数据源。

---

## 2. 协议传输层（已锁定，三方一致）

Linux 内核、OmenMon、OmenHwCtl、OmenSuperHub 完全一致：

- WMI：`root\wmi`，class `hpqBIntM`（实例 `ACPI\PNP0C14\0_0`），方法 `hpqBIOSInt{0,4,128,1024,4096}`（按输出大小选），输入类 `hpqBDataIn{Command, CommandType, hpqBData, Size, Sign}`。
- GUID `5FB7F034-2C63-45E9-BE91-3D44E2C707E4`；签名 `"SECU"`（`0x55434553` LE）。
- 命令组：`0x20008`（游戏主命令组）、`0x20009`（键盘灯光——**对本机无效**：16-wf0032TX/81L09PA 为 1 区白色背光键盘、无 RGB 硬件，背光开关走 EC/FN 键，2026-08-25 HP 规格页+实机确认）、`0x01`（Legacy 读）、`0x02`（GpuMode 写）。
- 返回码：0=OK，2=签名错误，3=未知命令，4=未知 cmdtype，5=参数无效（内核）；OmenMon 另观察到 1/4/6/46。
- 新固件读操作要求 **insize=0**（内核 `zero_if_sup` 自动检测）——Rust 传输层要照做这个探测。
- Rust 实现：`wmi` crate（v0.18，活跃）`exec_instance_method` 可直接调方法；数组/嵌套参数的文档与 release notes 有出入，需早期 spike 验证，`exec_method` 原始路径兜底。

---

## 3. 逐子系统可行性矩阵

### 遥测（Read）

| 子系统 | 路径 | 判定 | 依据 |
|---|---|---|---|
| CPU 封装/核心温度 | PawnIO → MSR 0x19C/0x1B1/0x1A2 | ✅ 可行 | IntelMSR.p 读白名单全覆盖；LHM 在 Raptor Lake（含 model 0xB7）实测 |
| CPU 封装功率 | RAPL 0x606+0x611 差分 | ✅ 可行 | 同上；OmenCore 用同一组 MSR |
| Effective clock | APERF/MPERF（0xE8/0xE7） | ✅ 可行 | 同上 |
| CPU/内存/磁盘/网络利用率 | Windows PDH/PerfLib | ✅ 可行 | windows-rs 覆盖；无需提权 |
| GPU 温度/利用率/频率 | NVAPI 公开 surface（NVML 备选） | ✅ 可行 | PresentMon NVAPI provider 用同一组调用 |
| **GPU 功率** | **NVAPI `ClientPowerTopologyGetStatus`（未公开 surface）** | ⚠️ 可行但有保留 | **NVML `nvmlDeviceGetPowerUsage` 在 AD107 返回 NOT_SUPPORTED（NVIDIA 官方论坛确认）**；ClientPower* 被 HWiNFO/LHM/OmenSuperHub 长期使用，在同平台实测有瓦数；空闲时读数可能不稳（0W/卡在 ~47.5W），负载下可靠。`nvapi-sys` 已有绑定 |
| GPU 降频原因 | `NvAPI_GPU_GetPerfDecreaseInfo` | ⚠️ 基本可行 | AD107 mobile 上逐 bit 覆盖未验证；NVML event reasons 作辅助 |
| 风扇 RPM | WMI 0x2D | ✅ 可行 | 内核在 8BAB 实测"fan RPMs are readable"；V1 机型返回值为 100-RPM 粒度（level×100） |
| 帧数据（FPS/frametime/latency） | PresentMon 服务（MIT） | ⚠️ 可行，有集成成本 | 需装 Windows 服务（管理员）+ 运行时动态加载 PresentMonAPI2.dll（named-pipe IPC）；**无 Rust binding**（扁平 C API，自包一层）；**其 GPU power 指标走 NVML，本机不可用**。备选：`ferrisetw` 自收 DXGI ETW，省服务但需自建事件关联 |

### 控制（Write）

| 子系统 | 路径 | 判定 | 依据 |
|---|---|---|---|
| EPP（AC/DC 分离） | PowrProf `PowerWriteAC/DCValueIndex`，PERFEPP `36687f9e-…-15eb381c6863` | ✅ 可行 | GUID 双源确认；windows-rs 覆盖；13900HX 有 SpeedShift（EPP 生效前提）；**写入需提权** |
| 最大频率/电源方案 | PowrProf 同上 | ✅ 可行 | 同上 |
| Thermal mode（Balanced 0x30 / Performance 0x31） | WMI 0x1A，payload `{0xFF, mode}` | ✅ **内核在 8BAB 实测** | commit 13fa3aaf02；8BAB 静态使用 V1 值，无需运行时版本探测 |
| Thermal mode **回读** | EC 偏移 0x59（本板布局） | ⚠️ 决策点 | BIOS 无查询接口（OmenMon 文档明示）；与"不碰 EC"原则冲突——见 §5-4 的处置建议 |
| Max Fan | WMI 0x27 | ✅ 可行 | 内核使用中；0x26 回读**不可靠**（内核已弃用，改为自追踪状态） |
| 手动风扇转速 | WMI 0x2E `{cpu_rpm, gpu_rpm}`，**单位 100 RPM，0=自动**；范围用 0x2F 风扇表 clamp | ✅ **内核在 8BAB 实测可控** | 同 commit；社区在 16-wf0xxx 独立逆出同一协议（CachyOS 帖子：type 46 = 0x2E 手动，type 26 = 0x1A 恢复自动） |
| GPU 平台功耗策略（cTGP/PPAB/dstate） | WMI 0x21/0x22 | ✅ 可行 | 内核 victus_s 路径（8BAB 归属此路径）在 profile 切换时写入；payload 四字节结构确认 |
| MUX（Hybrid/Discrete/Optimus） | WMI 0x52（读 cmd 0x01 / 写 cmd 0x02），SDD byte7 bit3 能力门控 | ✅ 可行 | 内核 2026-07 加入 + OmenMon/OmenHwCtl 均实现；**需重启，非热切换** |
| **CPU 功率限制（PL1/PL2/PL4/并发）** | WMI 0x29 | ❓ **最大未验证点** | 内核**从不在此板写显式值**（只在 AC/DC 事件恢复 DEFAULT）；OmenSuperHub/OmenHwCtl 有实现但**字节序互相矛盾**（内核结构 `{pl1,pl2,pl4,cc}` vs OSH 发 `{PL2,PL1,…}`）；OmenMon #37 记录固件在 OGH 退出后把 CPU 锁回 55W。必须按文档 §25 的三步验证法实机实测 |
| PL4 / ICC max | WMI 0x37（OmenSuperHub 用） | ❓ Tier C | 单项目来源 |
| 电压调节 | — | 🚫 不做 | 与文档一致；OmenCore 的 NVAPI 电压 offset 不采用 |

---

## 4. 风险登记册

**R1 — 固件夺回控制权（firmware clawback）★ 最高优先，文档缺失**
- 证据：OmenMon issue #37（8BAB 真机）：OGH 退出 → 风扇锁 auto + CPU 锁 55W。内核：手动风扇 120s 固件超时，驱动每 90s keep-alive，且 0x10（fan count get）的注释明确"调用它会启用/维持用户自定义 thermal/fan 状态"。OmenMon-Reborn 为此加了 BIOS heartbeat；OmenSuperHub 做了 OGH 伪装。
- 结论：**keep-alive 必须是一级架构组件**（`KeepAliveService`：周期性 0x10 心跳 + 状态重断言），而不是 fan curve 的附属逻辑。它同时服务 AR-12 的反面——app 退出时**停止**心跳即自然归还固件控制，这正是我们要的 fail-safe。

**R2 — V1/V2 风扇协议尺度混淆**
- V1（本机，2023）：krpm 尺度（级别×100=RPM，上限 ~55 级）；V2（2024+）：百分比 0–100。OmenCore 实测：给 V1 固件发百分比指令导致崩溃。
- 结论：capability model 里 `FanScale::{Krpm, Percent}` 必须由 board ID 决定；本机锁 `Krpm`。

**R3 — EC 写风扇在本板会锁死**
- 8BAB 在 OmenMon-Reborn `HasMaxFanFreeze` 黑名单（EC 被写 100% 手动风扇后锁死，转速反而比 idle 还慢）。
- 结论：即使未来开 `experimental-ec`，**8BAB 上 EC 风扇写也应硬编码禁止**。EC 只读诊断可保留（thermal mode 回读、温度传感器）。

**R4 — 0x29 未验证 + 字节序冲突** — 见 §3。Phase 0/2 的实机验证是唯一出路。

**R5 — GPU 功率依赖 NVAPI 未公开 surface** — 十年稳定但无合同保证；需 `MetricQuality::Estimated` 标注来源质量，空闲读数异常时降权显示。

> **⚠️ 2026-08-25 实机修订（M1 验收，Tier A 证据）**：R5 的两个前提在本机被推翻。
> (1) 论坛结论"NVML 在 AD107 功率 NOT_SUPPORTED"**不适用于本机 + 驱动 581.x**：`nvidia-smi` 报告连续可信功率（睡眠 1.81W → CUDA memset 满载 61.67W → 回落 8.39W），经引擎集成后逐 tick 可读（75s 运行 n=148，min 1.7W / max 64.4W）。
> (2) `ClientPowerTopologyGetStatus` 在本机**恒报 `num_entries=0`**（调用成功、空闲与满载皆然）——它在这台机器上什么都不是，不是"空闲不稳"。
> 结论反转：**GPU power 权威源 = NVML `nvmlDeviceGetPowerUsage`**；ClientPowerTopology 降为声明式 fallback。architecture.md §12/§29 与决议表已同步修订。

**R6 — PawnIO 运维成本** — 需管理员安装签名驱动；**FACEIT 反作弊封禁 PawnIO.sys**（证书被外挂共用）——玩 FACEIT 游戏的用户需在文档/UI 提示。驱动本身 Secure Boot/HVCI 兼容；GPL+例外条款，ioctl 使用不污染我方许可证。无官方 Rust crate，但已有 ~200 行 windows-rs 集成先例（cpu-temp）。

**R7 — GPUI 生态维护税** — 生态靠 pin git commit 运转（crates.io 版停滞 10 个月；gpui-component 每日提交、6 个月未打 tag、有破坏性变更先例）；GPUI **无系统托盘**（配 `tray-icon`）；**无软件渲染回退**（GPU 驱动异常时 app 起不来的风险要在本机早期验证）；Windows 渲染后端仍在变动（DX11→Vulkan/wgpu）。

**R8 — PresentMon 集成成本** — 服务安装（管理员）+ 无 Rust binding；其价值集中在帧数据（FPS/1% low/latency），GPU 硬件遥测在本机由我们自己的 NVAPI 路径覆盖更可靠。可以把 PresentMon 集成为 Phase 5 的可插拔 provider，失败降级为"无帧数据"而非硬依赖。

**R9 — 提权模型** — EPP 写入、PawnIO、PresentMon 服务、WMI 写都需要/最好有管理员权限。建议 app manifest `requireAdministrator`（单进程架构下最简方案），只读遥测子集未来可拆非提权模式。

---

## 5. architecture.md 修正清单

| # | 位置 | 现状 | 修正 |
|---|---|---|---|
| 1 | §2.2 reference platform | "HP OMEN 9 / i9-13900HX / RTX 4060" | 内容正确，但应写明完整 SKU：**OMEN Gaming Laptop 16-wf0032TX（81L09PA），预期 board ID 8BAB**（Phase 0 实机复核）；明确"只支持这一台" |
| 2 | §12 metric ownership | GPU power：NVML primary / PresentMon fallback | **改为 NVAPI（ClientPowerTopology）primary，无 fallback**；NVML 在本机不支持功率读数；PresentMon 的 GPU power 走 NVML 同样不可用 |
| 3 | §22 命令表 | 0x26 Max Fan Get "Very High" | **降级**：内核标记不可靠（Victus S 固件误报）并已不再调用；max-fan 状态应由应用自追踪 |
| 4 | §23 thermal | V0/V1 映射 + 0x28 byte3 探测 | 映射确认；但 8BAB 静态 V1，无需运行时探测；**补充决策：ObservedState 的 thermal mode 回读只能走 EC 0x59（只读）或"信任写入+keep-alive"**——建议后者为主、EC 只读诊断为辅 |
| 5 | §24 SDD | 只有 byte 3 | 补充社区已交叉使用的字节：byte 4 bit0 = 软件风扇控制支持（OSH）；byte 5 = 默认 PL4（OmenMon）；byte 7 = MUX 能力位（内核）。其余字节维持"未验证不升级"原则 |
| 6 | §25 0x29 | "最重要协议参考之一" | 补充：内核从不对 8BAB 写显式值；**OSH 与内核结构字节序冲突**（PL1/PL2 顺序相反）；固件 clawback 风险（55W 锁）；验证计划（写入→MSR 回读→RAPL 负载）保留并提升为 Phase 2 的强制门槛 |
| 7 | §27 fan | 未区分转速尺度 | 增加 `FanScale` capability：本机 V1 = krpm（0.1k RPM 单位）；V2 = 百分比（本机不用，但模型要防错发） |
| 8 | §27.1 custom fan curve | "待 WMI manual fan 验证" | **前提已满足**：内核在本板实测 0x2E 可控。保留的条件改为：keep-alive 可靠性 + 温度应急交回固件 |
| 9 | §38 采样表 | 无 keep-alive 概念 | 增加：**0x10 心跳（≤90s 周期）**作为控制会话生命周期的一部分 |
| 10 | §53 参考项目 | OmenSuperHub "HP DLL 调用结果" | 修正：它的控制实际走自己的 `SendOmenBiosWmi`（WMI 为主），OGH DLL 主要用于检测/枚举/灯光；另有 nvpcf（DB unlock）打包行为我们不采用。**许可证补充：OmenCore = MIT（可自由参考）；OmenHwCtl = 无 LICENSE（只参考协议行为）；OmenSuperHub/OmenMon = GPLv3** |
| 11 | §56/§58 Phase 0 | 通用探测清单 | 换成本机定制清单（见 §6） |
| 12 | §41 UI / §45 生命周期 | 未提托盘与渲染风险 | 补充：GPUI 无托盘 → `tray-icon`；pin git commit 策略；无软件渲染回退需早期验证 |

**新增架构概念（建议写入 §33 SafetySupervisor 附近或独立小节）**：
- `KeepAliveService`：控制会话心跳（0x10 周期调用 + 状态重断言），app 正常退出/崩溃时心跳停止 → 固件 120s 内自动收回控制权 → 这本身就是 AR-12 的实现机制。
- 参考 OmenCore 的两层安全网：`QuietSafetyMonitor`（90°C 迟滞 → 风扇拉满）与 `HardwareWatchdogService`（传感器冻结 >90s → 90% 风扇 + 恢复自动）——都是值得借鉴的 fail-closed 模式。

---

## 6. Phase 0 实机探测计划（本机定制版）

在真机上按顺序执行（全部只读，除标注外）：

1. **身份确认**：`Win32_BaseBoard.Product`（预期 `8BAB`）、BIOS 版本、`Win32_ComputerSystem.Model`（预期 "OMEN by HP Gaming Laptop 16-wf0xxx"）。**若 board ≠ 8BAB，停下重估**。
2. **WMI 传输 smoke test**：`hpqBIntM` 存在性、insize=0 探测、0x10 fan count（预期 2）。
3. **SDD dump（0x28，128B）**：byte3/byte4/byte5/byte7 记录存档；MUX 能力位。
4. **风扇表 dump（0x2F）**：min/max RPM、档位数；0x2D 当前转速读回。
5. **GPU 策略读（0x21）**：cTGP/PPAB/dstate/slowdown-temp 当前值。
6. **MUX 读（0x52，cmd 0x01）**：当前模式（预期 Hybrid）。
7. **PawnIO smoke test**：驱动安装 → IntelMSR 模块 → 0x19C/0x1B1/0x1A2/0x606/0x611/0xE7/0xE8 各读一次，与 HWiNFO 交叉比对。
8. **NVAPI smoke test**：`ClientPowerTopologyGetStatus` 空闲/负载各读一次（跑个游戏/烤机），验证瓦数非零且合理；`GetPerfDecreaseInfo` 位覆盖。
9. **EPP 读回**：PowrProf 读当前 AC/DC EPP（只读不验证提权写入）。
10. **输出**：capability snapshot JSON + diagnostic report 存档，作为 BoardProfile 的初版。

**不写阶段**（留到 Phase 2，且每步带 readback）：thermal mode 切换 → 0x27 max fan → 0x2E 手动风扇（低速起步）→ 0x22 GPU 策略 → 0x29（最后，严格三步验证）→ 0x52 MUX（最后，需重启）。

---

## 7. 参考项目可利用度（修正后）

| 项目 | 对本项目的真实价值 | 许可证 | 可利用方式 |
|---|---|---|---|
| Linux hp-wmi | **最高**：8BAB 内核级实机验证、传输细节（insize=0、SECU、返回码）、keep-alive 模式、MUX | GPL-2.0 | 协议行为参考，重实现 |
| OmenSuperHub | 本机开发平台；0x29/0x37 的 payload 参考（0x20009 灯光 payload 对本机无用——无 RGB 硬件）；自定义风扇曲线 1Hz 节奏参考；OGH 伪装思路（不采用——2026-08-25 源码核实：伪装=在 Resources/ 捆绑 HP 专有 HP.Omen.Core.*.dll 并经 HP 官方客户端栈调用，再分发专有二进制 + GPLv3 的组合有法律瑕疵，且对 OGH 版本脆弱；我们用内核证明的 0x10 心跳达到同一固件层效果） | GPLv3 | 协议行为参考，重实现 |
| OmenMon | 完整命令表（含键盘/legacy）；EC 寄存器图（只读诊断用）；issue #37 的本机行为记录 | GPLv3 | 协议行为参考 |
| OmenMon-Reborn | board DB 思路、EC 黑名单（8BAB max-fan-freeze）、只读探测流程 | GPLv3 | 设计模式参考 |
| OmenCore | **MIT，可直接参考代码**；安全网设计（90°C 迟滞、传感器冻结看门狗）；PawnIO MSR 用法；V1/V2 尺度警示 | MIT | 可重度参考 |
| OmenHwCtl | 协议考古老前辈；0x29 单字节写法的第二种实现 | **无 LICENSE** | 只参考协议行为，不复制任何代码 |
| PresentMon | 帧数据基础设施（Phase 5） | MIT | 服务 + 自包 C API |
| PawnIO | MSR 只读遥测 | GPL+例外 | ioctl 使用，无污染 |

---

## 8. 未决问题（只能实机/未来回答）

1. wf0032TX 的 BaseBoard Product 是否确为 8BAB（预期是，Phase 0 第 1 步确认）。
2. 0x29 在本板固件上的真实行为（范围、clamp、字节序、是否被 thermal mode 覆盖、clawback 时序）。
3. 0x2F 风扇表 4 字节输入的语义（内核发全零）。
4. GPU 功率读数在本机 MUX/Optimus 状态下的稳定性（空闲 0W 问题的实际表现）。
5. 本 SKU 的 RTX 4060 TGP 实测值（推断 ~120W）。
6. PresentMon 服务对 Intel CPU 功率/温度的采集路径（若它自己能读 RAPL，可省一部分 PawnIO 依赖——未验证）。
7. BIOS "76.44" 版本串与 F.xx 家族的关系。
8. GPUI 渲染后端在本机（RTX 4060 + 可能的老驱动）上的表现。

---

## 附：主要证据来源

- 内核 hp-wmi.c（master 2026-08-25，3041 行）+ commit 13fa3aaf02（8BAB）、8ca7515d3c、59f586eb93（MUX）、46be1453e6/c203c59fb5（手动风扇+keep-alive）、08ecf6d131（board feature data）
- kernel bugzilla 220639；LKML 16-wf0008la dmidecode
- github.com/OmenMon/OmenMon（Hardware/Bios*.cs、EcData.cs）+ omenmon.github.io + issue #37/#126
- github.com/seakyy/OmenMon-Reborn（wiki/Model-Database.md、Auto-Detection.md、CHANGELOG、OmenMon.xml）
- github.com/GeographicCone/OmenHwCtl（OmenHwCtl.ps1、Reference/）
- github.com/breadeding/OmenSuperHub（OmenHardware.cs、Program.cs、README、issues #86/#8/#42）
- github.com/theantipopau/omencore（HpWmiBios.cs、ModelCapabilityDatabase.cs、PowerLimitController.cs、QuietSafetyMonitor.cs、HardwareWatchdogService.cs；v4.2.0）
- github.com/namazso/PawnIO + PawnIO.Modules/IntelMSR.p；github.com/ylws702/cpu-temp；LibreHardwareMonitor IntelCpu.cs/NvidiaGpu.cs
- github.com/GameTechDev/PresentMon（PresentMonAPI.h、Nvapi/NvmlTelemetryProvider.cpp、README-Service.md）
- NVIDIA 论坛 AD107 功率线程（forums.developer.nvidia.com/t/280270）、nvidia-smi N/A 线程（t/177240）
- zed.dev/blog/zed-for-windows-is-here；github.com/longbridge/gpui-component（examples/system_monitor）；crates.io（wmi、nvapi-sys、nvml-wrapper、tray-icon 等）
- MS Learn：PERFENERGYPREFERENCE、PowrProf API；bitsum power GUID 表
- HP 官方：16-wf0000 系列规格文档（ish_7968187-7968239-16）、OGH 功能文档（ish_3912817-3737596-16）；notebookcheck/TrustedReviews 评测
