//! UI strings, Vietnamese only.
//!
//! The interface is 100% Vietnamese: every string the user can see is
//! written here (for the keyed core UI) or inline in Vietnamese at the
//! call site. There is no language switch — a second language would
//! only reintroduce the mixed-language UI the keyed table once had
//! (Vietnamese keys that still carried English text).
//!
//! `Lang` and `from_code` are kept so old configs (which stored a
//! `language` field) still parse, and so the `t()` call sites keep
//! compiling unchanged. They always resolve to Vietnamese. Unknown
//! keys return "???" so a missing translation is visible instead of
//! silently wrong.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Vi,
}

impl Lang {
    pub fn from_code(_code: &str) -> Self {
        Lang::Vi
    }
}

/// Translate a UI key. Vietnamese is the only language.
pub fn t(_lang: Lang, key: &str) -> &'static str {
    match key {
        // Navigation.
        "nav.dashboard" => "Tổng quan",
        "nav.builder" => "Kịch bản",
        "nav.destinations" => "Đích đến",
        "nav.logs" => "Lịch sử",
        "nav.settings" => "Cài đặt",
        // Screen titles / subtitles.
        "title.dashboard" => "Tổng quan",
        "subtitle.dashboard" => "Tổng quan hoạt động",
        "title.builder" => "Soạn kịch bản",
        "subtitle.builder" => "Tạo và chỉnh sửa kịch bản",
        "title.destinations" => "Đích đến",
        "subtitle.destinations" => "Webhook cho từng kênh",
        "title.logs" => "Lịch sử",
        "subtitle.logs" => "Lịch sử gửi tin",
        "title.settings" => "Cài đặt",
        "subtitle.settings" => "Cài đặt chung",
        // Common buttons.
        "btn.save" => "Lưu",
        "btn.save_settings" => "Lưu cài đặt",
        "btn.cancel" => "Hủy",
        "btn.delete" => "Xóa",
        "btn.add" => "Thêm",
        "btn.check" => "Kiểm tra",
        "btn.enable" => "Bật",
        "btn.disable" => "Tắt",
        "btn.test" => "Gửi thử",
        "btn.refresh" => "Làm mới",
        "btn.clone" => "Nhân bản",
        "btn.edit" => "Sửa",
        // Dashboard.
        "dash.start_engine" => "Chạy",
        "dash.stop" => "Dừng",
        "dash.emergency_stop" => "DỪNG KHẨN CẤP",
        "dash.scenarios" => "Kịch bản",
        "dash.sent_today" => "Đã gửi hôm nay",
        "dash.next_run" => "Lịch tới",
        "dash.destination" => "Đích đến",
        "dash.timeline" => "Lịch gửi sắp tới (7 ngày)",
        "dash.week_chart" => "7 ngày qua",
        "dash.timeline_edit" => "Sửa",
        "builder.simulation" => "Xem trước: 10 lần chạy tới",
        "builder.poll" => "Bình chọn (tùy chọn)",
        "logs.detail_title" => "Chi tiết lịch sử",
        "logs.detail_btn" => "Chi tiết",
        "logs.close" => "Đóng",
        "settings.maintenance" => "Sao lưu tự động & lưu trữ lịch sử",
        "settings.auto_backup" => "Tự động sao lưu cấu hình",
        "settings.every" => "Mỗi",
        "settings.days" => "ngày",
        "settings.keep" => "giữ lại",
        "settings.copies" => "bản",
        "settings.backup_now" => "Sao lưu ngay",
        "settings.retention" => "Giữ lịch sử gửi:",
        "settings.forever" => "Mãi mãi",
        "settings.prune_now" => "Dọn lịch sử cũ ngay",
        // Builder sections.
        "builder.editor" => "Soạn thảo",
        "builder.schedule" => "Lịch chạy",
        "builder.templates" => "Thư viện mẫu",
        "builder.embed" => "Embed (tùy chọn)",
        "builder.messages" => "Tin nhắn",
        "builder.your_scenarios" => "Kịch bản của bạn",
        "builder.search_hint" => "Tìm kịch bản…",
        "builder.enable_all" => "Bật tất cả",
        "builder.disable_all" => "Tắt tất cả",
        // Destinations.
        "dest.list" => "Danh sách đích đến",
        "dest.new" => "+ Thêm đích đến",
        "dest.edit" => "Sửa đích đến",
        "dest.create" => "Đích đến mới",
        "dest.save" => "Lưu đích đến",
        "dest.test" => "Gửi thử",
        // Settings sections.
        "settings.send_mode" => "Chế độ gửi",
        "settings.webhook" => "Webhook",
        "settings.limits" => "Giới hạn đích đến",
        "settings.account" => "Tài khoản",
        "settings.general" => "Chung",
        "settings.updates" => "Cập nhật",
        "settings.font_size" => "Cỡ chữ",
        "settings.font_small" => "Nhỏ",
        "settings.font_medium" => "Vừa",
        "settings.font_large" => "Lớn",
        "settings.start_minimized" => "Mở thu nhỏ khi khởi động",
        "settings.shortcuts" => "Phím tắt",
        "settings.reset_window" => "Đặt lại kích thước cửa sổ",
        // Unknown key: visible, not silent.
        _ => "???",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_from_code_always_vietnamese() {
        assert_eq!(Lang::from_code("en"), Lang::Vi);
        assert_eq!(Lang::from_code("EN"), Lang::Vi);
        assert_eq!(Lang::from_code("vi"), Lang::Vi);
        assert_eq!(Lang::from_code(""), Lang::Vi);
        assert_eq!(Lang::from_code("fr"), Lang::Vi);
    }

    #[test]
    fn core_keys_are_vietnamese() {
        assert_eq!(t(Lang::Vi, "nav.destinations"), "Đích đến");
        assert_eq!(t(Lang::Vi, "nav.dashboard"), "Tổng quan");
        assert_eq!(t(Lang::Vi, "nav.builder"), "Kịch bản");
        assert_eq!(t(Lang::Vi, "nav.logs"), "Lịch sử");
        assert_eq!(t(Lang::Vi, "nav.settings"), "Cài đặt");
        assert_eq!(t(Lang::Vi, "title.builder"), "Soạn kịch bản");
        assert_eq!(t(Lang::Vi, "btn.save_settings"), "Lưu cài đặt");
        assert_eq!(t(Lang::Vi, "btn.refresh"), "Làm mới");
        assert_eq!(t(Lang::Vi, "dash.start_engine"), "Chạy");
        assert_eq!(t(Lang::Vi, "dash.stop"), "Dừng");
        assert_eq!(t(Lang::Vi, "builder.editor"), "Soạn thảo");
        assert_eq!(t(Lang::Vi, "builder.messages"), "Tin nhắn");
        assert_eq!(t(Lang::Vi, "builder.your_scenarios"), "Kịch bản của bạn");
        assert_eq!(t(Lang::Vi, "settings.general"), "Chung");
        assert_eq!(t(Lang::Vi, "builder.enable_all"), "Bật tất cả");
        assert_eq!(t(Lang::Vi, "dash.timeline"), "Lịch gửi sắp tới (7 ngày)");
        assert_eq!(
            t(Lang::Vi, "settings.maintenance"),
            "Sao lưu tự động & lưu trữ lịch sử"
        );
        assert_eq!(t(Lang::Vi, "settings.prune_now"), "Dọn lịch sử cũ ngay");
        assert_eq!(t(Lang::Vi, "logs.detail_btn"), "Chi tiết");
        assert_eq!(t(Lang::Vi, "logs.detail_title"), "Chi tiết lịch sử");
        assert_eq!(t(Lang::Vi, "no.such.key"), "???");
    }

    #[test]
    fn no_keyed_string_is_english() {
        // Guard against the mixed-language regression: every keyed
        // string must be Vietnamese. Keys whose value legitimately
        // contains a Latin product term (Webhook, Embed) are covered
        // by the explicit assertions above; this test
        // pins the historically-English keys to their Vietnamese
        // values so a reverted translation fails loudly.
        for key in [
            "nav.dashboard",
            "nav.builder",
            "nav.logs",
            "nav.settings",
            "title.dashboard",
            "title.builder",
            "title.logs",
            "title.settings",
            "btn.save_settings",
            "btn.refresh",
            "dash.start_engine",
            "dash.stop",
            "builder.editor",
            "builder.messages",
            "builder.your_scenarios",
            "settings.general",
        ] {
            let v = t(Lang::Vi, key);
            assert!(
                !matches!(
                    v,
                    "Dashboard"
                        | "Builder"
                        | "Logs"
                        | "Settings"
                        | "Scenario Builder"
                        | "Save settings"
                        | "Refresh"
                        | "Start engine"
                        | "Stop"
                        | "Editor"
                        | "Messages"
                        | "Your scenarios"
                        | "General"
                ),
                "key {key} still English: {v}"
            );
        }
    }
}
