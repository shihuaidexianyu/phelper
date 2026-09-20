use super::dashboard::page_root;
use crate::shell::ShellView;
use gpui::{AppContext, Context, Entity, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_component::{
    ActiveTheme, Disableable, StyledExt,
    button::Button,
    input::{Input, InputState},
};
use phelper_core::app::{AppState, fmt};

pub(crate) struct MeasurementEditor {
    inputs: [Entity<InputState>; 3],
    notice: Option<String>,
}
impl MeasurementEditor {
    pub fn new(window: &mut Window, cx: &mut Context<ShellView>) -> Self {
        let labels = ["游戏进程 PID", "秒数 · 5–300", "测试标签，例如 OGH-性能"];
        let inputs: [Entity<InputState>; 3] = std::array::from_fn(|i| {
            cx.new(|cx| InputState::new(window, cx).placeholder(labels[i]))
        });
        inputs[1].update(cx, |s, cx| s.set_value("60", window, cx));
        Self {
            inputs,
            notice: None,
        }
    }
}

pub fn render(
    state: &AppState,
    editor: &MeasurementEditor,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let mut content = div().v_flex().gap_3().p_4().w_full()
        .child(div().text_lg().font_semibold().child("性能验证与诊断"))
        .child(div().text_sm().text_color(theme.muted_foreground).child("使用相同场景和画质分别采集。统计采用帧数最多的交换链；呈现帧率与屏幕实际显示帧率可能不同。"))
        .child(div().h_flex().gap_2().child(Input::new(&editor.inputs[0]).w(px(170.))).child(Input::new(&editor.inputs[1]).w(px(150.))))
        .child(Input::new(&editor.inputs[2]))
        .child(div().h_flex().gap_2()
            .child(Button::new("record-frames").label("开始采集").disabled(state.capture_running).on_click(cx.listener(|this, _, _, cx| {
                if let Some(editor) = &mut this.measurement_editor {
                    let pid = editor.inputs[0].read(cx).value().trim().parse::<u32>();
                    let seconds = editor.inputs[1].read(cx).value().trim().parse::<u32>();
                    match (pid, seconds) {
                        (Ok(pid), Ok(seconds)) => { editor.notice = None; this.app.start_capture(pid, seconds, editor.inputs[2].read(cx).value().to_string()); },
                        _ => editor.notice = Some("请填写有效的 PID 和采集秒数".into()),
                    }
                } cx.notify();
            })))
            .child(Button::new("stop-frames").label("停止采集").outline().disabled(!state.capture_running).on_click(cx.listener(|this, _, _, _| this.app.stop_capture())))
            .child(Button::new("baseline-frames").label("将本次设为对照").outline().disabled(state.capture_report.is_none()).on_click(cx.listener(|this, _, _, _| this.app.use_capture_baseline()))));
    for notice in [editor.notice.as_ref(), state.capture_notice.as_ref()]
        .into_iter()
        .flatten()
    {
        content = content.child(
            div()
                .text_sm()
                .text_color(theme.warning)
                .child(notice.clone()),
        );
    }
    if let Some(report) = &state.capture_report {
        let f = &report.frames;
        content = content
            .child(
                div()
                    .text_base()
                    .font_semibold()
                    .child(format!("{} · {} 帧", report.label, f.frames)),
            )
            .child(div().text_sm().child(format!(
                "平均 {:.1} FPS · 1% Low {:.1} FPS · P95 {:.2} ms · P99 {:.2} ms",
                f.mean_fps, f.one_percent_low_fps, f.p95_ms, f.p99_ms
            )))
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "1% Low = 最慢 1% 呈现间隔的平均值的倒数。报告同时保存硬件采样、配置与原始 CSV。",
            ));
        if let Some(baseline) = &state.capture_baseline {
            if baseline.executable.eq_ignore_ascii_case(&report.executable) {
                content = content.child(div().text_sm().child(format!(
                    "相对 {}：平均帧率 {:+.1}% · P99 帧时间 {:+.2} ms",
                    baseline.label,
                    (f.mean_fps / baseline.frames.mean_fps - 1.0) * 100.0,
                    f.p99_ms - baseline.frames.p99_ms
                )));
            } else {
                content =
                    content.child(div().text_sm().child("对照来自不同应用，请重新选择对照。"));
            }
        }
    }
    content = content
        .child(div().text_base().font_semibold().child("控制证据"))
        .child(
            Button::new("export-diagnostics")
                .label("导出诊断报告")
                .outline()
                .on_click(cx.listener(|this, _, _, _| this.app.export_diagnostics())),
        );
    if let Some(path) = &state.diagnostic_path {
        content = content.child(div().text_xs().child(path.clone()));
    }
    if let Some(identity) = &state.identity {
        content = content.child(div().text_sm().child(format!(
            "{} · 主板 {} · BIOS {}",
            identity.product_name, identity.board_id, identity.bios_version
        )));
    }
    if let Some(outcome) = &state.last_outcome {
        for step in &outcome.steps {
            content = content.child(
                div()
                    .v_flex()
                    .gap_1()
                    .p_2()
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(div().text_sm().child(format!(
                        "{} · {}",
                        step.step,
                        fmt::verification_zh(&step.verification)
                    )))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} → {}",
                                step.before.as_deref().unwrap_or("—"),
                                step.after.as_deref().unwrap_or("—")
                            )),
                    ),
            );
        }
    }
    if let Some(telemetry) = &state.telemetry {
        for (name, status) in &telemetry.providers {
            content = content.child(div().text_xs().child(format!("{name}：{status:?}")));
        }
    }
    page_root("validation-scroll").child(content)
}
