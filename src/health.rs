//! Startup health report (Q15): offline checks over the loaded
//! config, destinations, and scenarios, shown once on the Dashboard.
//!
//! No network calls: webhook reachability stays a manual,
//! per-destination check (the Destinations screen). The checks are
//! pure functions over plain data, so the rules are unit-tested
//! without a database or a window. Messages are Vietnamese (the
//! default UI language); the report is a summary, not a settings
//! screen — each issue points at the screen where it can be fixed.

use crate::destination::{Destination, FailureState};
use crate::scenario::Scenario;
use crate::store::{AppConfig, SendMode};

/// How much the user should care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// Where the fix lives, if the issue has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthTarget {
    Destinations,
    Builder,
    Settings,
}

#[derive(Debug, Clone)]
pub struct HealthIssue {
    pub severity: Severity,
    pub message: String,
    pub target: Option<HealthTarget>,
}

/// At most this many per-issue rows of one kind; the rest collapse
/// into a "+N nữa" line so a big setup cannot flood the Dashboard.
const MAX_ROWS_PER_KIND: usize = 5;

/// Run every startup check. `destinations` and `scenarios` are the
/// currently loaded lists; `config` is the loaded config. The order
/// is worst-first (errors, then warnings, then info).
pub fn startup_checks(
    config: &AppConfig,
    destinations: &[Destination],
    scenarios: &[Scenario],
) -> Vec<HealthIssue> {
    let mut issues = Vec::new();
    let webhook_mode = config.send_mode == SendMode::Webhook;

    if webhook_mode {
        let with_url = destinations
            .iter()
            .filter(|d| !d.encrypted_url.trim().is_empty())
            .count();
        if with_url == 0 {
            issues.push(HealthIssue {
                severity: Severity::Error,
                message: "Chưa có đích đến nào có URL webhook — thêm một đích đến để bắt đầu gửi."
                    .to_string(),
                target: Some(HealthTarget::Destinations),
            });
        } else if destinations.iter().all(|d| !d.enabled) {
            issues.push(HealthIssue {
                severity: Severity::Warning,
                message: "Tất cả đích đến đang tắt — engine sẽ không gửi tin nào.".to_string(),
                target: Some(HealthTarget::Destinations),
            });
        }
        // Paused destinations (persisted failure state).
        let paused: Vec<&Destination> = destinations
            .iter()
            .filter(|d| d.failure_state != FailureState::Ok)
            .collect();
        for d in paused.iter().take(MAX_ROWS_PER_KIND) {
            issues.push(HealthIssue {
                severity: Severity::Warning,
                message: format!(
                    "Đích đến '{}' đang tạm dừng ({}). Kiểm tra lại trong màn hình Đích đến.",
                    d.display_name(),
                    d.failure_state.label()
                ),
                target: Some(HealthTarget::Destinations),
            });
        }
        if paused.len() > MAX_ROWS_PER_KIND {
            issues.push(HealthIssue {
                severity: Severity::Warning,
                message: format!(
                    "Và {} đích đến khác cũng đang tạm dừng.",
                    paused.len() - MAX_ROWS_PER_KIND
                ),
                target: Some(HealthTarget::Destinations),
            });
        }
        if config.global_daily_cap == 0 {
            issues.push(HealthIssue {
                severity: Severity::Warning,
                message: "Giới hạn gửi toàn cục đang là 0 — không tin nào được gửi cho tới khi bạn tăng nó lên."
                    .to_string(),
                target: Some(HealthTarget::Settings),
            });
        }
        if destinations.len() as u32 > config.max_destinations.max(1) {
            issues.push(HealthIssue {
                severity: Severity::Warning,
                message: format!(
                    "Bạn có {} đích đến nhưng giới hạn đang là {} — không thêm được đích mới cho tới khi tăng giới hạn hoặc xóa bớt.",
                    destinations.len(),
                    config.max_destinations
                ),
                target: Some(HealthTarget::Settings),
            });
        }
    } else if config.encrypted_token.trim().is_empty() {
        issues.push(HealthIssue {
            severity: Severity::Error,
            message: "Chế độ Tài khoản đang bật nhưng chưa có token — thêm token trong Cài đặt hoặc chuyển sang chế độ Webhook."
                .to_string(),
            target: Some(HealthTarget::Settings),
        });
    }

    // Enabled scenarios that would fail at send time.
    let mut bad = 0usize;
    for s in scenarios.iter().filter(|s| s.enabled) {
        let result = if webhook_mode {
            s.validate_for_webhook()
        } else {
            s.validate()
        };
        if let Err(e) = result {
            bad += 1;
            if bad <= MAX_ROWS_PER_KIND {
                issues.push(HealthIssue {
                    severity: Severity::Warning,
                    message: format!("Kịch bản '{}' đang bật nhưng lỗi: {e}", s.name),
                    target: Some(HealthTarget::Builder),
                });
            }
        }
    }
    if bad > MAX_ROWS_PER_KIND {
        issues.push(HealthIssue {
            severity: Severity::Warning,
            message: format!(
                "Và {} kịch bản đang bật khác cũng lỗi.",
                bad - MAX_ROWS_PER_KIND
            ),
            target: Some(HealthTarget::Builder),
        });
    }

    if scenarios.is_empty() {
        issues.push(HealthIssue {
            severity: Severity::Info,
            message: "Chưa có kịch bản nào — tạo kịch bản đầu tiên trong màn hình Soạn."
                .to_string(),
            target: Some(HealthTarget::Builder),
        });
    }

    // Backup reminder: only when there is actually a setup to lose.
    if config.last_export_at.trim().is_empty()
        && (!scenarios.is_empty() || !destinations.is_empty())
    {
        issues.push(HealthIssue {
            severity: Severity::Info,
            message: "Bạn chưa xuất cấu hình lần nào — xuất một bản sao lưu trong Cài đặt để chuyển máy hoặc phòng khi mất dữ liệu."
                .to_string(),
            target: Some(HealthTarget::Settings),
        });
    }

    // Worst first, stable within a severity.
    issues.sort_by_key(|i| std::cmp::Reverse(i.severity));
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::Destination;

    fn webhook_config() -> AppConfig {
        AppConfig {
            send_mode: SendMode::Webhook,
            ..AppConfig::default()
        }
    }

    #[test]
    fn empty_webhook_setup_reports_error_and_info() {
        let issues = startup_checks(&webhook_config(), &[], &[]);
        assert!(issues.iter().any(|i| i.severity == Severity::Error));
        assert!(issues
            .iter()
            .any(|i| i.message.contains("Chưa có kịch bản")));
        // No backup reminder when there is nothing to back up.
        assert!(!issues.iter().any(|i| i.message.contains("chưa xuất")));
    }

    #[test]
    fn paused_destination_and_zero_cap_warn() {
        let mut d = Destination::new("Kênh A");
        d.encrypted_url = "blob".to_string();
        d.failure_state = FailureState::Paused404;
        let mut cfg = webhook_config();
        cfg.global_daily_cap = 0;
        let issues = startup_checks(&cfg, &[d], &[]);
        assert!(issues
            .iter()
            .any(|i| i.message.contains("Kênh A") && i.message.contains("tạm dừng")));
        assert!(issues
            .iter()
            .any(|i| i.message.contains("Giới hạn gửi toàn cục")));
        assert!(issues.iter().any(|i| i.message.contains("chưa xuất")));
    }

    #[test]
    fn healthy_setup_is_quiet_except_backup_reminder() {
        let mut d = Destination::new("Kênh A");
        d.encrypted_url = "blob".to_string();
        let mut cfg = webhook_config();
        cfg.last_export_at = "2026-11-17T00:00:00Z".to_string();
        let mut s = Scenario::new("Kịch bản");
        s.destination_ids = vec!["d1".to_string()];
        s.enabled = true;
        let issues = startup_checks(&cfg, &[d], &[s]);
        assert!(issues.is_empty(), "expected no issues, got: {issues:?}");
    }

    #[test]
    fn user_mode_without_token_errors() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.send_mode, SendMode::UserToken);
        let issues = startup_checks(&cfg, &[], &[]);
        assert!(issues
            .iter()
            .any(|i| i.severity == Severity::Error && i.message.contains("token")));
    }
}
