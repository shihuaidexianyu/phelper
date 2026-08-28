# Windows 应用级调度

> 状态：已接入 core、CLI 和桌面端“应用”页
> 日期：2026-08-28

## 这层解决什么问题

phelper 现在分成两条控制链：硬件链负责 HP Thermal、风扇、GPU 平台策略和功耗墙；OS 链负责某个进程或线程在 Windows 调度器中的约束。两条链互不抢写，也不把“高性能模式”当成万能开关。

OS 链目前支持：

| 参数 | 目标 | 作用 |
| --- | --- | --- |
| CPU Sets | 进程 / 线程 | 全部、P 核、E 核或指定 CPU Set ID |
| Affinity | 进程 / 线程 | 一个处理器组内的硬亲和性位掩码 |
| QoS | 进程 / 线程 | 系统管理、高执行速度、EcoQoS |
| 优先级 | 进程 / 线程 | 安全子集，不暴露 Realtime |
| 内存优先级 | 进程 / 线程 | 影响内存回收倾向，不等于 CPU 优先级 |
| 理想处理器 | 线程 | 调度提示，不是硬绑定 |
| GPU 首选项 | 进程 / 可执行文件 | Windows Graphics Settings 的下次启动首选项 |

P/E 核不是通过“第几个逻辑处理器”猜测，而是读取 `GetSystemCpuSetInformation` 的 `EfficiencyClass`，再转换成 Windows CPU Set ID。较高的 efficiency class 作为性能组，较低的作为能效组；非混合架构机器上两组可能相同，这是正常的。

## 生命周期和恢复

`OsPolicyHandle` 创建时不扫描 CPU 拓扑，也不枚举进程，所以不会拖慢首屏。第一次打开“应用”页或执行 `os topology` 时才读取拓扑。

每个目标第一次应用策略前，core 捕获原始 CPU Sets、Affinity、QoS、优先级、内存优先级、理想处理器和 GPU 注册表值。重复调整同一个目标不会覆盖这份基线。应用过程中任何一步失败都会回滚到本次操作前的状态；桌面端退出时恢复所有仍由 phelper 接管的目标。

恢复前会重新读取可执行文件路径。PID/TID 被复用、路径无法确认或目标类型不匹配时，core 宁可不恢复，也不会把旧策略写到新进程。

Realtime 进程优先级和 Time Critical 线程优先级不开放；这类设置可能饿死输入、存储或安全线程，不适合作为普通性能工具的选项。

## 配置档语义

配置档可以携带 `os_policy`，但它只描述 OS 策略，不隐含目标进程：

```toml
description = "高性能应用"

[os_policy]
cpu_placement = "performance"
qos = "high"
process_priority = "above_normal"
memory_priority = "normal"
gpu_preference = "high_performance"
```

`control profile apply` 不会猜 PID，因此遇到带 `os_policy` 的配置档会明确拒绝，并提示使用 `os apply --profile <name> --pid <PID>`。这样配置档不会在用户没有指定目标时误改任意程序。

## CLI

只读查看：

```powershell
cargo run -p phelper-cli -- os topology
cargo run -p phelper-cli -- os processes
```

对现有进程应用 P 核、高 QoS、较高进程优先级，并在 120 秒后恢复：

```powershell
cargo run -p phelper-cli -- os apply --pid 1234 --cpu performance --qos high --process-priority above-normal --hold 120
```

线程级控制：

```powershell
cargo run -p phelper-cli -- os apply --tid 5678 --cpu performance --thread-priority highest --ideal-group 0 --ideal-number 4 --hold 120
```

显式 CPU Set 和 Affinity：

```powershell
cargo run -p phelper-cli -- os apply --pid 1234 --cpu-set 0,2,4,6 --hold 120
cargo run -p phelper-cli -- os apply --pid 1234 --affinity-group 0 --affinity-mask 0x55 --hold 120
```

`--hold 0` 表示一直保持到 Ctrl+C；CLI 自己退出前会调用恢复。桌面端则由 Engine 的 shutdown 路径统一恢复。

## 电源感知自动调度（第一版）

自动调度不替代上面的逐目标手动控制，也不切换 Windows 活动电源方案。当前只有
`Off` 和 `BatteryEfficiency`：用户明确选择后，core 在确认电池供电时每 2 秒扫描一次
当前用户会话，排除 Windows 系统路径、桌面/音频/安全关键进程和无法读取身份的目标，
对剩余进程写入：

```text
default CPU Sets = E-core CPU Set IDs
QoS = EcoQoS
```

这是软调度提示，不是 Affinity 硬锁；不自动改优先级、内存优先级、GPU 首选项或硬件
风扇/功耗墙。交流供电、供电未知、模式关闭和正常退出都会释放自动 owner 并恢复
基线。自动和手动共享同一份 ledger，手动目标优先，PID 复用会检查路径和创建时间。

自动模式默认关闭且当前不持久化。CLI 验证入口：

```powershell
cargo run -p phelper-cli -- os auto status
cargo run -p phelper-cli -- os auto battery --hold 120
```

自动 worker 优先注册 PowrProf 电源事件，但事件只触发完整上下文重读；注册不可用时
回退到低频轮询。进程发现目前仍是轮询，ETW 和前台/后台分类留到后续阶段。

## GPU 首选项的边界

Windows 官方的 DXGI GPU preference API 是给应用自己在创建 D3D 设备时选择适配器用的，不能把一个已经运行在某个 GPU 上的任意进程迁移到另一张 GPU。phelper 对进程路径写入 `HKCU\Software\Microsoft\DirectX\UserGpuPreferences` 的 `GpuPreference=1;` 或 `GpuPreference=2;`，这是 Windows 图形设置使用的 per-executable 约定，效果需要目标进程下一次启动才有机会体现。

因此 UI 会标明“下次启动”，不会把它伪装成即时 MUX 切换，也不会替代硬件层的显卡模式。注册表原值会进入 phelper 的会话基线，退出时恢复。

## 官方接口依据

- [CPU Sets](https://learn.microsoft.com/en-us/windows/win32/procthread/cpu-sets)
- [GetSystemCpuSetInformation / SYSTEM_CPU_SET_INFORMATION](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-system_cpu_set_information)
- [SetProcessAffinityMask](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setprocessaffinitymask)
- [SetProcessInformation](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setprocessinformation)
- [Quality of Service](https://learn.microsoft.com/en-us/windows/win32/procthread/quality-of-service)
- [DXGI_GPU_PREFERENCE](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_6/ne-dxgi_gpu_preference)
- [IDXGIFactory6::EnumAdapterByGpuPreference](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_6/nf-dxgi1_6-idxgifactory6-enumadapterbygpupreference)
