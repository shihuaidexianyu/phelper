# Windows 软件电源策略设计

> 状态：已实现第一版（core + CLI + 性能页）
> 日期：2026-08-28
> 适用：Windows 11，首个验证平台为 OMEN 16-wf0032TX / board 8BAB

## 结论

phelper 不把 Windows 的“均衡 / 高性能 / 最佳性能”当成唯一控制入口。Windows
实际同时存在三层状态：

```text
硬件层：HP Thermal / 风扇 / GPU 平台策略 / 0x29 功耗墙
                         │
软件层：当前活动电源计划（PowrProf）
        ├─ AC/DC 的 PPM 参数索引：phelper 可以逐项读写
        ├─ Windows 设置中的高层模式：只读观察
        └─ 实际生效模式：只读观察，可能受到电池节能、游戏模式等影响
```

活动电源计划是 phelper 的软件写入目标；高层模式和实际生效模式只是上下文。
它们可能不一致，因此 phelper 不会为了让几个标签看起来一致而偷偷切换 Windows
模式或替用户改活动计划。

## 参数模型

| 参数 | Windows 设置 | 实际含义 | phelper 关系 | UI 位置 |
| --- | --- | --- | --- | --- |
| P 核 EPP | `PERFEPP` | 性能偏好，0 偏性能、100 偏省电；不是硬频率锁 | 已有 AC/DC 读写、回读和快速合并 | 性能页主区域 |
| E 核 EPP | `PERFEPP1` | E-core 的同类性能偏好 | 已有 AC/DC 读写、回读 | 性能页主区域 |
| 最大频率 | `PROCFREQMAX` | MHz 频率上限，0 表示不限 | 已有 AC/DC 读写、回读 | 性能页主区域 |
| 最低性能 | `PROCTHROTTLEMIN` | PPM 性能下限；设高会减少降频/空闲空间 | 已加入 AC/DC 独立读写、回读和顺序安全检查 | 性能页“更多参数” |
| 最高性能 | `PROCTHROTTLEMAX` | PPM 性能上限；不能低于最低性能 | 已加入 AC/DC 独立读写、回读和顺序安全检查 | 性能页“更多参数” |
| Boost | `PERFBOOSTMODE` | 睿频策略，不同值对 turbo 的允许方式不同 | 已加入 AC/DC 独立读写、回读；profile/CLI 可用 | profile / CLI 高级入口 |
| PL1 / PL2 / PL4 | HP `0x29` | 硬件功耗保护包络 | 独立于 Windows PPM，仍是 experimental 双门禁 | 高级实验抽屉 |
| Thermal / 风扇 | HP WMI | 平台散热和风扇执行层 | 由既有 HP 单写者、心跳和退出恢复负责 | 性能页散热区域 |

EPP、最低/最高性能和频率上限不要混为同一个“性能百分比”：EPP 是偏好，
最低/最高性能是范围，频率上限是 MHz。PL1/PL2/PL4 也不是同一层，它们是
硬件功耗限制。这样才能解释为什么调了一个 Windows 模式后，功耗、频率或风扇
不一定按同样的比例变化。

## Core 实现

### Domain 和 port

`phelper-domain` 增加了：

- `WindowsPpmValues`：一条 AC 或 DC 电源轨的细粒度参数；
- `WindowsPpmState`：活动计划、Windows 高层模式、实际生效模式和 AC/DC 参数；
- `CpuPolicy` 的 AC/DC 性能上下限和 AC/DC Boost 字段；
- `CpuPolicyBackend` 的完整读写接口，所有字段仍然是 `Option`，未指定即不动。

旧的 `boost_policy` 保留为“AC/DC 同值”的兼容简写；当 profile 或命令同时给出
rail-specific 字段时，AC/DC 字段优先。

### PowrProf backend

正式 backend 只使用 `PowrProf.dll`：

1. 读活动计划 GUID；
2. 用 `PowerReadAC/DCValueIndex` 读对应的 PPM setting；
3. 写入时只调用 typed 的 `PowerWriteAC/DCValueIndex`；
4. 写过的同一个活动计划 GUID 再经 `PowerSetActiveScheme` 提交；
5. 立即读取同一 setting 做 readback，失败就如实返回 `Failed`。

`powercfg.exe` 只用于开发核对和诊断，不进入 core 写路径。一个命令的 AC/DC
未指定侧不会被填成 0，也不会因为切换 profile 而向硬件写入 `0,0`。

### Windows 高层模式

Windows 11 的用户选择通过 `PowerGetUserConfiguredACPowerMode` /
`PowerGetUserConfiguredDCPowerMode` 读取；实际生效模式通过
`PowerRegisterForEffectivePowerModeNotifications` 接收。两者都只进入状态模型，
当前版本不提供写入口，原因是它们是 Windows 的高层策略投票/覆盖结果，不等价于
活动计划里的某一个 PPM index。这样可以观察冲突，但不会和 Windows 设置形成两个
互相抢写的控制器。

### Capability 和安全

能力探测以完整 snapshot 为单位读取 PPM。每个参数都必须在 AC、DC 两侧读到并
通过范围解析，才能标记为 `Supported`；读不到或出现未知值就是 `Unsupported`。
写入仍需要管理员权限。

性能上下限写入前还会检查：

- 每个值在 `0..=100`；
- 同一电源轨的最低性能不高于最高性能；
- 如果只改一侧，另一侧的当前已验证值会参与比较；
- 写后只验证用户实际请求的 AC/DC 侧，未请求侧不被误判为失败。

所有写操作仍只经过 `ControlCoordinator`。启动探测得到的 Windows snapshot 会
直接交给 coordinator 作为初始读回，避免启动时重复扫描 PowrProf；退出时只恢复
本会话实际接管过的 HP 风扇、Thermal、GPU 或实验功耗墙。Windows PPM 设置是系统
策略，不带 phelper 会话恢复语义，因此不会在退出时自动恢复。

## UI 取舍

性能页只把常用的 EPP、E-core EPP 和频率上限放在首屏。最低/最高性能放进“更多
参数”，按 AC/DC 两列展示，并同时显示目标与当前读回。页面顶部只保留一行有决策
价值的上下文：活动计划、高层配置模式和实际生效模式。

页面不展示“引擎运行中”“已应用”等不会帮助用户做下一步决策的状态文案，也不把
温度和功率塞进同一坐标轴。骨架先渲染布局，PPM snapshot 到达后再填充 slider；
读不到的参数保持禁用/空值，不用假数据占位。

Boost 的七种 Windows 枚举目前主要服务 profile 和 CLI，因为把 AC/DC 两个下拉框
常驻首屏会明显增加密度；如果实际使用中需要频繁调 Boost，再把它放进同一个“更多
参数”区域，而不是重新增加一个独立页面。

## CLI 验证

```powershell
cargo run -p phelper-cli -- control status
cargo run -p phelper-cli -- control min-perf --ac 20 --dc 5
cargo run -p phelper-cli -- control max-perf --ac 100 --dc 80
cargo run -p phelper-cli -- control boost --ac aggressive --dc efficient-enabled
```

`control status` 会分别打印：活动计划、Windows 配置模式、实际生效模式，以及
AC/DC 的 EPP、EPP1、Boost、频率上限和性能上下限。修改前后可以用 Windows 自带
`powercfg /QH` 做只读交叉核对。

## 官方接口依据

- [PowerGetActiveScheme](https://learn.microsoft.com/en-us/windows/win32/api/powersetting/nf-powersetting-powergetactivescheme)
- [PowerSetActiveScheme](https://learn.microsoft.com/en-us/windows/win32/api/powersetting/nf-powersetting-powersetactivescheme)
- [Power policy settings](https://learn.microsoft.com/en-us/windows/win32/power/power-policy-settings)
- [Processor power-management options](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/configure-processor-power-management-options)
- [PERFENERGYPREFERENCE / EPP](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/options-for-perf-state-engine-perfenergypreference)
- [MinPerformance / PROCTHROTTLEMIN](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/options-for-perf-state-engine-minperformance)
- [MaxPerformance / PROCTHROTTLEMAX](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/options-for-perf-state-engine-maxperformance)
- [Effective power-mode notifications](https://learn.microsoft.com/en-us/windows/win32/api/powersetting/nf-powersetting-powerregisterforeffectivepowermodenotifications)
- [PowerGetUserConfiguredACPowerMode](https://learn.microsoft.com/en-us/windows/win32/api/powrprof/nf-powrprof-powergetuserconfiguredacpowermode)
- [PowerGetUserConfiguredDCPowerMode](https://learn.microsoft.com/en-us/windows/win32/api/powrprof/nf-powrprof-powergetuserconfigureddcpowermode)
