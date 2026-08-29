//! Compact process/thread scheduling surface.
//!
//! The page exposes the useful OS-level controls without putting them on the
//! hardware performance page.  All mutations go through AppHandle; this file
//! never opens a Windows handle.

use gpui::{
    ClickEvent, Context, IntoElement, ParentElement, Styled, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::{
    ActiveTheme, Disableable, StyledExt,
    button::{Button, ButtonVariants},
    input::Input,
};
use phelper_core::app::AppState;
use phelper_core::app::runtime::AppHandle;
use phelper_domain::automatic::{AutomaticMode, AutomaticPhase, PowerSource};
use phelper_domain::os_policy::{
    AffinityMask, CpuPlacement, GpuPreference, MemoryPriority, OsPolicyOwner, OsPolicyTarget,
    OsSchedulingPolicy, ProcessPriority, ProcessorRef, QosLevel, ThreadPriority,
};

use crate::shell::{OsPolicyState, ShellView};

use super::dashboard::page_root;

fn parse_u64(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("不能为空".into());
    }
    if let Some(value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(value, 16).map_err(|_| "不是有效数字".into())
    } else {
        value.parse::<u64>().map_err(|_| "不是有效数字".into())
    }
}

fn text(
    input: &gpui::Entity<gpui_component::input::InputState>,
    cx: &mut Context<ShellView>,
) -> String {
    input.read(cx).text().to_string()
}

fn target_and_policy(
    os: &OsPolicyState,
    cx: &mut Context<ShellView>,
) -> Result<(OsPolicyTarget, OsSchedulingPolicy), String> {
    let pid = text(&os.pid, cx);
    let tid = text(&os.tid, cx);
    let target = match (pid.trim().is_empty(), tid.trim().is_empty()) {
        (false, true) => OsPolicyTarget::Process {
            pid: pid.trim().parse().map_err(|_| "PID 无效".to_string())?,
        },
        (true, false) => OsPolicyTarget::Thread {
            tid: tid.trim().parse().map_err(|_| "TID 无效".to_string())?,
        },
        (true, true) => return Err("请输入 PID 或 TID".into()),
        (false, false) => return Err("PID 和 TID 只能填一个".into()),
    };

    let custom_cpu_sets = text(&os.cpu_sets, cx);
    let cpu_placement = if custom_cpu_sets.trim().is_empty() {
        os.placement.clone()
    } else {
        let mut ids = Vec::new();
        for value in custom_cpu_sets.split(',') {
            ids.push(
                value
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| "CPU Set ID 无效".to_string())?,
            );
        }
        CpuPlacement::Custom(ids)
    };

    let affinity_group = text(&os.affinity_group, cx);
    let affinity_mask = text(&os.affinity_mask, cx);
    let affinity = match (
        affinity_group.trim().is_empty(),
        affinity_mask.trim().is_empty(),
    ) {
        (true, true) => None,
        (false, false) => Some(AffinityMask {
            group: affinity_group
                .trim()
                .parse()
                .map_err(|_| "Affinity 组无效".to_string())?,
            mask: parse_u64(&affinity_mask)?,
        }),
        _ => return Err("Affinity 组和 mask 需要同时填写".into()),
    };

    let ideal_group = text(&os.ideal_group, cx);
    let ideal_number = text(&os.ideal_number, cx);
    let ideal_processor = match (
        ideal_group.trim().is_empty(),
        ideal_number.trim().is_empty(),
    ) {
        (true, true) => None,
        (false, false) => Some(ProcessorRef {
            group: ideal_group
                .trim()
                .parse()
                .map_err(|_| "理想处理器组无效".to_string())?,
            number: ideal_number
                .trim()
                .parse()
                .map_err(|_| "理想处理器编号无效".to_string())?,
        }),
        _ => return Err("理想处理器组和编号需要同时填写".into()),
    };

    let mut policy = OsSchedulingPolicy {
        cpu_placement: (os.placement_touched || !custom_cpu_sets.trim().is_empty())
            .then_some(cpu_placement),
        affinity,
        qos: os.qos_touched.then_some(os.qos),
        process_priority: None,
        thread_priority: None,
        memory_priority: os.memory_priority_touched.then_some(os.memory_priority),
        ideal_processor: ideal_processor
            .filter(|_| matches!(target, OsPolicyTarget::Thread { .. })),
        gpu_preference: None,
    };
    match target {
        OsPolicyTarget::Process { .. } => {
            policy.process_priority = os.process_priority_touched.then_some(os.process_priority);
            policy.gpu_preference = os.gpu_touched.then_some(os.gpu_preference);
            if ideal_processor.is_some() {
                return Err("理想处理器只对 TID 目标生效".into());
            }
        }
        OsPolicyTarget::Thread { .. } => {
            policy.thread_priority = os.thread_priority_touched.then_some(os.thread_priority);
        }
    }
    policy
        .validate_for(&target)
        .map_err(|reason| reason.to_string())?;
    Ok((target, policy))
}

fn choice_button(
    label: &'static str,
    active: bool,
    id: (&'static str, usize),
    on_click: impl Fn(&mut ShellView, &ClickEvent, &mut Window, &mut Context<ShellView>) + 'static,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    Button::new(id)
        .label(label)
        .when(active, |button| button.primary())
        .when(!active, |button| button.outline())
        .on_click(cx.listener(on_click))
}

fn row(label: &'static str, content: impl IntoElement) -> impl IntoElement {
    div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(div().w(px(72.)).text_sm().child(label))
        .child(content)
}

pub fn render(
    state: &AppState,
    app: &AppHandle,
    os: &OsPolicyState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    // Keep an owned theme snapshot while constructing listeners below;
    // `cx.listener` needs a mutable Context.
    let theme = cx.theme().clone();
    let app_refresh = app.clone();
    let refresh = Button::new("os-refresh")
        .label("刷新")
        .outline()
        .on_click(cx.listener(move |_, _: &ClickEvent, _: &mut Window, _| {
            app_refresh.refresh_os_data();
        }));
    let topology = state.os_policy.topology.as_ref().map(|topology| {
        format!(
            "P {} · E {} · CPU Sets {}",
            topology.performance_ids.len(),
            topology.efficiency_ids.len(),
            topology.cpu_sets.len()
        )
    });
    let header = div()
        .h_flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .h_flex()
                .items_baseline()
                .gap_2()
                .child(div().text_xl().font_semibold().child("应用调度"))
                .when_some(topology, |d, topology| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(topology),
                    )
                }),
        )
        .child(refresh);

    let auto_app_off = app.clone();
    let auto_off = Button::new("auto-off")
        .label("关闭")
        .when(state.automatic.mode == AutomaticMode::Off, |button| {
            button.primary()
        })
        .when(state.automatic.mode != AutomaticMode::Off, |button| {
            button.outline()
        })
        .on_click(cx.listener(move |_, _: &ClickEvent, _: &mut Window, _| {
            auto_app_off.set_automatic_mode(AutomaticMode::Off);
        }));
    let auto_app_battery = app.clone();
    let auto_battery = Button::new("auto-battery")
        .label("电池节能")
        .when(
            state.automatic.mode == AutomaticMode::BatteryEfficiency,
            |button| button.primary(),
        )
        .when(
            state.automatic.mode != AutomaticMode::BatteryEfficiency,
            |button| button.outline(),
        )
        .on_click(cx.listener(move |_, _: &ClickEvent, _: &mut Window, _| {
            auto_app_battery.set_automatic_mode(AutomaticMode::BatteryEfficiency);
        }));
    let automatic_power = state
        .automatic
        .power
        .as_ref()
        .map(|power| match power.source {
            PowerSource::Ac => "交流",
            PowerSource::Battery => "电池",
            PowerSource::Unknown => "供电未知",
        })
        .unwrap_or("供电未知");
    let automatic_status = match state.automatic.phase {
        AutomaticPhase::Disabled => "未启用".to_string(),
        AutomaticPhase::Waiting => format!("{automatic_power} · 等待"),
        AutomaticPhase::Applying => format!("{automatic_power} · 调整中"),
        AutomaticPhase::Active => format!(
            "{automatic_power} · {} 个进程",
            state.automatic.managed_processes
        ),
        AutomaticPhase::Error => "不可用".to_string(),
    };
    let automatic_card = div()
        .h_flex()
        .items_center()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("自动调度"))
        .child(auto_off)
        .child(auto_battery)
        .child(
            div()
                .text_xs()
                .text_color(if state.automatic.phase == AutomaticPhase::Error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(automatic_status),
        )
        .when_some(state.automatic.last_error.clone(), |d, error| {
            d.when(state.automatic.phase == AutomaticPhase::Error, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.danger)
                        .overflow_hidden()
                        .child(error),
                )
            })
        });

    let target_row = div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(div().text_sm().font_semibold().child("目标"))
        .child(div().w(px(150.)).child(Input::new(&os.pid)))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("PID"),
        )
        .child(div().w(px(150.)).child(Input::new(&os.tid)))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("TID"),
        );

    let mut placement_buttons = div().h_flex().gap_1();
    for (label, placement, id) in [
        ("全部", CpuPlacement::All, 0usize),
        ("P 核", CpuPlacement::Performance, 1),
        ("E 核", CpuPlacement::Efficiency, 2),
    ] {
        let active = os.placement == placement;
        placement_buttons = placement_buttons.child(choice_button(
            label,
            active,
            ("os-placement", id),
            move |this, _, _, cx| {
                this.os.placement = placement.clone();
                this.os.placement_touched = true;
                cx.notify();
            },
            cx,
        ));
    }
    let cpu_card = div()
        .v_flex()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("CPU"))
        .child(row("CPU Sets", placement_buttons))
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(72.)).text_sm().child("自定义"))
                .child(div().w(px(230.)).child(Input::new(&os.cpu_sets)))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("ID,ID…"),
                ),
        );

    let mut qos_buttons = div().h_flex().gap_1();
    for (label, value, id) in [
        ("系统", QosLevel::System, 0usize),
        ("高性能", QosLevel::High, 1),
        ("节能", QosLevel::Eco, 2),
    ] {
        let active = os.qos == value;
        qos_buttons = qos_buttons.child(choice_button(
            label,
            active,
            ("os-qos", id),
            move |this, _, _, cx| {
                this.os.qos = value;
                this.os.qos_touched = true;
                cx.notify();
            },
            cx,
        ));
    }
    let mut memory_buttons = div().h_flex().gap_1();
    for (label, value, id) in [
        ("低", MemoryPriority::Low, 0usize),
        ("中", MemoryPriority::Medium, 1),
        ("普通", MemoryPriority::Normal, 2),
    ] {
        let active = os.memory_priority == value;
        memory_buttons = memory_buttons.child(choice_button(
            label,
            active,
            ("os-memory", id),
            move |this, _, _, cx| {
                this.os.memory_priority = value;
                this.os.memory_priority_touched = true;
                cx.notify();
            },
            cx,
        ));
    }
    let exec_card = div()
        .v_flex()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("执行"))
        .child(row("QoS", qos_buttons))
        .child(row("内存", memory_buttons));

    let mut process_priority_buttons = div().h_flex().gap_1();
    for (label, value, id) in [
        ("低", ProcessPriority::BelowNormal, 0usize),
        ("普通", ProcessPriority::Normal, 1),
        ("高", ProcessPriority::AboveNormal, 2),
        ("更高", ProcessPriority::High, 3),
    ] {
        let active = os.process_priority == value;
        process_priority_buttons = process_priority_buttons.child(choice_button(
            label,
            active,
            ("os-process-priority", id),
            move |this, _, _, cx| {
                this.os.process_priority = value;
                this.os.process_priority_touched = true;
                cx.notify();
            },
            cx,
        ));
    }
    let mut thread_priority_buttons = div().h_flex().gap_1();
    for (label, value, id) in [
        ("低", ThreadPriority::Lowest, 0usize),
        ("普通", ThreadPriority::Normal, 1),
        ("高", ThreadPriority::AboveNormal, 2),
        ("更高", ThreadPriority::Highest, 3),
    ] {
        let active = os.thread_priority == value;
        thread_priority_buttons = thread_priority_buttons.child(choice_button(
            label,
            active,
            ("os-thread-priority", id),
            move |this, _, _, cx| {
                this.os.thread_priority = value;
                this.os.thread_priority_touched = true;
                cx.notify();
            },
            cx,
        ));
    }
    let mut gpu_buttons = div().h_flex().gap_1();
    for (label, value, id) in [
        ("系统", GpuPreference::System, 0usize),
        ("节能 GPU", GpuPreference::PowerSaving, 1),
        ("高性能 GPU", GpuPreference::HighPerformance, 2),
    ] {
        let active = os.gpu_preference == value;
        gpu_buttons = gpu_buttons.child(choice_button(
            label,
            active,
            ("os-gpu", id),
            move |this, _, _, cx| {
                this.os.gpu_preference = value;
                this.os.gpu_touched = true;
                cx.notify();
            },
            cx,
        ));
    }
    let priority_card = div()
        .v_flex()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("优先级与 GPU"))
        .child(row("进程", process_priority_buttons))
        .child(row("线程", thread_priority_buttons))
        .child(row("图形", gpu_buttons));

    let apply_app = app.clone();
    let apply = Button::new("os-apply")
        .label("应用")
        .primary()
        .disabled(matches!(
            state.engine,
            phelper_core::app::EngineStatus::Starting | phelper_core::app::EngineStatus::Failed(_)
        ))
        .on_click(cx.listener(
            move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, cx| {
                let snapshot = &this.os;
                let result = target_and_policy(snapshot, cx);
                match result {
                    Ok((target, policy)) => {
                        this.os.note = None;
                        apply_app.apply_os_policy(target, policy);
                    }
                    Err(error) => {
                        this.os.note = Some((error, false));
                    }
                }
                cx.notify();
            },
        ));
    let advanced_toggle = Button::new("os-advanced")
        .label(if os.advanced {
            "收起高级"
        } else {
            "高级"
        })
        .outline()
        .on_click(
            cx.listener(|this: &mut ShellView, _: &ClickEvent, _: &mut Window, cx| {
                this.os.advanced = !this.os.advanced;
                cx.notify();
            }),
        );
    let actions = div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(apply)
        .child(advanced_toggle)
        .when_some(
            os.note.as_ref().map(|(message, ok)| (message.clone(), *ok)),
            |d, (message, ok)| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(if ok { theme.success } else { theme.danger })
                        .child(message),
                )
            },
        )
        .when_some(state.os_policy_error.clone(), |d, error| {
            d.child(div().text_xs().text_color(theme.danger).child(error))
        });

    let advanced = if os.advanced {
        Some(
            div()
                .v_flex()
                .gap_2()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.group_box)
                .child(div().text_base().font_semibold().child("高级"))
                .child(row(
                    "Affinity",
                    div()
                        .h_flex()
                        .gap_2()
                        .child(div().w(px(100.)).child(Input::new(&os.affinity_group)))
                        .child(div().w(px(180.)).child(Input::new(&os.affinity_mask))),
                ))
                .child(row(
                    "理想处理器",
                    div()
                        .h_flex()
                        .gap_2()
                        .child(div().w(px(100.)).child(Input::new(&os.ideal_group)))
                        .child(div().w(px(100.)).child(Input::new(&os.ideal_number))),
                )),
        )
    } else {
        None
    };

    let manual_active = state
        .os_policy
        .active
        .iter()
        .filter(|item| item.owner == OsPolicyOwner::Manual)
        .collect::<Vec<_>>();
    let active = if manual_active.is_empty() {
        div()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child("无")
    } else {
        let mut rows = div().v_flex().gap_1();
        for (index, item) in manual_active.iter().enumerate() {
            let label = match item.target {
                OsPolicyTarget::Process { pid } => format!(
                    "PID {pid} · {}",
                    item.executable.as_deref().unwrap_or("未知程序")
                ),
                OsPolicyTarget::Thread { tid } => format!(
                    "TID {tid} · {}",
                    item.executable.as_deref().unwrap_or("未知程序")
                ),
            };
            let target = item.target;
            let app2 = app.clone();
            rows = rows.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().text_xs().overflow_hidden().child(label))
                    .child(
                        Button::new(("os-restore", index))
                            .label("恢复")
                            .outline()
                            .on_click(cx.listener(move |_, _: &ClickEvent, _: &mut Window, _| {
                                app2.restore_os_policy(target);
                            })),
                    ),
            );
        }
        rows
    };
    let active_card = div()
        .v_flex()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("当前接管"))
        .child(active);

    let process_rows = state.os_processes.iter().take(12).map(|process| {
        div()
            .h_flex()
            .gap_2()
            .items_center()
            .child(div().w(px(60.)).text_xs().child(process.pid.to_string()))
            .child(div().flex_1().text_xs().child(process.name.clone()))
            .child(
                div()
                    .w(px(54.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{} 线程", process.thread_count)),
            )
    });
    let process_card = div()
        .v_flex()
        .gap_1()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("进程"))
        .children(process_rows);

    page_root("applications-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(header)
            .child(automatic_card)
            .child(target_row)
            .child(
                div()
                    .h_flex()
                    .gap_3()
                    .w_full()
                    .child(cpu_card)
                    .child(exec_card)
                    .child(priority_card),
            )
            .when_some(advanced, |d, advanced| d.child(advanced))
            .child(actions)
            .child(active_card)
            .child(process_card),
    )
}
