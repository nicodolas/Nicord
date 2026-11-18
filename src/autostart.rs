//! Real OS autostart hook.
//!
//! Windows: the Run key `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! (value `Nicord` = quoted path of the current exe).
//! Linux: an XDG desktop entry `~/.config/autostart/nicord.desktop`.
//! Other platforms: `is_enabled` is false and `set_enabled` errors.

#[cfg(target_os = "windows")]
pub fn is_enabled() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .ok()
        .and_then(|k| k.get_value::<String, _>("Nicord").ok())
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu
        .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map_err(|e| format!("không mở được khóa Run: {e}"))?;
    if enabled {
        let exe =
            std::env::current_exe().map_err(|e| format!("không tìm được đường dẫn exe: {e}"))?;
        let value = format!("\"{}\"", exe.display());
        run.set_value("Nicord", &value)
            .map_err(|e| format!("không ghi được khóa Run: {e}"))?;
    } else {
        // Deleting a missing value is not an error.
        let _ = run.delete_value("Nicord");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn is_enabled() -> bool {
    autostart_file().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let path = autostart_file().ok_or_else(|| "không tìm được thư mục autostart".to_string())?;
    if enabled {
        let exe =
            std::env::current_exe().map_err(|e| format!("không tìm được đường dẫn exe: {e}"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, desktop_entry(&exe.display().to_string()))
            .map_err(|e| e.to_string())?;
    } else if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn autostart_file() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("autostart").join("nicord.desktop"))
}

/// The XDG desktop entry written for autostart.
#[cfg(target_os = "linux")]
fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Nicord\nExec=\"{exe}\"\nX-GNOME-Autostart-enabled=true\n"
    )
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn set_enabled(_enabled: bool) -> Result<(), String> {
    Err("nền tảng này không hỗ trợ tự khởi động".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_probe_never_panics() {
        // No side effects: just reads the current OS state.
        let _ = is_enabled();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_entry_points_at_the_exe() {
        let entry = desktop_entry("/opt/nicord/nicord");
        assert!(entry.contains("[Desktop Entry]"));
        assert!(entry.contains("Exec=\"/opt/nicord/nicord\""));
        assert!(entry.contains("X-GNOME-Autostart-enabled=true"));
    }
}
