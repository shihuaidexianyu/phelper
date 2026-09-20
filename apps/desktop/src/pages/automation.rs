use super::dashboard::page_root;
use crate::shell::ShellView;
use gpui::{AppContext, Context, Entity, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_component::{
    ActiveTheme, Disableable, StyledExt,
    button::Button,
    input::{Input, InputState},
};
use phelper_core::{
    app::AppState,
    automation::{ApplicationRule, AutomationConfig},
};

pub(crate) struct AutomationEditor {
    inputs: [Entity<InputState>; 4],
    rules: Vec<ApplicationRule>,
    notice: Option<String>,
    battery_efficiency: bool,
}
impl AutomationEditor {
    pub fn new(
        config: &AutomationConfig,
        window: &mut Window,
        cx: &mut Context<ShellView>,
    ) -> Self {
        let placeholders = [
            "插电配置名 · 留空不接管",
            "电池配置名 · 留空不接管",
            "应用 .exe 完整路径",
            "应用配置名",
        ];
        let inputs: [Entity<InputState>; 4] = std::array::from_fn(|i| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholders[i]))
        });
        for (i, value) in [config.ac_profile.clone(), config.battery_profile.clone()]
            .into_iter()
            .enumerate()
        {
            inputs[i].update(cx, |s, cx| {
                s.set_value(value.unwrap_or_default(), window, cx)
            });
        }
        Self {
            inputs,
            rules: config.applications.clone(),
            notice: None,
            battery_efficiency: config.battery_efficiency,
        }
    }
    fn config(&self, enabled: bool, cx: &Context<ShellView>) -> AutomationConfig {
        let profile = |i: usize| {
            let value = self.inputs[i].read(cx).value().trim().to_string();
            (!value.is_empty()).then_some(value)
        };
        AutomationConfig {
            enabled,
            battery_efficiency: self.battery_efficiency,
            ac_profile: profile(0),
            battery_profile: profile(1),
            applications: self.rules.clone(),
        }
    }
}

pub fn render(
    state: &AppState,
    editor: &AutomationEditor,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let status = if !state.writes_available() {
        "只读模式 · 自动规则未运行"
    } else if state.automation.paused_by_manual {
        "自动规则已暂停"
    } else if state.automation.config.enabled {
        "自动规则已启用"
    } else {
        "自动规则未启用"
    };
    let mut content = div().v_flex().gap_3().p_4().w_full()
        .child(div().text_lg().font_semibold().child("自动切换"))
        .child(div().text_sm().child(status))
        .child(div().text_sm().text_color(theme.muted_foreground).child("应用运行期间使用对应配置，退出后恢复。手动调整优先，并暂停自动规则。列表中靠前的应用优先。"))
        .child(div().text_xs().child(format!("可用配置：{}", state.profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(" · "))))
        .child(div().h_flex().gap_2().child(Input::new(&editor.inputs[0]).w(px(245.))).child(Input::new(&editor.inputs[1]).w(px(245.))))
        .child(div().h_flex().gap_2()
            .child(Button::new("enable-automation").label("保存并启用 / 恢复").disabled(!state.writes_available()).on_click(cx.listener(|this, _, _, cx| {
                if let Some(editor) = &this.automation_editor { this.app.configure_automation(editor.config(true, cx)); } cx.notify();
            })))
            .child(Button::new("disable-automation").label("关闭并恢复接管").outline().disabled(!state.writes_available()).on_click(cx.listener(|this, _, _, cx| {
                let mut config = this.state.automation.config.clone(); config.enabled = false; this.app.configure_automation(config); cx.notify();
            }))));
    for message in [state.automation.message.as_ref(), editor.notice.as_ref()]
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
    if let Some(profile) = &state.automation.active_profile {
        content = content.child(div().text_sm().child(format!("当前自动会话：{profile}")));
    }
    content = content.child(div().text_base().font_semibold().child("应用规则"))
        .child(Button::new("battery-ecoqos").label(if editor.battery_efficiency { "电池进程节能：开启" } else { "电池进程节能：关闭" }).outline().on_click(cx.listener(|this, _, _, cx| {
            if let Some(editor) = &mut this.automation_editor { editor.battery_efficiency = !editor.battery_efficiency; }
            cx.notify();
        })))
        .child(div().text_xs().text_color(theme.muted_foreground).child("保存后生效。电池供电时为符合条件的用户进程设置 E 核 CPU Sets 与 EcoQoS；插电、关闭或手动接管时恢复。"))
        .child(div().text_xs().child(format!("进程节能：{:?} · 接管 {} 个进程 · {}", state.automation.process_policy.phase, state.automation.process_policy.managed_processes, state.automation.process_policy.last_error.as_deref().unwrap_or(""))))
        .child(Input::new(&editor.inputs[2])).child(Input::new(&editor.inputs[3]).w(px(245.)))
        .child(Button::new("add-application-rule").label("加入列表").outline().on_click(cx.listener(|this, _, _, cx| {
            if let Some(editor) = &mut this.automation_editor {
                let executable = editor.inputs[2].read(cx).value().trim().to_string();
                let profile = editor.inputs[3].read(cx).value().trim().to_string();
                if executable.is_empty() || profile.is_empty() { editor.notice = Some("请填写应用路径与配置名称".into()); }
                else { editor.rules.push(ApplicationRule { executable, profile, power: None }); editor.notice = Some("规则已加入编辑列表；点击保存并启用后生效。".into()); }
            }
            cx.notify();
        })));
    for (index, rule) in editor.rules.iter().enumerate() {
        content = content.child(
            div()
                .h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .child(format!("{} → {}", rule.executable, rule.profile)),
                )
                .child(
                    Button::new(("remove-rule", index))
                        .label("移除")
                        .outline()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(editor) = &mut this.automation_editor
                                && index < editor.rules.len()
                            {
                                editor.rules.remove(index);
                            }
                            cx.notify();
                        })),
                ),
        );
    }
    page_root("automation-scroll").child(content)
}
