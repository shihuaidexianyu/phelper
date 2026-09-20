//! Explicit draft editing. Typing never writes hardware; Apply validates the whole plan.
use super::dashboard::page_root;
use crate::shell::ShellView;
use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    ActiveTheme, Disableable, StyledExt,
    button::Button,
    input::{Input, InputState},
};
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::{AppState, EXPERIMENTAL_COMPILED, KnobId, KnobStatus, fmt};
use phelper_domain::command::ControlCommand;
use phelper_domain::policy::{
    BoostPolicy, CpuPolicy, CpuPowerLimits, FanCurve, FanCurvePoint, FanLevels, FanMode,
    ThermalMode,
};
use phelper_domain::profile::{GpuPolicyPatch, PerformanceProfile};

const CPU_ROWS: [&str; 5] = [
    "性能偏好 EPP · 0–100",
    "E 核性能偏好 · 0–100",
    "频率上限 MHz · 0 不限",
    "性能下限 %",
    "性能上限 %",
];
const BOOSTS: [Option<BoostPolicy>; 8] = [
    None,
    Some(BoostPolicy::Disabled),
    Some(BoostPolicy::Enabled),
    Some(BoostPolicy::Aggressive),
    Some(BoostPolicy::EfficientEnabled),
    Some(BoostPolicy::EfficientAggressive),
    Some(BoostPolicy::AggressiveGuaranteed),
    Some(BoostPolicy::EfficientAggressiveGuaranteed),
];
const BOOST_LABELS: [&str; 8] = [
    "继承",
    "禁用睿频",
    "启用睿频",
    "积极睿频",
    "高效睿频",
    "高效积极",
    "积极保障",
    "高效积极保障",
];

pub(crate) struct PerformanceEditor {
    pub name: Entity<InputState>,
    cpu: [Entity<InputState>; 10],
    curve: [Entity<InputState>; 12],
    manual: [Entity<InputState>; 2],
    power: [Entity<InputState>; 3],
    boost: [usize; 2],
    thermal: usize,
    fan: usize,
    gpu: [usize; 2],
    base: PerformanceProfile,
    pub notice: Option<String>,
}

impl PerformanceEditor {
    pub fn new(window: &mut Window, cx: &mut Context<ShellView>) -> Self {
        let mut input =
            |placeholder| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        Self {
            name: input("自定义配置名称"),
            cpu: std::array::from_fn(|_| input("继承")),
            curve: std::array::from_fn(|i| input(if i % 3 == 0 { "°C" } else { "RPM" })),
            manual: std::array::from_fn(|_| input("RPM")),
            power: std::array::from_fn(|_| input("继承")),
            boost: [0, 0],
            thermal: 0,
            fan: 0,
            gpu: [0, 0],
            base: PerformanceProfile {
                description: "自定义性能配置；空白字段继承当前设置".into(),
                ..Default::default()
            },
            notice: None,
        }
    }

    pub fn load(
        &mut self,
        name: &str,
        p: &PerformanceProfile,
        window: &mut Window,
        cx: &mut Context<ShellView>,
    ) {
        self.base = p.clone();
        self.name
            .update(cx, |s, cx| s.set_value(name.to_string(), window, cx));
        let c = &p.cpu;
        let values = [
            c.epp_ac.map(u32::from),
            c.epp_dc.map(u32::from),
            c.epp1_ac.map(u32::from),
            c.epp1_dc.map(u32::from),
            c.max_freq_mhz_ac,
            c.max_freq_mhz_dc,
            c.min_performance_ac.map(u32::from),
            c.min_performance_dc.map(u32::from),
            c.max_performance_ac.map(u32::from),
            c.max_performance_dc.map(u32::from),
        ];
        for (input, value) in self.cpu.iter().zip(values) {
            input.update(cx, |s, cx| {
                s.set_value(value.map(|v| v.to_string()).unwrap_or_default(), window, cx)
            });
        }
        self.boost = [
            c.boost_policy_ac.or(c.boost_policy),
            c.boost_policy_dc.or(c.boost_policy),
        ]
        .map(|v| BOOSTS.iter().position(|p| *p == v).unwrap_or(0));
        self.thermal = match p.thermal_mode {
            None => 0,
            Some(ThermalMode::Balanced) => 1,
            Some(ThermalMode::Performance) => 2,
        };
        self.fan = match p.fan {
            None => 0,
            Some(FanMode::Curve(_)) => 1,
            Some(FanMode::Manual(_)) => 2,
            Some(FanMode::Max) => 3,
            Some(FanMode::FirmwareAuto) => 4,
        };
        let curve = match p.fan {
            Some(FanMode::Curve(c)) => c,
            _ => FanCurve::balanced(),
        };
        for (i, input) in self.curve.iter().enumerate() {
            let point = curve.points[i / 3];
            let value = match i % 3 {
                0 => u32::from(point.temp_c),
                1 => point.left_rpm(),
                _ => point.right_rpm(),
            };
            input.update(cx, |s, cx| s.set_value(value.to_string(), window, cx));
        }
        let manual = match p.fan {
            Some(FanMode::Manual(l)) => l,
            _ => FanLevels::new(30, 30),
        };
        for (input, value) in self
            .manual
            .iter()
            .zip([manual.left_rpm(), manual.right_rpm()])
        {
            input.update(cx, |s, cx| s.set_value(value.to_string(), window, cx));
        }
        self.gpu = [
            p.gpu_policy.and_then(|p| p.ctgp),
            p.gpu_policy.and_then(|p| p.ppab),
        ]
        .map(|p| match p {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        });
        let limits = p.power_limits;
        for (input, value) in self.power.iter().zip([
            limits.map(|l| l.pl1_w),
            limits.map(|l| l.pl2_w),
            limits.filter(|l| l.pl4_w != 0).map(|l| l.pl4_w),
        ]) {
            input.update(cx, |s, cx| {
                s.set_value(value.map(|v| v.to_string()).unwrap_or_default(), window, cx)
            });
        }
        self.notice = None;
    }

    pub fn profile(&self, cx: &Context<ShellView>) -> Result<PerformanceProfile, String> {
        let parse = |input: &Entity<InputState>| -> Result<Option<u32>, String> {
            let text = input.read(cx).value();
            if text.trim().is_empty() {
                Ok(None)
            } else {
                text.trim()
                    .parse::<u32>()
                    .map(Some)
                    .map_err(|_| format!("请输入非负整数：{text}"))
            }
        };
        let small = |value: Option<u32>| -> Result<Option<u8>, String> {
            value
                .map(|v| u8::try_from(v).map_err(|_| "数值超出范围".to_string()))
                .transpose()
        };
        let v: Vec<_> = self.cpu.iter().map(parse).collect::<Result<_, _>>()?;
        let cpu = CpuPolicy {
            epp_ac: small(v[0])?,
            epp_dc: small(v[1])?,
            epp1_ac: small(v[2])?,
            epp1_dc: small(v[3])?,
            max_freq_mhz_ac: v[4],
            max_freq_mhz_dc: v[5],
            min_performance_ac: small(v[6])?,
            min_performance_dc: small(v[7])?,
            max_performance_ac: small(v[8])?,
            max_performance_dc: small(v[9])?,
            boost_policy_ac: BOOSTS[self.boost[0]],
            boost_policy_dc: BOOSTS[self.boost[1]],
            ..Default::default()
        };
        let level = |input: &Entity<InputState>| -> Result<u16, String> {
            let rpm = parse(input)?.ok_or("请填写风扇转速")?;
            if rpm == 0 || rpm % 100 != 0 {
                return Err("风扇转速必须为正数，且为 100 RPM 的倍数".into());
            }
            u16::try_from(rpm / 100).map_err(|_| "风扇转速超出范围".into())
        };
        let fan = match self.fan {
            0 => None,
            1 => {
                let mut points = FanCurve::balanced().points;
                for (i, point) in points.iter_mut().enumerate() {
                    *point = FanCurvePoint::new(
                        small(parse(&self.curve[i * 3])?)?.ok_or("请填写曲线温度")?,
                        level(&self.curve[i * 3 + 1])?,
                        level(&self.curve[i * 3 + 2])?,
                    );
                }
                let curve = FanCurve::new(points);
                curve.validate().map_err(str::to_owned)?;
                Some(FanMode::Curve(curve))
            }
            2 => Some(FanMode::Manual(FanLevels::new(
                level(&self.manual[0])?,
                level(&self.manual[1])?,
            ))),
            3 => Some(FanMode::Max),
            _ => Some(FanMode::FirmwareAuto),
        };
        let switch = |v| match v {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        };
        let patch = GpuPolicyPatch {
            ctgp: switch(self.gpu[0]),
            ppab: switch(self.gpu[1]),
            ..self.base.gpu_policy.unwrap_or_default()
        };
        let gpu_policy = (patch != GpuPolicyPatch::default()).then_some(patch);
        let power: Vec<_> = self.power.iter().map(parse).collect::<Result<_, _>>()?;
        let power_limits = if power.iter().all(Option::is_none) {
            None
        } else {
            if !EXPERIMENTAL_COMPILED {
                return Err("当前构建未启用实验性功率控制".into());
            }
            let limits = CpuPowerLimits {
                pl1_w: small(power[0])?.ok_or("请填写 PL1")?,
                pl2_w: small(power[1])?.ok_or("请填写 PL2")?,
                pl4_w: small(power[2])?.unwrap_or(0),
                cpu_gpu_concurrent_w: 0,
            };
            // Same shared envelope check the CLI pre-flight runs (domain) —
            // previously this form relied solely on the post-engine safety
            // rejection, which surfaced as "filled in, then rejected".
            limits
                .validate()
                .map_err(|e| format!("功率限制超出范围：{e}"))?;
            Some(limits)
        };
        Ok(PerformanceProfile {
            description: self.base.description.clone(),
            thermal_mode: match self.thermal {
                1 => Some(ThermalMode::Balanced),
                2 => Some(ThermalMode::Performance),
                _ => None,
            },
            cpu,
            fan,
            gpu_policy,
            power_limits,
            os_policy: self.base.os_policy.clone(),
        })
    }
}

pub fn render(
    state: &AppState,
    app: &AppHandle,
    editor: &PerformanceEditor,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let disabled = !state.writes_available()
        || matches!(
            state.knob_status(KnobId::Profile),
            KnobStatus::InFlight(_) | KnobStatus::Pending
        );
    let mut content = div()
        .v_flex()
        .gap_3()
        .p_4()
        .w_full()
        .child(div().text_lg().font_semibold().child("性能控制"))
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("空白与“继承”保留当前字段。手动 PPM 设置退出后保留；恢复按钮交还硬件控制，自动会话另有 PPM 恢复记录。"),
        );
    let mut presets = div().h_flex().flex_wrap().gap_2();
    for (i, summary) in state.profiles.iter().enumerate() {
        let p = summary.profile.clone();
        let name = if summary.builtin {
            format!("my-{}", summary.name)
        } else {
            summary.name.clone()
        };
        presets = presets.child(
            Button::new(("load-draft", i))
                .label(format!("载入 {}", summary.name))
                .outline()
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(editor) = &mut this.performance_editor {
                        editor.load(&name, &p, window, cx);
                    }
                    cx.notify();
                })),
        );
    }
    content = content.child(presets).child(Input::new(&editor.name));
    let save = app.clone();
    let apply = app.clone();
    let recover = app.clone();
    content = content.child(
        div()
            .h_flex()
            .gap_2()
            .child(
                Button::new("save-draft")
                    .label("保存配置档")
                    .outline()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(editor) = &mut this.performance_editor {
                            match editor.profile(cx) {
                                Ok(p) => save.save_profile(
                                    editor.name.read(cx).value().trim().to_string(),
                                    p,
                                ),
                                Err(e) => editor.notice = Some(e),
                            }
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("apply-draft")
                    .label("应用编辑内容")
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(editor) = &mut this.performance_editor {
                            match editor.profile(cx) {
                                Ok(p) => {
                                    editor.notice = None;
                                    apply.dispatch(
                                        KnobId::Profile,
                                        ControlCommand::ApplyProfileDefinition {
                                            profile: Box::new(p),
                                        },
                                    );
                                }
                                Err(e) => editor.notice = Some(e),
                            }
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("restore-session")
                    .label("恢复硬件与自动会话")
                    .outline()
                    .disabled(!state.writes_available())
                    .on_click(move |_, _, _| {
                        recover.dispatch(KnobId::Recovery, ControlCommand::RestoreSession)
                    }),
            ),
    );
    for message in [
        editor.notice.as_ref(),
        state.profile_notice.as_ref(),
        state.observed.control_notice.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        content = content.child(
            div()
                .text_sm()
                .text_color(theme.warning)
                .child(message.clone()),
        );
    }
    if let Some(outcome) = &state.last_outcome {
        let message = match &outcome.status {
            phelper_domain::command::ControlStatus::Applied { verification } => {
                fmt::verification_zh(verification)
            }
            phelper_domain::command::ControlStatus::Rejected { error } => {
                fmt::control_error_zh(error)
            }
            phelper_domain::command::ControlStatus::Partial => {
                "部分完成，请查看诊断中的逐项结果".into()
            }
        };
        content = content.child(div().text_sm().child(message));
    }
    content = content.child(div().text_base().font_semibold().child("CPU · 插电 / 电池"));
    if let Some(ppm) = &state.windows_ppm {
        content = content.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("当前电源计划：{}", ppm.active_scheme_name)),
        );
    }
    for (row, label) in CPU_ROWS.iter().enumerate() {
        content = content.child(
            div()
                .h_flex()
                .gap_2()
                .items_center()
                .child(div().w(px(195.)).text_sm().child(*label))
                .child(Input::new(&editor.cpu[row * 2]).w(px(115.)))
                .child(Input::new(&editor.cpu[row * 2 + 1]).w(px(115.))),
        );
    }
    let mut boost_row = div()
        .h_flex()
        .gap_2()
        .child(div().w(px(195.)).text_sm().child("睿频策略"));
    for rail in 0..2 {
        boost_row = boost_row.child(
            Button::new(("boost-draft", rail))
                .label(BOOST_LABELS[editor.boost[rail]])
                .outline()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(e) = &mut this.performance_editor {
                        e.boost[rail] = (e.boost[rail] + 1) % BOOSTS.len();
                    }
                    cx.notify();
                })),
        );
    }
    content = content
        .child(boost_row)
        .child(div().text_base().font_semibold().child("散热与风扇"))
        .child(
            div()
                .h_flex()
                .gap_2()
                .child(
                    Button::new("thermal-draft")
                        .label(["热模式：继承", "热模式：均衡", "热模式：性能"][editor.thermal])
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(e) = &mut this.performance_editor {
                                e.thermal = (e.thermal + 1) % 3;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("fan-draft")
                        .label(
                            [
                                "风扇：继承",
                                "风扇：软件曲线",
                                "风扇：固定转速",
                                "风扇：全速",
                                "风扇：交还固件",
                            ][editor.fan],
                        )
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(e) = &mut this.performance_editor {
                                e.fan = (e.fan + 1) % 5;
                            }
                            cx.notify();
                        })),
                ),
        );
    if editor.fan == 1 {
        content = content.child(div().text_xs().child("温度 °C / 左风扇 RPM / 右风扇 RPM"));
        for point in 0..4 {
            content = content.child(div().h_flex().gap_2().children(
                (0..3).map(|column| Input::new(&editor.curve[point * 3 + column]).w(px(130.))),
            ));
        }
    }
    if editor.fan == 2 {
        content = content.child(
            div()
                .h_flex()
                .gap_2()
                .child(Input::new(&editor.manual[0]).w(px(150.)))
                .child(Input::new(&editor.manual[1]).w(px(150.))),
        );
    }
    content = content.child(div().text_base().font_semibold().child("GPU 平台策略"));
    for index in 0..2 {
        content = content.child(
            Button::new(("gpu-draft", index))
                .label(format!(
                    "{}：{}",
                    ["cTGP", "动态功率分配 PPAB"][index],
                    ["继承", "开启", "关闭"][editor.gpu[index]]
                ))
                .outline()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(e) = &mut this.performance_editor {
                        e.gpu[index] = (e.gpu[index] + 1) % 3;
                    }
                    cx.notify();
                })),
        );
    }
    let power_available = EXPERIMENTAL_COMPILED
        && state
            .caps
            .as_ref()
            .is_some_and(|c| c.power_limits == phelper_domain::capability::Support::Experimental);
    content = content.when(power_available, |content| {
        content
            .child(
                div()
                    .text_base()
                    .font_semibold()
                    .child("实验性 CPU 功率限制"),
            )
            .child(
                div()
                    .text_xs()
                    .child("PL1 / PL2 / PL4，单位 W；PL4 留空则不修改。退出时恢复接管前的值。"),
            )
            .child(
                div().h_flex().gap_2().children(
                    editor
                        .power
                        .iter()
                        .map(|input| Input::new(input).w(px(130.))),
                ),
            )
    });
    page_root("performance-scroll").child(content)
}
