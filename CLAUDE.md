# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository status

**M0 (read-only probe), M1 (telemetry engine), and M2 (control foundation) are complete and verified on the reference machine.** Workspace: `phelper-domain` (pure domain model + ports) → `phelper-core` (engine + platform adapters) → `phelper-cli` (dev/verification harness). UI is deferred; core-first (telemetry + control + policy).

M1 delivered: `Engine` (telemetry-only assembly, provider-tolerant startup), `TelemetryCoordinator` thread with per-cadence collectors (PawnIO 250 ms / NVAPI 500 ms / PDH 1 s / HP fans 1 s hard rule / power 5 s), bounded ring-buffer store (8192/metric), `TelemetryHandle` {snapshot/history/stats/subscribe/request_fresh}, and the HpActor thread owning all WMI traffic (closed typed-request enum — no raw-payload channel, §50). The coordinator thread is **pinned to logical processor 0** (APERF/MPERF are per-core MSRs; same-core reads make the effective-clock ratio honest — measured starvation without pinning). Verified on-device: all metrics live over a 10-min load-phased run, jitter ≤35 ms normal, graceful shutdown. **GPU power = NVML** (`nvmlDeviceGetPowerUsage` — on-device evidence overturned the R5 "NVML dead on AD107" finding; ClientPowerTopology reports 0 entries on this machine and is now the declared fallback).

M2 delivered: **`ControlCoordinator`** (single-writer FIFO thread, sync_channel(32)→Busy; §32 multi-step CPU policy; AR-10 verification: PPM readback=Verified / fan 0x2D 8×1s ±1000 RPM / thermal+max-fan=TrustedNoReadback), **`SafetySupervisor`** (write-time validation incl. fresh-temp ≤5 s gate for manual fan; ≥90 °C→ForceMaxFan / ≤85 °C→ReleaseTo hysteresis while fan-held; 90 s sensor-freeze watchdog → restore auto), **`KeepAliveService`** (60 s 0x10 heartbeat + non-default TrustedWrite re-assertion; 2 consecutive failures → fail closed), **`ControlJournal`** (JSONL, origin user/keepalive/safety/shutdown, before/after evidence per step), capability probe THROUGH the running HpActor (`probe_runtime` — never a second WMI connection), OGH second-writer startup scan (warn-only), readback metrics (`cpu.epp_ac/dc`, `cpu.pl1_w/pl2_w/power_limit_raw` via MSR 0x610, `gpu.power_limit_w` via NVML **GetEnforcedPowerLimit** — the Management variant is NOT_SUPPORTED here), and `phelper-cli control` subcommands with §56 before/command/after output. First writes: EPP / max-freq / boost (PowrProf, never restored), thermal 0x1A, fan 0x2E manual + 0x27 max (restored on graceful shutdown). **16-step HIL passed on-device** incl. manual fan held past the 120 s clawback window (heartbeat proof), taskkill /F → firmware clawback at ~150 s (AR-12 proven), hysteresis force/release, and the negative ladder (clamp / non-multiple / no-temp-feed / unelevated). On-device corrections M2 surfaced: **0x64F is not in the signed IntelMSR allow-list** (metric removed); the hysteresis release must write 0x27-off **before** re-applying manual levels or the keepalive re-asserts a stale max_fan (HIL-13 catch); max→manual fan ramp-down is ~6–9 s; 8BAB does idle fan-stop (auto = 0 RPM when cold). Deferred to M3+: 0x29 power limits, 0x22 GPU policy, MUX write, profiles, fan curves, PERFEPP1.

**`architecture.md` is the source of truth** (post-M0 corrections applied; where it conflicts with older external docs, `docs/feasibility-16-wf0032TX.md` wins). Before writing code, read it — especially §4 (AR-01–AR-12), §21 (MOF facts), §33.1 (KeepAliveService), and §60 (review checklist). New features must be checked against the §60 checklist; if its 15 questions can't be answered, the feature does not go in.

## What this project is

An HP OMEN laptop performance-control and hardware-telemetry desktop app: **Rust + GPUI (gpui-component), Windows 11**, single-process modular monolith. The one and only target: **OMEN Gaming Laptop 16-wf0032TX (SKU 81L09PA, board 8BAB)** — i9-13900HX + RTX 4060 Laptop (AD107). It deliberately reimplements only the performance/telemetry core of OMEN Gaming Hub — no store, RGB editor, cloud, or account features (§3 Non-Goals).

## Hard architectural invariants (§4)

These are non-negotiable; code that violates them is wrong:

- **UI never touches hardware.** GPUI dispatches `ControlCommand`s and subscribes to AppState only — no WMI/PowrProf/NVAPI/PawnIO/MSR/EC calls from UI code.
- **Read path and write path are fully separate.** Telemetry answers "what is happening"; control answers "what we want". Separate data structures, lifecycles, error models, thread models.
- **All writes go through `ControlCoordinator`** (single-writer, serialized FIFO queue). No shortcut hardware-write path from any subsystem, tray, hotkey, CLI, or profile. Slider-style command coalescing lives in the Application layer.
- **Capabilities are discovered, never assumed** (AR-05), and **unknown means unsupported** (AR-06). No feature inference from "OMEN + Intel + NVIDIA".
- **No direct EC writes in supported control paths** (AR-07). EC is at most optional, read-only, board-verified diagnostics behind an `experimental-ec` Cargo feature.
- **Windows CPU policy via PowrProf/PPM APIs only** (AR-08) — never `powercfg.exe` as a backend (dev-verification/diagnostics only), never dual-write HWP policy via MSR.
- **PawnIO is read-only MSR telemetry infrastructure** (AR-09): allow-listed Intel MSRs (thermals, RAPL energy, APERF/MPERF). Never expose generic `write_msr()`/`write_io_port()`/`write_ec()`.
- **Three states stay distinct** (AR-10): `DesiredState` (user intent), `ObservedState` (verified readback), `TelemetryState` (live sensor data). "API returned success" ≠ "hardware is in desired state" — every write needs readback/verification.
- **Fail closed** (AR-11): when safety/support/verification is uncertain, don't write.
- **Firmware safe state wins** (AR-12): on crash/timeout/exit, restore firmware automatic fan control. The app must never be the sole thing keeping the machine cool.

## Structure and dependency direction

Actual workspace (the §6 crate split is realized as modules inside `phelper-core`; splitting further is a mechanical later step):

```
crates/phelper-domain  → pure domain model + ports/traits; deps only serde/thiserror
                         (compiler-enforced AR-01: NO Win32/WMI/GPUI/NVAPI)
crates/phelper-core    → engine + platform adapters behind domain ports:
                         platform/{hp_wmi, pawnio, nvidia, windows_ppm, identity, elevation},
                         capability (probe + BoardProfile boards/8bab.toml),
                         telemetry (M1), control (M2), persistence, smoke (dev harness)
crates/phelper-cli     → probe/telemetry dev harness (clap); asInvoker manifest
apps/desktop           → future GPUI shell (only depends on core)
```

`phelper-domain` must not depend on UI or platform crates; platform modules implement its ports (hexagonal style). `BoardProfile` (developer-maintained: what this machine is/what's verified) and `PerformanceProfile` (user-facing: desired behavior) are permanently separate concepts (§36).

## Domain essentials

- **Metric ownership** (§12): every metric has exactly one authoritative source + declared fallbacks — e.g. FPS/frame data = PresentMon; CPU package temp/power = PawnIO MSR/RAPL; GPU power = **NVML** (fallback ClientPowerTopology, verified 0-entries on this machine); other GPU metrics = NVAPI; RAM/disk/network = Windows; fan/thermal mode = HP WMI. Two collectors never both own a metric.
- **HP WMI control surface** (§21–27): `root\wmi` class `hpqBIntM`, methods `hpqBIOSInt{0,4,128,1024,4096}(InData, OutData)->Boolean`, gaming command group `0x20008`, behind a `BiosInvoker` seam (pub(crate) `raw_execute`; no raw payload escapes the module). Thermal mode on 8BAB is **static V1** ({Balanced=0x30, Performance=0x31}; Cool unconfirmed → not in the writable enum). `0x29` power limits: byte-order conflict unresolved → Experimental gate + mandatory three-step verification.
- **EPP is a first-class control** (§18): CPU responsiveness (0–100, AC/DC split) via Windows `PERFEPP`, preferred over PL1 for light-load behavior.
- **Structured `ControlError`** (§34): UI shows human messages (`Unsupported`, `UnsafeRequest`, `VerificationFailed`, …), never raw HRESULTs.
- **Source reliability tiers** (§54): Tier A (cross-validated incl. Linux `hp-wmi`) → stable; Tier B → board-gated; Tier C → diagnostics/experimental only; **Tier D (unknown) → write prohibited**.
- **Safe rollout for any new hardware write** (§57): read-only probe → dev-only command → reference-machine verification → capability-gated experimental UI → stable. Never "found a register on Monday, in stable UI on Tuesday".
- **Sampling** (§38): per-domain cadences; HP WMI/firmware is *never* high-frequency polled (~1 s slow sensors).

## Licensing constraint (§55)

Key protocol references (OmenSuperHub, Linux `hp-wmi`, PawnIO) are **GPL**. Re-implement adapters from protocol behavior and public documentation — do not copy GPL code into this project. PresentMon and NVAPI SDK are MIT and safe to use directly.

## Implementation order (§58)

Phase 0 is a **CLI hardware probe** (DeviceIdentity, Board ID, BIOS, ThermalPolicyVersion, SystemDesignData, WMI availability, NVIDIA, PawnIO, Windows PPM) to build the reference-platform capability snapshot — before any GPUI work. Then: telemetry foundation → control foundation (CLI/integration-tested) → GPUI shell → profiles → PresentMon gaming telemetry → advanced thermal.

## Build / test commands

Standard Cargo workspace:

- `cargo build --workspace` / `cargo test --workspace` / `cargo clippy --workspace` (all clean; 55 tests, also under `--all-features`)
- `cargo run -p phelper-cli -- probe [--json PATH] [--record-fixtures DIR] [--emit-board-profile PATH]` — read-only capability probe. **Must run elevated** (`root\wmi` ACL is admin-only → 0x80041003 otherwise); unelevated it degrades to identity-only.
- `cargo run -p phelper-cli -- telemetry [--interval-ms N] [--duration S] [--metrics SUBSTR]...` — live metric table (repeat `--metrics` per substring). Ctrl+C now shuts the engine down GRACEFULLY (the engine includes the control coordinator since M2 — an ungraceful kill leaves fan/thermal state to the ~120 s firmware clawback).
- `cargo run -p phelper-cli -- control <SUB>` — M2 control plane (hard-enables `phelper-core/control`). `status` (caps + observed + OGH findings + journal tail), `epp --ac N --dc N`, `max-freq --ac MHZ --dc MHZ` (0 = unlimited), `boost <mode>`, `thermal <balanced|performance> [--hold 120]`, `fan auto`, `fan max --on|--off [--hold 120]`, `fan manual --cpu RPM --gpu RPM [--hold 120]` (RPM must be multiples of 100 — rejected client-side before engine start). Every mutating command prints §56 BEFORE/COMMAND/AFTER. HP-state commands hold the process so the 60 s heartbeat keeps the firmware from clawing state back; Ctrl+C/Break or hold expiry → graceful restore (AR-12). `--hold 0` = fire-and-exit WITHOUT restore (clawback is the net). PPM settings are Windows-native: they persist and are never auto-restored — restore manually (the reference values on this machine: epp 0/0, max-freq 0/0, boost aggressive).
- `cargo run -p phelper-cli -- gpu-load [--seconds N] [--mem-mib N]` — DEV-ONLY verification load generator (CUDA Driver API via nvcuda.dll, no toolkit needed). Wakes the dGPU so gpu.power_w / clocks can be acceptance-tested. Never touches write paths.
- `cargo run -p phelper-cli -- hp-spike [--cpu RPM] [--gpu RPM]` — DEV-ONLY S2 write-transport spike (0x1A round-trip + 0x2E manual fan + 0x2D readback, always restores auto). Target must sit clearly away from the auto baseline.
- The CLI manifest is `asInvoker` deliberately: elevation is the operator's choice (run from an admin terminal), the engine detects the token and degrades (unelevated: pawnio + hp-wmi unavailable, telemetry continues, writes → `PermissionDenied`). `highestAvailable` breaks `cargo test` from non-elevated shells (os error 740/741).
- `assets/pawnio/IntelMSR.bin` + `COPYING` — signed PawnIO module (LGPL runtime data, from namazso/PawnIO.Modules releases); override path via `PHELPER_INTELMSR`. Note: **MSR 0x64F (limit reasons) is NOT in the module's allow-list** (0x80070005 on-device) — that metric was removed; 0x610 PL1/PL2 works.
- `docs/hil*.ps1` — the M2 HIL scripts (fan hold / clawback / hysteresis / watchdog gate), kept as re-runnable verification assets.

Hardware-in-the-loop and verification tests (before/command/after, e.g. WMI `0x29` write + MSR readback + RAPL workload response) are required for control features and must run on the physical reference machine (§56).
