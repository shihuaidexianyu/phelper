use crate::shell::ShellView;
use gpui::{Context, IntoElement, ParentElement, Styled, div};
use gpui_component::{ActiveTheme, Disableable, StyledExt, button::Button};
use phelper_core::app::{AppState, KnobId, runtime::AppHandle};
use phelper_domain::{command::ControlCommand, policy::MuxMode};

pub fn render(state: &AppState, app: &AppHandle, cx: &mut Context<ShellView>) -> impl IntoElement {
    let theme = cx.theme();
    let available = state.writes_available()
        && state.caps.as_ref().is_some_and(|caps| {
            caps.mux == phelper_domain::capability::Support::Supported && caps.ppm.write_privileged
        })
        && cfg!(feature = "experimental-mux")
        && state.observed.mux_write_verified;
    let mut content = div().v_flex().gap_4().p_4().w_full()
        .child(div().text_lg().font_semibold().child("硬件能力"))
        .child(div().text_base().font_semibold().child("显卡切换 · MUX"))
        .child(div().text_sm().child(format!("固件报告：{}", match state.observed.mux.value() { Some(MuxMode::Hybrid) => "混合模式", Some(MuxMode::Discrete) => "独显模式", Some(_) => "其他模式", None => "暂不可用" })))
        .child(div().text_sm().child(if state.observed.mux_status.is_empty() { "控制尚未连接，无法核验切换记录".into() } else { state.observed.mux_status.clone() }))
        .child(div().text_xs().text_color(theme.muted_foreground).child("切换后需自行保存工作并重启 Windows。应用会在下次启动时核验；关闭应用不会撤销待重启请求。"));
    let mut buttons = div().h_flex().gap_2();
    for (id, label, mode) in [
        ("mux-hybrid", "请求混合模式", MuxMode::Hybrid),
        ("mux-discrete", "请求独显模式", MuxMode::Discrete),
    ] {
        let app = app.clone();
        buttons = buttons.child(
            Button::new(id)
                .label(label)
                .outline()
                .disabled(!available)
                .on_click(move |_, _, _| {
                    app.dispatch(KnobId::Mux, ControlCommand::SetMuxMode(mode))
                }),
        );
    }
    content = content.child(buttons)
        .child(div().text_xs().child(match state.knob_status(KnobId::Mux) {
            phelper_core::app::KnobStatus::Idle => "尚未请求切换".into(),
            phelper_core::app::KnobStatus::Failed { error, .. } => phelper_core::app::fmt::control_error_zh(error),
            phelper_core::app::KnobStatus::Pending | phelper_core::app::KnobStatus::InFlight(_) => "正在提交切换请求…".into(),
            _ => "请查看上方重启核验状态".into(),
        }))
        .child(div().text_base().font_semibold().child("降压与超频"))
        .child(div().text_sm().child(format!("XTU 服务运行：{} · OEM Intel SDK 文件：{} · VBS 状态：{}",
            state.hardware.xtu_service_running.map_or("未知", |v| if v { "是" } else { "否" }),
            if state.hardware.oem_intel_sdk.is_some() { "已发现" } else { "未发现" },
            match state.hardware.vbs_status { Some(0) => "未启用", Some(1) => "已启用但未运行", Some(2) => "正在运行", _ => "未知" })))
        .child(div().text_sm().text_color(theme.muted_foreground).child("以下能力按 2026-09-23 实机探测结论披露（hpqBIntM 两条命令空间只读全扫 + hpCpsPub 通道穷尽）；证据记录见 ogh-milestones.md。"));
    for (name, reason) in [
        ("CPU 降压", "XTU 驱动栈在位（iocbios2+SDK），但本机 VBS 激活：Intel 官方裁决 runtime 降压禁用；待 v0.4.0 立项"),
        ("CPU 超频", "与降压同通道，VBS 激活下 runtime OC 同禁；实用部分已由功耗墙（EXPERIMENTAL）覆盖"),
        ("内存超频", "已实测：固件两条命令空间（0x00–0x5F）未暴露任何内存超频接口"),
        ("电池充电上限", "已实测：全部已知通道穷尽（两条命令空间 + hpCpsPub 串行通道），本机固件不提供"),
    ] {
        content = content.child(div().text_sm().child(format!("{name}：{reason}")));
    }
    if state.hardware.vbs_status == Some(2) {
        content = content.child(
            div()
                .text_sm()
                .text_color(theme.warning)
                .child("本机 VBS 正在运行；Intel 文档将该配置列为运行时降压的限制条件。"),
        );
    }
    super::dashboard::page_root("hardware-scroll").child(content)
}
