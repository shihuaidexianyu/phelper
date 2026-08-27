//! MetricCard — one telemetry metric on the Dashboard: title, big value,
//! unit, and quality badge. Staleness rule (plan D-G):
//! gray out when the sample age exceeds 3× its registry cadence.

use std::time::Duration;

use gpui::{App, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder};
use gpui_component::{ActiveTheme, StyledExt};
use phelper_core::app::fmt;
use phelper_domain::telemetry::{MetricQuality, MetricSample};

pub struct MetricCard {
    title: SharedString,
    value: SharedString,
    unit: SharedString,
    subtitle: Option<SharedString>,
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
                subtitle: None,
                quality: Some(MetricQuality::Unavailable),
                stale: true,
            };
        };
        let quality = if s.value.as_f64().is_some() {
            s.quality
        } else {
            MetricQuality::Unavailable
        };
        let stale = s.timestamp.elapsed() > cadence * 3
            || matches!(
                quality,
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
            subtitle: None,
            quality: Some(quality),
            stale,
        }
    }

    /// A card not backed by one sample (e.g. the fan card composes two).
    pub fn custom(title: &str, value: String, unit: &str, subtitle: String) -> Self {
        Self {
            title: title.into(),
            value: value.into(),
            unit: unit.into(),
            subtitle: Some(subtitle.into()),
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
        // Healthy values speak for themselves. Keep a badge only for a
        // degraded value so the overview is quiet until it needs attention.
        let quality_badge = self
            .quality
            .and_then(|q| match (q, self.stale) {
                (MetricQuality::Fresh | MetricQuality::Estimated, true) => {
                    Some(MetricQuality::Stale)
                }
                (MetricQuality::Fresh, false) => None,
                (q, _) => Some(q),
            })
            .map(|q| {
                let color = match q {
                    MetricQuality::Fresh => theme.success,
                    MetricQuality::Estimated => theme.warning,
                    _ => theme.muted_foreground,
                };
                div().text_xs().text_color(color).child(fmt::quality_zh(q))
            });

        div()
            .v_flex()
            .gap_1()
            .p_2()
            .min_w_0()
            .flex_1()
            .rounded_md()
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
            .when_some(self.subtitle, |d, subtitle| {
                d.child(div().text_xs().text_color(sub_fg).child(subtitle))
            })
    }
}
