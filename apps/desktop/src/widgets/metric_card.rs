//! MetricCard — one telemetry metric on the Dashboard: title, big value,
//! unit, quality badge, provenance subtitle. Staleness rule (plan D-G):
//! gray out when the sample age exceeds 3× its registry cadence.

use std::time::Duration;

use gpui::{App, IntoElement, ParentElement, SharedString, Styled, div};
use gpui_component::{ActiveTheme, StyledExt};
use phelper_core::app::fmt;
use phelper_domain::telemetry::{MetricQuality, MetricSample};

pub struct MetricCard {
    title: SharedString,
    value: SharedString,
    unit: SharedString,
    subtitle: SharedString,
    quality: Option<MetricQuality>,
    stale: bool,
}

impl MetricCard {
    /// Build from a live sample. `cadence` is the metric's registry cadence
    /// (telemetry::registry::meta) — age > 3× cadence = stale.
    pub fn from_sample(
        title: &str,
        sample: Option<&MetricSample>,
        cadence: Duration,
        decimals: usize,
        unit: &str,
    ) -> Self {
        let Some(s) = sample else {
            return Self {
                title: title.into(),
                value: "—".into(),
                unit: unit.into(),
                subtitle: "等待数据…".into(),
                quality: None,
                stale: true,
            };
        };
        let stale = s.timestamp.elapsed() > cadence * 3
            || matches!(
                s.quality,
                MetricQuality::Stale | MetricQuality::Unavailable | MetricQuality::Unsupported
            );
        let value = s
            .value
            .as_f64()
            .map(|v| format!("{v:.decimals$}"))
            .unwrap_or_else(|| "—".into());
        Self {
            title: title.into(),
            value: value.into(),
            unit: unit.into(),
            subtitle: fmt::source_zh(s.source).into(),
            quality: Some(s.quality),
            stale,
        }
    }

    /// A card not backed by one sample (e.g. the fan card composes two).
    pub fn custom(title: &str, value: String, unit: &str, subtitle: String) -> Self {
        Self {
            title: title.into(),
            value: value.into(),
            unit: unit.into(),
            subtitle: subtitle.into(),
            quality: None,
            stale: false,
        }
    }

    pub fn render(self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, sub_fg) = if self.stale {
            (theme.muted_foreground, theme.muted_foreground)
        } else {
            (theme.foreground, theme.muted_foreground)
        };
        let quality_badge = self.quality.map(|q| {
            let color = match q {
                MetricQuality::Fresh => theme.success,
                MetricQuality::Estimated => theme.warning,
                _ => theme.muted_foreground,
            };
            div()
                .text_xs()
                .text_color(color)
                .child(fmt::quality_zh(q))
        });

        div()
            .v_flex()
            .gap_1()
            .p_3()
            .min_w_0()
            .flex_1()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.group_box)
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .child(div().text_sm().text_color(sub_fg).child(self.title))
                    .children(quality_badge),
            )
            .child(
                div()
                    .h_flex()
                    .items_baseline()
                    .gap_1()
                    .child(div().text_2xl().text_color(fg).child(self.value))
                    .child(div().text_sm().text_color(sub_fg).child(self.unit)),
            )
            .child(div().text_xs().text_color(sub_fg).child(self.subtitle))
    }
}
