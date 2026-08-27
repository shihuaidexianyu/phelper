# PresentMon 帧遥测（Phase 5 foundation）

当前仓库已经接入一个只读的 PresentMon provider，但还没有自动选择游戏进程的 UI。
为了避免后台误跟踪任意进程，必须在启动 phelper 前显式指定目标 PID：

```powershell
$env:PHELPER_PRESENTMON_PID = "12345"
cargo run -p phelper-cli -- telemetry --duration 30
```

如果 `PresentMonAPI2.dll` 不在标准安装位置，可以额外指定 DLL 路径：

```powershell
$env:PHELPER_PRESENTMON_DLL = "C:\path\to\PresentMonAPI2.dll"
```

适配器通过动态加载 PresentMonAPI2.dll 建立 session，启动该 PID 的 tracking，注册
frame query，并将每轮记录转换为 canonical telemetry。它不会写入硬件，也不会自动
扫描或附加进程。没有 PID、DLL、PresentMon service 或目标进程时，provider 只会在
Diagnostics 中显示 Unavailable/Degraded，其他遥测和控制路径照常运行。

当前注册的指标：

| 指标 | 单位 | 说明 |
| --- | --- | --- |
| `frame.displayed_fps` | FPS | 最新 collection batch 的显示帧速率 |
| `frame.1p_low_fps` | FPS | 1 秒滚动窗口中 displayed frame time 的 p99 倒数；样本标为 Estimated |
| `frame.time_ms` | ms | 最新 batch 的平均 displayed frame time |
| `frame.cpu_busy_ms` | ms | 最新 batch 的平均 CPU busy |
| `frame.gpu_time_ms` | ms | 最新 batch 的平均 GPU time |
| `frame.display_latency_ms` | ms | 最新 batch 的平均 display latency |

查询会从完整字段逐级降级到只保留 `frame.time_ms`，因此 PresentMon 配置未启用
某个可选字段时，基础 FPS/帧时间仍可用。Monitor 和 Diagnostics 从统一 registry
自动展示这些指标，Dashboard 目前展示 FPS、1% Low、帧时间和显示延迟四张卡片。

## 当前边界

- 仍需人工提供 PID；尚未实现进程选择器、游戏启动/退出跟踪和多进程聚合。
- 1% Low 使用 collection-time 标记的 1 秒窗口，是可用的初版估计，不替代后续基于
  PresentMon 原始时间戳的 benchmark 分析。
- 尚未完成真实游戏 HIL、PresentMon service 安装向导和 benchmark 导出格式。
