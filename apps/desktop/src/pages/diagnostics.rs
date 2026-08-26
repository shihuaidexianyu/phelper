//! Diagnostics (plan D-G): identity card, capability matrix, provider
//! health (+ scheduler jitter), the §12 metric ownership map with
//! fallback highlighting, OGH second-writer findings, the live journal
//! tail, and the one-click diagnostic report export. This page is the
//! debugging instrument for every later write-page task (built first by
//! design, plan D-G note).

use gpui::{App, Context, Div, IntoElement, ParentElement, Styled, Window, div, prelude::FluentBuilder, px};
use gpui_component::{ActiveTheme, StyledExt, button::Button};
use phelper_core::app::fmt;
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::{AppState, EngineStatus};
use phelper_core::telemetry::registry;
use phelper_domain::capability::Support;
use phelper_domain::telemetry::MetricQuality;

use crate::shell::{DiagState, ShellView};
use crate::widgets::journal_view;

use super::dashboard::page_root;

fn section(cx: &App, title: &str, body: Div) -> Div {
    let theme = cx.theme();
    div()
        .v_flex()
        .gap_2()
        .w_full()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child(title.to_string()))
        .child(body)
}

fn row_kv(cx: &App, k: &'static str, v: String) -> Div {
    let theme = cx.theme();
    div()
        .h_flex()
        .gap_2()
        .child(
            div()
                .w(px(160.))
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(k),
        )
        .child(div().text_sm().child(v))
}

fn support_badge(cx: &App, s: Support) -> Div {
    let theme = cx.theme();
    let color = match s {
        Support::Supported => theme.success,
        Support::Experimental => theme.warning,
        Support::Unsupported => theme.danger,
        Support::NotProbed => theme.muted_foreground,
    };
    div()
        .w(px(64.))
        .text_sm()
        .text_color(color)
        .child(fmt::support_zh(s))
}

pub fn render(
    state: &AppState,
    app: &AppHandle,
    diag: &DiagState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();

    // ---- header: export button + note ----
    let app_c = app.clone();
    let header = div()
        .h_flex()
        .gap_3()
        .w_full()
        .child(
            Button::new("export-diag")
                .outline()
                .label("导出诊断报告 (JSON)")
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                    let st = app_c.state();
                    let jitter = app_c.scheduler_jitter();
                    this.diag.export_note = Some(match phelper_core::app::report::write_report(&st, &jitter) {
                        Ok(p) => (format!("已导出：{}", p.display()), true),
                        Err(e) => (format!("导出失败：{e}"), false),
                    });
                    cx.notify();
                })),
        )
        .when_some(diag.export_note.as_ref(), |this, (msg, ok)| {
            this.child(
                div()
                    .text_sm()
                    .text_color(if *ok { theme.success } else { theme.danger })
                    .child(msg.clone()),
            )
        });

    // ---- identity card ----
    let identity = {
        let mut b = div().v_flex().gap_1();
        match &state.identity {
            Some(id) => {
                b = b
                    .child(row_kv(cx, "产品", format!("{} {}", id.manufacturer, id.product_name)))
                    .child(row_kv(cx, "主板 ID / BIOS", format!("{} · {}", id.board_id, id.bios_version)))
                    .child(row_kv(cx, "CPU", id.cpu.name.clone()))
                    .child(row_kv(
                        cx,
                        "GPU",
                        id.gpu.iter().map(|g| g.name.clone()).collect::<Vec<_>>().join(" + "),
                    ));
            }
            None => {
                b = b.child(div().text_sm().text_color(theme.muted_foreground).child("等待引擎…"));
            }
        }
        section(cx, "设备标识", b)
    };

    // ---- engine / elevation line ----
    let engine_line = {
        let label = match &state.engine {
            EngineStatus::Starting => "启动中…".to_string(),
            EngineStatus::Running => "运行中（控制可用）".to_string(),
            EngineStatus::TelemetryOnly => "遥测模式（控制不可用——可能未提权）".to_string(),
            EngineStatus::Failed(e) => format!("故障：{e}"),
        };
        let privileged = state
            .caps
            .as_ref()
            .map(|c| c.ppm.write_privileged)
            .unwrap_or(false);
        div().v_flex().gap_1().child(row_kv(cx, "引擎", label)).child(row_kv(
            cx,
            "提权态",
            if privileged { "已提权（PPM 写入可用）".into() } else { "未提权/未知".into() },
        ))
    };

    // ---- OGH findings ----
    let ogh = {
        let b = if state.ogh_findings.is_empty() {
            div().h_flex().child(
                div()
                    .text_sm()
                    .text_color(theme.success)
                    .child("未发现第二写入者（OMEN Gaming Hub 未在运行）"),
            )
        } else {
            let mut v = div().v_flex().gap_1();
            for f in &state.ogh_findings {
                // Only a RUNNING second writer is actionable; an installed
                // package or a stopped service is the normal state of a
                // machine that merely has OGH on disk — mute it.
                let color = match f.kind {
                    phelper_core::OghFindingKind::RunningWriter => theme.warning,
                    _ => theme.muted_foreground,
                };
                v = v.child(div().text_sm().text_color(color).child(f.to_string()));
            }
            v
        };
        section(cx, "第二写入者扫描（OGH）", b)
    };

    // ---- capability matrix ----
    let caps_card = {
        let mut b = div().v_flex().gap_1();
        match &state.caps {
            Some(c) => {
                let rows: [(&'static str, Support); 7] = [
                    ("散热模式 (0x1A)", c.thermal_mode),
                    ("风扇转速读取 (0x2D)", c.fan_rpm_read),
                    ("手动风扇 (0x2E)", c.fan_manual_level),
                    ("最大风扇 (0x27)", c.max_fan),
                    ("GPU 平台策略 (0x21/0x22)", c.gpu_platform_policy),
                    ("MUX 显示模式", c.mux),
                    ("功耗墙 (0x29)", c.power_limits),
                ];
                for (label, s) in rows {
                    b = b.child(div().h_flex().gap_2().child(support_badge(cx, s)).child(
                        div().text_sm().child(label),
                    ));
                }
                let fan_range = match (c.fan.clamp_min, c.fan.clamp_max) {
                    (Some(lo), Some(hi)) => {
                        format!("{} 个 · 手动 {}–{} RPM", c.fan.count, lo * 100, hi * 100)
                    }
                    _ => format!("{} 个 · 手动范围未探测", c.fan.count),
                };
                b = b
                    .child(div().h_flex().gap_2().child(support_badge(cx, c.ppm.epp)).child(
                        div().text_sm().child("EPP（P 核）"),
                    ))
                    .child(div().h_flex().gap_2().child(support_badge(cx, c.ppm.epp1)).child(
                        div().text_sm().child("EPP1（E 核）"),
                    ))
                    .child(div().h_flex().gap_2().child(support_badge(cx, c.ppm.max_freq)).child(
                        div().text_sm().child("频率上限"),
                    ))
                    .child(row_kv(cx, "风扇硬件范围", fan_range));
                if !c.notes.is_empty() {
                    for n in &c.notes {
                        b = b.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("· {n}")),
                        );
                    }
                }
            }
            None => {
                b = b.child(div().text_sm().text_color(theme.muted_foreground).child("能力位未就绪"));
            }
        }
        section(cx, "能力位（AR-05 实机探测）", b)
    };

    // ---- providers + jitter ----
    let providers_card = {
        let jitter = app.scheduler_jitter();
        let mut b = div().v_flex().gap_1();
        match &state.telemetry {
            Some(snap) if !snap.providers.is_empty() => {
                for (name, status) in &snap.providers {
                    let ok = matches!(status, phelper_domain::telemetry::ProviderStatus::Ok);
                    b = b.child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(140.))
                                    .text_sm()
                                    .child(name.to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .text_color(if ok { theme.success } else { theme.warning })
                                    .child(fmt::provider_status_zh(status)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(
                                        jitter
                                            .get(name)
                                            .map(|d| format!("最大抖动 {} ms", d.as_millis()))
                                            .unwrap_or_else(|| "—".into()),
                                    ),
                            ),
                    );
                }
            }
            _ => {
                b = b.child(div().text_sm().text_color(theme.muted_foreground).child("等待遥测…"));
            }
        }
        section(cx, "遥测提供方", b)
    };

    // ---- metric ownership map (§12) ----
    let metrics_card = {
        let mut b = div().v_flex().gap_px();
        b = b.child(
            div()
                .h_flex()
                .gap_2()
                .child(div().w(px(200.)).text_xs().text_color(theme.muted_foreground).child("指标"))
                .child(div().w(px(130.)).text_xs().text_color(theme.muted_foreground).child("归属（权威源）"))
                .child(div().w(px(130.)).text_xs().text_color(theme.muted_foreground).child("实际来源"))
                .child(div().w(px(56.)).text_xs().text_color(theme.muted_foreground).child("质量"))
                .child(div().w(px(60.)).text_xs().text_color(theme.muted_foreground).child("周期"))
                .child(div().flex_1().text_xs().text_color(theme.muted_foreground).child("备注")),
        );
        for meta in registry::all() {
            let sample = state
                .telemetry
                .as_deref()
                .and_then(|s| s.samples.get(&meta.id));
            let (live_source, quality) = sample
                .map(|s| (Some(s.source), s.quality))
                .unwrap_or((None, MetricQuality::Unavailable));
            let fallback = live_source.is_some_and(|src| src != meta.owner);
            let q_color = match quality {
                MetricQuality::Fresh => theme.success,
                MetricQuality::Estimated | MetricQuality::Stale => theme.warning,
                _ => theme.muted_foreground,
            };
            b = b.child(
                div()
                    .h_flex()
                    .gap_2()
                    .py_px()
                    .child(div().w(px(200.)).text_sm().child(meta.id.0.to_string()))
                    .child(
                        div()
                            .w(px(130.))
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(fmt::source_zh(meta.owner)),
                    )
                    .child(
                        div()
                            .w(px(130.))
                            .text_sm()
                            .when(fallback, |d| d.text_color(theme.warning).font_semibold())
                            .child(
                                live_source
                                    .map(fmt::source_zh)
                                    .unwrap_or("—")
                                    .to_string(),
                            ),
                    )
                    .child(
                        div()
                            .w(px(56.))
                            .text_sm()
                            .text_color(q_color)
                            .child(fmt::quality_zh(quality)),
                    )
                    .child(
                        div()
                            .w(px(60.))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{} ms", meta.cadence.as_millis())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(meta.note),
                    ),
            );
        }
        section(cx, "指标归属映射（§12）— 高亮 = 回退源使用中", b)
    };

    // ---- journal tail ----
    let journal_card = {
        let diag_expanded = &diag.expanded;
        let body = div().v_flex().w_full().child(journal_view::render(
            &state.journal_tail,
            diag_expanded,
            "暂无日志条目——执行任意控制命令后出现",
            cx,
            |this: &mut ShellView, key: String| {
                if !this.diag.expanded.remove(&key) {
                    this.diag.expanded.insert(key);
                }
            },
        ));
        section(cx, "控制日志（实时 · 新→旧 · 点击展开步骤证据）", body)
    };

    page_root("diag-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(header)
            .child(identity)
            .child(section(cx, "引擎状态", engine_line))
            .child(ogh)
            .child(caps_card)
            .child(providers_card)
            .child(metrics_card)
            .child(journal_card),
    )
}
