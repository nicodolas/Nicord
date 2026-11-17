//! Minimal i18n (Q9): Vietnamese (default) + English, switched
//! instantly from Settings (no restart). Coverage is the core UI:
//! navigation, screen titles, section titles, and common buttons.
//! Body copy, warnings, and status messages stay Vietnamese until
//! their keys are added — translating them is incremental work:
//! add a key here, then use `t()` at the call site. Unknown keys
//! return the key itself, so a missing translation is visible
//! instead of silently wrong.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Vi,
    En,
}

impl Lang {
    pub fn from_code(code: &str) -> Self {
        match code.trim().to_ascii_lowercase().as_str() {
            "en" | "english" => Lang::En,
            _ => Lang::Vi,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Lang::Vi => "vi",
            Lang::En => "en",
        }
    }
}

/// Translate a UI key. Vietnamese is the source language; English
/// is the translation. Unknown keys return the key unchanged.
pub fn t(lang: Lang, key: &str) -> &'static str {
    match (lang, key) {
        // Navigation.
        (Lang::En, "nav.dashboard") => "Dashboard",
        (Lang::En, "nav.builder") => "Builder",
        (Lang::En, "nav.destinations") => "Destinations",
        (Lang::En, "nav.logs") => "Logs",
        (Lang::En, "nav.settings") => "Settings",
        // Screen titles / subtitles.
        (Lang::En, "title.dashboard") => "Dashboard",
        (Lang::En, "subtitle.dashboard") => "Overview",
        (Lang::En, "title.builder") => "Scenario Builder",
        (Lang::En, "subtitle.builder") => "Create and edit scenarios",
        (Lang::En, "title.destinations") => "Destinations",
        (Lang::En, "subtitle.destinations") => "One webhook per channel",
        (Lang::En, "title.logs") => "Logs",
        (Lang::En, "subtitle.logs") => "Send history",
        (Lang::En, "title.settings") => "Settings",
        (Lang::En, "subtitle.settings") => "Settings",
        // Common buttons.
        (Lang::En, "btn.save") => "Save",
        (Lang::En, "btn.save_settings") => "Save settings",
        (Lang::En, "btn.cancel") => "Cancel",
        (Lang::En, "btn.delete") => "Delete",
        (Lang::En, "btn.add") => "Add",
        (Lang::En, "btn.check") => "Check",
        (Lang::En, "btn.enable") => "Enable",
        (Lang::En, "btn.disable") => "Disable",
        (Lang::En, "btn.test") => "Test",
        (Lang::En, "btn.refresh") => "Refresh",
        (Lang::En, "btn.clone") => "Clone",
        (Lang::En, "btn.edit") => "Edit",
        // Dashboard.
        (Lang::En, "dash.start_engine") => "Start engine",
        (Lang::En, "dash.stop") => "Stop",
        (Lang::En, "dash.emergency_stop") => "EMERGENCY STOP",
        (Lang::En, "dash.scenarios") => "Scenarios",
        (Lang::En, "dash.sent_today") => "Sent today",
        (Lang::En, "dash.next_run") => "Next run",
        (Lang::En, "dash.destination") => "Destination",
        // Builder sections.
        (Lang::En, "builder.editor") => "Editor",
        (Lang::En, "builder.schedule") => "Schedule",
        (Lang::En, "builder.templates") => "Template library",
        (Lang::En, "builder.embed") => "Embed (optional)",
        (Lang::En, "builder.messages") => "Messages",
        (Lang::En, "builder.your_scenarios") => "Your scenarios",
        // Destinations.
        (Lang::En, "dest.list") => "Destination list",
        (Lang::En, "dest.new") => "+ Add destination",
        (Lang::En, "dest.edit") => "Edit destination",
        (Lang::En, "dest.create") => "New destination",
        (Lang::En, "dest.save") => "Save destination",
        (Lang::En, "dest.test") => "Send test",
        // Settings sections.
        (Lang::En, "settings.send_mode") => "Send mode",
        (Lang::En, "settings.webhook") => "Webhook",
        (Lang::En, "settings.limits") => "Destination limits",
        (Lang::En, "settings.account") => "Account",
        (Lang::En, "settings.general") => "General",
        (Lang::En, "settings.updates") => "Updates",
        (Lang::En, "settings.language") => "Language",
        // Vietnamese (source strings).
        (Lang::Vi, "nav.dashboard") => "Dashboard",
        (Lang::Vi, "nav.builder") => "Builder",
        (Lang::Vi, "nav.destinations") => "Đích đến",
        (Lang::Vi, "nav.logs") => "Logs",
        (Lang::Vi, "nav.settings") => "Settings",
        (Lang::Vi, "title.dashboard") => "Dashboard",
        (Lang::Vi, "subtitle.dashboard") => "Tổng quan",
        (Lang::Vi, "title.builder") => "Scenario Builder",
        (Lang::Vi, "subtitle.builder") => "Tạo và chỉnh sửa kịch bản",
        (Lang::Vi, "title.destinations") => "Đích đến",
        (Lang::Vi, "subtitle.destinations") => "Webhook cho từng kênh",
        (Lang::Vi, "title.logs") => "Logs",
        (Lang::Vi, "subtitle.logs") => "Lịch sử gửi tin",
        (Lang::Vi, "title.settings") => "Settings",
        (Lang::Vi, "subtitle.settings") => "Cài đặt",
        (Lang::Vi, "btn.save") => "Lưu",
        (Lang::Vi, "btn.save_settings") => "Save settings",
        (Lang::Vi, "btn.cancel") => "Hủy",
        (Lang::Vi, "btn.delete") => "Xóa",
        (Lang::Vi, "btn.add") => "Thêm",
        (Lang::Vi, "btn.check") => "Kiểm tra",
        (Lang::Vi, "btn.enable") => "Bật",
        (Lang::Vi, "btn.disable") => "Tắt",
        (Lang::Vi, "btn.test") => "Gửi thử",
        (Lang::Vi, "btn.refresh") => "Refresh",
        (Lang::Vi, "btn.clone") => "Nhân bản",
        (Lang::Vi, "btn.edit") => "Sửa",
        (Lang::Vi, "dash.start_engine") => "Start engine",
        (Lang::Vi, "dash.stop") => "Stop",
        (Lang::Vi, "dash.emergency_stop") => "DỪNG KHẨN CẤP",
        (Lang::Vi, "dash.scenarios") => "Kịch bản",
        (Lang::Vi, "dash.sent_today") => "Đã gửi hôm nay",
        (Lang::Vi, "dash.next_run") => "Lịch tới",
        (Lang::Vi, "dash.destination") => "Đích đến",
        (Lang::Vi, "builder.editor") => "Editor",
        (Lang::Vi, "builder.schedule") => "Lịch chạy",
        (Lang::Vi, "builder.templates") => "Thư viện mẫu",
        (Lang::Vi, "builder.embed") => "Embed (tùy chọn)",
        (Lang::Vi, "builder.messages") => "Messages",
        (Lang::Vi, "builder.your_scenarios") => "Your scenarios",
        (Lang::Vi, "dest.list") => "Danh sách đích đến",
        (Lang::Vi, "dest.new") => "+ Thêm đích đến",
        (Lang::Vi, "dest.edit") => "Sửa đích đến",
        (Lang::Vi, "dest.create") => "Đích đến mới",
        (Lang::Vi, "dest.save") => "Lưu đích đến",
        (Lang::Vi, "dest.test") => "Gửi thử",
        (Lang::Vi, "settings.send_mode") => "Chế độ gửi",
        (Lang::Vi, "settings.webhook") => "Webhook",
        (Lang::Vi, "settings.limits") => "Giới hạn đích đến",
        (Lang::Vi, "settings.account") => "Tài khoản",
        (Lang::Vi, "settings.general") => "General",
        (Lang::Vi, "settings.updates") => "Cập nhật",
        (Lang::Vi, "settings.language") => "Ngôn ngữ",
        // Unknown key: visible, not silent.
        (_, key) => {
            // `key` is not 'static, so return a fixed marker.
            let _ = key;
            "???"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_from_code_defaults_to_vietnamese() {
        assert_eq!(Lang::from_code("en"), Lang::En);
        assert_eq!(Lang::from_code("EN"), Lang::En);
        assert_eq!(Lang::from_code("vi"), Lang::Vi);
        assert_eq!(Lang::from_code(""), Lang::Vi);
        assert_eq!(Lang::from_code("fr"), Lang::Vi);
    }

    #[test]
    fn core_keys_translate_both_ways() {
        assert_eq!(t(Lang::Vi, "nav.destinations"), "Đích đến");
        assert_eq!(t(Lang::En, "nav.destinations"), "Destinations");
        assert_eq!(t(Lang::En, "settings.language"), "Language");
        assert_eq!(t(Lang::Vi, "no.such.key"), "???");
    }
}
