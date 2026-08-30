//! Chinese presentation used by the two remaining desktop pages.

use phelper_domain::command::Verification;
use phelper_domain::error::ControlError;
use phelper_domain::policy::FanMode;
use phelper_domain::telemetry::MetricQuality;

pub fn control_error_zh(error: &ControlError) -> String {
    match error {
        ControlError::Unsupported => "本机不支持该功能".into(),
        ControlError::UnsafeRequest { reason } => format!("安全校验拒绝：{reason}"),
        ControlError::PermissionDenied => "权限不足——需要以管理员身份运行".into(),
        ControlError::DriverUnavailable { what } => format!("驱动不可用：{what}"),
        ControlError::FirmwareRejected { detail } => format!("固件拒绝了请求：{detail}"),
        ControlError::VerificationFailed { expected, actual } => {
            format!("写入后回读不符：期望 {expected}，读回 {actual}")
        }
        ControlError::Timeout => "操作超时".into(),
        ControlError::BackendUnavailable { what } => format!("后端不可用：{what}"),
        ControlError::Busy => "另一条控制操作正在进行中".into(),
        ControlError::UnknownProfile { name } => format!("未知配置档：{name}"),
    }
}

pub fn verification_zh(verification: &Verification) -> String {
    match verification {
        Verification::Verified => "已回读验证".into(),
        Verification::TrustedNoReadback => "信任写入（无回读通道，心跳维持）".into(),
        Verification::Failed { expected, actual } => {
            format!("验证失败：期望 {expected}，读回 {actual}")
        }
        Verification::Skipped => "未验证（已跳过）".into(),
    }
}

pub fn quality_zh(quality: MetricQuality) -> &'static str {
    match quality {
        MetricQuality::Fresh => "实时",
        MetricQuality::Estimated => "估算",
        MetricQuality::Stale => "陈旧",
        MetricQuality::Unavailable => "不可用",
        MetricQuality::Unsupported => "不支持",
    }
}

pub fn fan_mode_zh(mode: &FanMode) -> String {
    match mode {
        FanMode::FirmwareAuto => "固件自动".into(),
        FanMode::Max => "最大转速".into(),
        FanMode::Manual(level) => format!("手动 {} / {} RPM", level.cpu * 100, level.gpu * 100),
        FanMode::Curve(_) => "温度曲线".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_and_degraded_metric_labels_are_distinct() {
        assert_eq!(quality_zh(MetricQuality::Fresh), "实时");
        assert_eq!(quality_zh(MetricQuality::Unavailable), "不可用");
    }

    #[test]
    fn firmware_fan_mode_is_human_readable() {
        assert_eq!(fan_mode_zh(&FanMode::FirmwareAuto), "固件自动");
    }
}
