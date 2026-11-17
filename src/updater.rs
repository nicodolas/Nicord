//! In-app auto-update (no login, no user data sent).
//!
//! On startup (background thread, never blocks the UI) Nicord asks the
//! public GitHub Releases API for the latest release of this repo and
//! compares its tag with the running binary's own version
//! (`env!("CARGO_PKG_VERSION")`). The API call is unauthenticated
//! (60 requests/hour/IP — one check per launch is far below that) and
//! sends nothing but a User-Agent; no token, no account, no machine id.
//!
//! An update NEVER installs silently: the user sees a dialog and picks
//! "Cập nhật ngay" or "Để sau". If there are no releases yet (the
//! releases page was emptied), the API answers 404 and the check just
//! reports "no update available" — quietly, no error dialog.
//!
//! Install (Windows, the release target): a running exe cannot
//! overwrite itself, so the download is extracted to a temp staging
//! dir and a small helper `.bat` waits for Nicord's process to exit,
//! swaps the new files over the install folder (keeping a
//! `Nicord.exe.bak` backup), deletes the staging dir and relaunches
//! Nicord. The user's data is never touched: `config.json` and
//! `nicord.db` live in the OS config dir (`%APPDATA%\nicord` /
//! `~/.config/nicord`), not next to the exe.
//! Linux uses the same flow with a `.sh` helper instead of `.bat`;
//! the Linux asset name is looked up the same way, so the flow works
//! the moment a Linux release asset exists.

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Public GitHub API: latest release of this repo (no auth needed).
pub const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/nicodolas/Nicord/releases/latest";

/// Name of the checksum asset every release publishes (see
/// `.github/workflows/release.yml`). The updater refuses to install
/// a release that has no checksum file.
pub const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";

/// Largest release download accepted. A real Nicord zip is a few
/// MB; 100 MiB leaves ample headroom while bounding a runaway or
/// malicious response.
pub const MAX_DOWNLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// Largest checksum file accepted. SHA256SUMS.txt is a few lines.
pub const MAX_CHECKSUMS_BYTES: u64 = 64 * 1024;

/// Version of the running binary, from Cargo.toml.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parse "1.2.3" or "v1.2.3". Anything else (wrong part count,
    /// non-numeric parts, empty) is `None` — and `None` never counts
    /// as an update, so a malformed tag can never trigger a download.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: Version,
    pub asset: ReleaseAsset,
    /// Download URL of the release's SHA256SUMS.txt asset, when the
    /// release published one. `None` fails the install closed.
    pub checksums_url: Option<String>,
}

/// The release-zip name the release workflow produces, for this
/// platform. Windows is the release target today; Linux looks for a
/// Linux asset the same way, so the flow works when one exists.
pub fn expected_asset_name(version: &Version) -> String {
    #[cfg(target_os = "windows")]
    {
        format!("Nicord-v{version}-windows-x64.zip")
    }
    #[cfg(not(target_os = "windows"))]
    {
        format!("Nicord-v{version}-linux-x64.zip")
    }
}

/// Pick the downloadable asset for `version`: the exact expected name
/// first, then any `.zip` for this platform ("windows"/"linux" in the
/// name), otherwise nothing — never a wrong-platform file.
pub fn pick_asset(assets: &[ReleaseAsset], version: &Version) -> Option<ReleaseAsset> {
    let expected = expected_asset_name(version);
    if let Some(a) = assets.iter().find(|a| a.name == expected) {
        return Some(a.clone());
    }
    let marker = if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    assets
        .iter()
        .find(|a| a.name.ends_with(".zip") && a.name.contains(marker))
        .cloned()
}

/// Parse a `GET /releases/latest` JSON body. `None` means "no usable
/// release": bad tag, or no downloadable asset for this platform.
pub fn parse_release(json: &serde_json::Value) -> Option<ReleaseInfo> {
    let tag = json.get("tag_name")?.as_str()?;
    let version = Version::parse(tag)?;
    let assets_json = json.get("assets")?.as_array()?;
    let assets: Vec<ReleaseAsset> = assets_json
        .iter()
        .filter_map(|a| {
            Some(ReleaseAsset {
                name: a.get("name")?.as_str()?.to_string(),
                url: a.get("browser_download_url")?.as_str()?.to_string(),
                size: a.get("size").and_then(|s| s.as_u64()),
            })
        })
        .collect();
    let asset = pick_asset(&assets, &version)?;
    let checksums_url = assets
        .iter()
        .find(|a| a.name == CHECKSUMS_ASSET_NAME)
        .map(|a| a.url.clone());
    Some(ReleaseInfo {
        tag: tag.to_string(),
        version,
        asset,
        checksums_url,
    })
}

#[derive(Debug, Clone)]
pub enum CheckResult {
    /// A newer release exists — ask the user, never install silently.
    Available(ReleaseInfo),
    /// Running the latest version (or the latest tag is not newer).
    UpToDate,
    /// No releases exist (API 404) or no asset for this platform.
    NoRelease,
    /// Network / API error. The caller logs it quietly; no dialog.
    Failed(String),
}

/// One background check against the public Releases API. Sends no user
/// data — only a User-Agent. Never panics on bad responses.
pub async fn check_for_update() -> CheckResult {
    let client = match reqwest::Client::builder()
        .user_agent(format!("Nicord/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => return CheckResult::Failed(format!("không tạo được HTTP client: {e}")),
    };
    let resp = match client
        .get(RELEASES_LATEST_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return CheckResult::Failed(format!("không kết nối được GitHub: {e}")),
    };
    if resp.status() == StatusCode::NOT_FOUND {
        // No releases published yet (the releases page was emptied).
        return CheckResult::NoRelease;
    }
    if !resp.status().is_success() {
        return CheckResult::Failed(format!("GitHub trả về HTTP {}", resp.status()));
    }
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => return CheckResult::Failed(format!("không đọc được phản hồi GitHub: {e}")),
    };
    let Some(info) = parse_release(&json) else {
        return CheckResult::Failed("không đọc được thông tin bản phát hành".to_string());
    };
    if info.version > current_version() {
        CheckResult::Available(info)
    } else {
        CheckResult::UpToDate
    }
}

/// Download progress shared with the UI thread.
#[derive(Debug, Default, Clone, Copy)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// Stream the release file to `dest`, reporting progress. The whole
/// file is never held in memory (streaming also gives a real
/// progress bar). At most `max_bytes` are written: a declared
/// Content-Length above the cap is rejected before any byte is
/// written, and a body that grows past the cap aborts mid-stream.
/// On any failure the partial file is removed, so a failed
/// download can never be mistaken for a complete one.
pub async fn download_asset(
    url: &str,
    dest: &Path,
    progress: Arc<Mutex<DownloadProgress>>,
    max_bytes: u64,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("Nicord/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("không tạo được HTTP client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("không tải được bản mới: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("tải bản mới thất bại: HTTP {}", resp.status()));
    }
    if let Some(len) = resp.content_length() {
        if len > max_bytes {
            return Err(format!(
                "file tải về khai báo {len} byte, vượt quá giới hạn {max_bytes} byte"
            ));
        }
    }
    if let Ok(mut p) = progress.lock() {
        p.total = resp.content_length();
    }
    let mut file =
        std::fs::File::create(dest).map_err(|e| format!("không ghi được file tạm: {e}"))?;
    let mut downloaded: u64 = 0;
    let mut stream = resp.bytes_stream();
    let mut result = Ok(());
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                result = Err(format!("tải bản mới thất bại: {e}"));
                break;
            }
        };
        downloaded += chunk.len() as u64;
        if downloaded > max_bytes {
            result = Err(format!(
                "file tải về vượt quá giới hạn {max_bytes} byte; dừng tải"
            ));
            break;
        }
        if let Err(e) = file.write_all(&chunk) {
            result = Err(format!("không ghi được file tạm: {e}"));
            break;
        }
        if let Ok(mut p) = progress.lock() {
            p.downloaded = downloaded;
        }
    }
    // The handle must be closed before removing the file: Windows
    // refuses to delete an open file.
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// Parse a `SHA256SUMS.txt` body into (filename, lowercase hex)
/// pairs. Accepts the conventional two-space separator and the
/// GNU `*` binary-mode marker, and tolerates CRLF. A malformed
/// line fails the whole file: checksums are a security control,
/// not a best-effort hint.
pub fn parse_checksums(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let hash = parts.next().unwrap_or("");
        let name = parts
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('*')
            .trim();
        let hash_ok = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        if !hash_ok || name.is_empty() {
            return Err(format!(
                "file checksum: dòng {} không đúng định dạng",
                i + 1
            ));
        }
        out.push((name.to_string(), hash.to_lowercase()));
    }
    if out.is_empty() {
        return Err("file checksum rỗng".to_string());
    }
    Ok(out)
}

/// SHA-256 hex digest of a file, streamed in 64 KiB chunks.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("không mở được file để kiểm tra: {e}"))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("không đọc được file để kiểm tra: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>())
}

/// Verify a downloaded release file against the release's
/// checksums text. Fails when the file has no entry or the digest
/// differs — a mismatch means the download is corrupt or was
/// replaced, and it must never be extracted or executed.
pub fn verify_download(
    zip_path: &Path,
    checksums_text: &str,
    asset_name: &str,
) -> Result<(), String> {
    let sums = parse_checksums(checksums_text)?;
    let expected = sums
        .iter()
        .find(|(name, _)| name == asset_name)
        .map(|(_, hash)| hash.clone())
        .ok_or_else(|| format!("file checksum không có mã băm cho {asset_name}"))?;
    let actual = sha256_file(zip_path)?;
    if actual != expected {
        return Err(format!(
            "mã băm SHA-256 của {asset_name} không khớp với file checksum — file tải về có thể đã hỏng hoặc bị thay đổi; từ chối cài đặt"
        ));
    }
    Ok(())
}

/// Download the release's checksum file and verify `zip_path`
/// against it. A release without a checksum asset fails closed:
/// unverified code is never installed.
pub async fn download_and_verify_checksums(
    info: &ReleaseInfo,
    zip_path: &Path,
    stage_dir: &Path,
) -> Result<(), String> {
    let url = info.checksums_url.as_ref().ok_or_else(|| {
        "bản phát hành không có file checksum (SHA256SUMS.txt); từ chối cài đặt".to_string()
    })?;
    let sums_path = stage_dir.join(CHECKSUMS_ASSET_NAME);
    let progress = Arc::new(Mutex::new(DownloadProgress::default()));
    download_asset(url, &sums_path, progress, MAX_CHECKSUMS_BYTES).await?;
    let text = std::fs::read_to_string(&sums_path)
        .map_err(|e| format!("không đọc được file checksum: {e}"))?;
    verify_download(zip_path, &text, &info.asset.name)
}

/// Zip-bomb guards for release extraction. A real Nicord release is a
/// few MB with a handful of files; these caps are far above that.
pub const MAX_ZIP_FILES: usize = 1_000;
pub const MAX_ZIP_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_ZIP_TOTAL_BYTES: u64 = 200 * 1024 * 1024;

/// Extraction limits (see the `MAX_ZIP_*` constants for production
/// values). Tests pass small limits to exercise the guards without
/// building huge zips.
#[derive(Debug, Clone, Copy)]
pub struct ZipLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for ZipLimits {
    fn default() -> Self {
        Self {
            max_files: MAX_ZIP_FILES,
            max_file_bytes: MAX_ZIP_FILE_BYTES,
            max_total_bytes: MAX_ZIP_TOTAL_BYTES,
        }
    }
}

/// Extract a zip into `dest`, returning the extracted file paths.
/// `ZipFile::enclosed_name` rejects absolute paths and `..` entries
/// (Zip Slip), so a hostile zip cannot write outside `dest`.
pub fn extract_zip(zip_path: &Path, dest: &Path) -> Result<Vec<PathBuf>, String> {
    extract_zip_with_limits(zip_path, dest, ZipLimits::default())
}

pub fn extract_zip_with_limits(
    zip_path: &Path,
    dest: &Path,
    limits: ZipLimits,
) -> Result<Vec<PathBuf>, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("không mở được file zip: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("file zip không hợp lệ: {e}"))?;
    if archive.len() > limits.max_files {
        return Err("file zip chứa quá nhiều file".into());
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("không tạo được thư mục tạm: {e}"))?;
    let mut out = Vec::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("không đọc được file zip: {e}"))?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(format!(
                "file zip chứa đường dẫn không an toàn: {}",
                entry.name()
            ));
        };
        let target = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("không tạo được thư mục tạm: {e}"))?;
            continue;
        }
        // Reject an oversized declared size early; the `take` below is
        // the enforced check (a declared size could lie).
        if entry.size() > limits.max_file_bytes {
            return Err("file zip chứa file quá lớn".into());
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("không tạo được thư mục tạm: {e}"))?;
        }
        let mut f =
            std::fs::File::create(&target).map_err(|e| format!("không giải nén được: {e}"))?;
        let mut limited = (&mut entry).take(limits.max_file_bytes + 1);
        let copied =
            std::io::copy(&mut limited, &mut f).map_err(|e| format!("không giải nén được: {e}"))?;
        if copied > limits.max_file_bytes {
            return Err("file zip chứa file quá lớn".into());
        }
        total += copied;
        if total > limits.max_total_bytes {
            return Err("file zip giải nén vượt quá giới hạn cho phép".into());
        }
        out.push(target);
    }
    Ok(out)
}

/// The staged executable inside an extracted release: the shipped
/// `Nicord.exe` first, then any `.exe`, then (non-Windows) a file
/// named `nicord`. Nothing found → the release has no runnable file.
pub fn find_staged_exe(files: &[PathBuf]) -> Option<PathBuf> {
    let by_name = |n: &str| {
        files.iter().find(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().eq_ignore_ascii_case(n))
                .unwrap_or(false)
        })
    };
    if let Some(p) = by_name("Nicord.exe") {
        return Some(p.clone());
    }
    if let Some(p) = files.iter().find(|p| {
        p.extension()
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
    }) {
        return Some(p.clone());
    }
    #[cfg(not(target_os = "windows"))]
    if let Some(p) = by_name("nicord") {
        return Some(p.clone());
    }
    None
}

/// Everything the install step needs, resolved before the app quits.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    /// Temp dir holding the extracted release + helper script.
    pub stage_dir: PathBuf,
    /// File name of the staged executable inside `stage_dir`.
    pub staged_exe_name: String,
    /// Folder holding the running exe (the release zip's files land here).
    pub install_dir: PathBuf,
    /// Full path of the currently running exe.
    pub exe_path: PathBuf,
}

/// Extract the downloaded zip into a fresh temp staging dir and
/// resolve the install locations. Nothing in the install folder is
/// touched yet — the swap happens later, in the helper script.
pub fn prepare_install(zip_path: &Path) -> Result<InstallPlan, String> {
    let exe_path =
        std::env::current_exe().map_err(|e| format!("không tìm thấy đường dẫn exe: {e}"))?;
    let install_dir = exe_path
        .parent()
        .ok_or("không tìm thấy thư mục cài đặt")?
        .to_path_buf();
    let stage_dir = std::env::temp_dir().join(format!("nicord-update-{}", uuid::Uuid::new_v4()));
    let files = extract_zip(zip_path, &stage_dir)?;
    let staged_exe = find_staged_exe(&files).ok_or("không tìm thấy file thực thi trong bản mới")?;
    let staged_exe_name = staged_exe
        .file_name()
        .ok_or("không tìm thấy file thực thi trong bản mới")?
        .to_string_lossy()
        .into_owned();
    Ok(InstallPlan {
        stage_dir,
        staged_exe_name,
        install_dir,
        exe_path,
    })
}

/// Single-quote a value for POSIX sh: wrap it in `'...'`, escaping
/// an embedded quote as `'\''`. The result can be interpolated
/// into a script safely — `$`, backticks, and newlines inside
/// single quotes are literal.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A value baked into the Windows `.bat` helper must not contain
/// characters `cmd.exe` would interpret: `"` breaks out of the
/// quoted `set`, `%` expands environment variables, and CR/LF ends
/// the line (command injection via the install path). There is no
/// reliable escaping for these in batch files, so the update is
/// refused instead. Compiled on Windows (where the script needs
/// it) and in tests (where its behaviour is verified on every
/// platform); the `.sh` script needs no validation because
/// single-quote escaping handles every character.
#[cfg(any(target_os = "windows", test))]
fn validate_bat_value(value: &str) -> Result<(), String> {
    if value.chars().any(|c| matches!(c, '"' | '%' | '\r' | '\n')) {
        return Err(
            "đường dẫn cài đặt chứa ký tự không an toàn cho script cập nhật (\" % hoặc xuống dòng)"
                .to_string(),
        );
    }
    Ok(())
}

/// The helper script the installer runs after Nicord exits. It waits
/// for Nicord's PID to disappear, backs the old exe up to
/// `<exe>.bak`, copies the staged files over the install folder,
/// deletes the staging dir, relaunches Nicord and deletes itself.
/// Paths are baked in quoted, so folders with spaces work — and
/// escaped/validated, so a hostile path cannot inject commands
/// (see `sh_quote` / `validate_bat_value`).
pub fn helper_script_text(plan: &InstallPlan) -> Result<String, String> {
    let stage = plan.stage_dir.display().to_string();
    let dir = plan.install_dir.display().to_string();
    let exe = plan.exe_path.display().to_string();
    let staged = plan.staged_exe_name.clone();
    #[cfg(target_os = "windows")]
    {
        for value in [&stage, &dir, &exe, &staged] {
            validate_bat_value(value)?;
        }
        Ok(format!(
            "@echo off\r\n\
             set \"PID=%1\"\r\n\
             set \"STAGE={stage}\"\r\n\
             set \"DIR={dir}\"\r\n\
             set \"EXE={exe}\"\r\n\
             :waitloop\r\n\
             tasklist /FI \"PID eq %PID%\" 2>nul | find /I \"%PID%\" >nul\r\n\
             if not errorlevel 1 (\r\n\
             \x20 timeout /t 1 /nobreak >nul\r\n\
             \x20 goto waitloop\r\n\
             )\r\n\
             if exist \"%EXE%.bak\" del /F /Q \"%EXE%.bak\"\r\n\
             copy /Y \"%EXE%\" \"%EXE%.bak\" >nul\r\n\
             copy /Y \"%STAGE%\\{staged}\" \"%EXE%\" >nul\r\n\
             copy /Y \"%STAGE%\\README.txt\" \"%DIR%\\README.txt\" >nul\r\n\
             copy /Y \"%STAGE%\\LICENSE\" \"%DIR%\\LICENSE\" >nul\r\n\
             copy /Y \"%STAGE%\\icon.ico\" \"%DIR%\\icon.ico\" >nul\r\n\
             rmdir /S /Q \"%STAGE%\"\r\n\
             start \"\" \"%EXE%\"\r\n\
             del \"%~f0\"\r\n"
        ))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let stage = sh_quote(&stage);
        let dir = sh_quote(&dir);
        let exe = sh_quote(&exe);
        let staged = sh_quote(&staged);
        Ok(format!(
            "#!/bin/sh\n\
             PID=\"$1\"\n\
             STAGE={stage}\n\
             DIR={dir}\n\
             EXE={exe}\n\
             STAGED={staged}\n\
             while kill -0 \"$PID\" 2>/dev/null; do sleep 1; done\n\
             cp -f \"$EXE\" \"$EXE.bak\"\n\
             cp -f \"$STAGE/$STAGED\" \"$EXE\"\n\
             cp -f \"$STAGE/README.txt\" \"$DIR/README.txt\"\n\
             cp -f \"$STAGE/LICENSE\" \"$DIR/LICENSE\"\n\
             cp -f \"$STAGE/icon.ico\" \"$DIR/icon.ico\"\n\
             rm -rf \"$STAGE\"\n\
             nohup \"$EXE\" >/dev/null 2>&1 &\n\
             rm -- \"$0\"\n"
        ))
    }
}

/// Write the helper script into the staging dir and return its path.
pub fn write_helper(plan: &InstallPlan) -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let name = "update.bat";
    #[cfg(not(target_os = "windows"))]
    let name = "update.sh";
    let path = plan.stage_dir.join(name);
    std::fs::write(&path, helper_script_text(plan)?)
        .map_err(|e| format!("không ghi được script cập nhật: {e}"))?;
    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(path)
}

/// Launch the helper detached (it survives Nicord exiting) and quit.
/// The helper does the swap + relaunch; this process must be gone
/// before the swap, which is why the helper waits on our PID.
pub fn launch_helper_and_exit(helper: &Path) -> ! {
    let pid = std::process::id().to_string();
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x8) + CREATE_NO_WINDOW (0x8000000): no
        // console window flashes, and the helper outlives this process.
        let _ = std::process::Command::new(helper)
            .arg(&pid)
            .creation_flags(0x0000_0008 | 0x0800_0000)
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::process::CommandExt;
        let _ = std::process::Command::new("sh")
            .arg(helper)
            .arg(&pid)
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    std::process::exit(0);
}

/// Human-readable byte count for the download progress line.
pub fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// UI state for the updater, shared between the background threads
/// and the egui frame (one Mutex, read once per frame).
#[derive(Debug, Clone, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    Available(ReleaseInfo),
    UpToDate,
    NoRelease,
    Downloading,
    ReadyToInstall,
    Failed(String),
}

#[derive(Debug)]
pub struct UpdateStatus {
    pub state: UpdateState,
    /// Whether the update dialog is currently open.
    pub dialog: bool,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Resolved by `prepare_install` once the download is extracted.
    pub plan: Option<InstallPlan>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            state: UpdateState::Idle,
            dialog: false,
            downloaded: 0,
            total: None,
            plan: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn release_json(tag: &str, assets: &[(&str, &str, u64)]) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "assets": assets.iter().map(|(name, url, size)| {
                serde_json::json!({"name": name, "browser_download_url": url, "size": size})
            }).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn version_parses_v_prefix_and_plain() {
        assert_eq!(
            Version::parse("v1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            Version::parse("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            Version::parse("0.1.0"),
            Some(Version {
                major: 0,
                minor: 1,
                patch: 0
            })
        );
    }

    #[test]
    fn version_rejects_malformed_tags() {
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("v1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("abc"), None);
        assert_eq!(Version::parse("v1.x.3"), None);
        assert_eq!(Version::parse("v 1.2.3"), None);
    }

    #[test]
    fn version_compare_decides_update() {
        let cur = Version {
            major: 1,
            minor: 2,
            patch: 3,
        };
        // Equal → no update.
        assert!(
            !(Version {
                major: 1,
                minor: 2,
                patch: 3
            } > cur)
        );
        // Newer patch/minor/major → update.
        assert!(
            Version {
                major: 1,
                minor: 2,
                patch: 4
            } > cur
        );
        assert!(
            Version {
                major: 1,
                minor: 3,
                patch: 0
            } > cur
        );
        assert!(
            Version {
                major: 2,
                minor: 0,
                patch: 0
            } > cur
        );
        // Older → no update.
        assert!(
            !(Version {
                major: 1,
                minor: 2,
                patch: 2
            } > cur)
        );
        assert!(
            !(Version {
                major: 0,
                minor: 9,
                patch: 9
            } > cur)
        );
    }

    #[test]
    fn asset_pick_prefers_exact_name_then_platform_zip() {
        let v = Version {
            major: 0,
            minor: 1,
            patch: 1,
        };
        let expected = expected_asset_name(&v);
        let assets = vec![
            ReleaseAsset {
                name: "something-else.zip".into(),
                url: "u0".into(),
                size: None,
            },
            ReleaseAsset {
                name: expected.clone(),
                url: "u1".into(),
                size: Some(10),
            },
        ];
        assert_eq!(pick_asset(&assets, &v).unwrap().url, "u1");
        // No exact name → first .zip for this platform.
        let assets = vec![ReleaseAsset {
            name: if cfg!(target_os = "windows") {
                "Nicord-v0.1.1-windows-x64.zip".into()
            } else {
                "Nicord-v0.1.1-linux-x64.zip".into()
            },
            url: "u2".into(),
            size: None,
        }];
        assert_eq!(pick_asset(&assets, &v).unwrap().url, "u2");
        // Wrong-platform zip only → no asset.
        let assets = vec![ReleaseAsset {
            name: if cfg!(target_os = "windows") {
                "Nicord-v0.1.1-linux-x64.zip".into()
            } else {
                "Nicord-v0.1.1-windows-x64.zip".into()
            },
            url: "u3".into(),
            size: None,
        }];
        assert!(pick_asset(&assets, &v).is_none());
        // No zip at all → no asset.
        assert!(pick_asset(
            &[ReleaseAsset {
                name: "notes.txt".into(),
                url: "u4".into(),
                size: None
            }],
            &v
        )
        .is_none());
    }

    #[test]
    fn parse_release_accepts_good_payload() {
        let v = Version {
            major: 0,
            minor: 1,
            patch: 1,
        };
        let json = release_json(
            "v0.1.1",
            &[(
                &expected_asset_name(&v),
                "https://example.com/Nicord-v0.1.1-windows-x64.zip",
                4_551_454,
            )],
        );
        let info = parse_release(&json).expect("release must parse");
        assert_eq!(info.tag, "v0.1.1");
        assert_eq!(info.version, v);
        assert_eq!(info.asset.size, Some(4_551_454));
    }

    #[test]
    fn parse_release_rejects_bad_tag_and_missing_asset() {
        assert!(parse_release(&release_json("not-a-version", &[])).is_none());
        assert!(parse_release(&release_json(
            "v0.1.1",
            &[("notes.txt", "https://example.com/notes.txt", 1)]
        ))
        .is_none());
        assert!(parse_release(&serde_json::json!({"tag_name": "v0.1.1"})).is_none());
    }

    #[test]
    fn parse_release_finds_checksums_asset() {
        let v = Version {
            major: 0,
            minor: 1,
            patch: 1,
        };
        let expected = expected_asset_name(&v);
        let with_sums = release_json(
            "v0.1.1",
            &[
                (
                    &expected,
                    "https://example.com/Nicord-v0.1.1-windows-x64.zip",
                    4_551_454,
                ),
                (
                    CHECKSUMS_ASSET_NAME,
                    "https://example.com/SHA256SUMS.txt",
                    128,
                ),
            ],
        );
        let info = parse_release(&with_sums).expect("release must parse");
        assert_eq!(
            info.checksums_url.as_deref(),
            Some("https://example.com/SHA256SUMS.txt")
        );
        let without_sums = release_json(
            "v0.1.1",
            &[(
                &expected,
                "https://example.com/Nicord-v0.1.1-windows-x64.zip",
                4_551_454,
            )],
        );
        let info = parse_release(&without_sums).expect("release must parse");
        assert!(info.checksums_url.is_none());
    }

    #[test]
    fn checksums_parse_conventional_and_gnu_forms() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let text = format!("{hash}  file.zip\n{hash} *other.zip\r\n");
        let sums = parse_checksums(&text).unwrap();
        assert_eq!(sums.len(), 2);
        assert_eq!(sums[0], ("file.zip".to_string(), hash.to_string()));
        assert_eq!(sums[1], ("other.zip".to_string(), hash.to_string()));
        // Uppercase hex is normalized to lowercase.
        let sums = parse_checksums(&format!("{}  f.zip", hash.to_uppercase())).unwrap();
        assert_eq!(sums[0].1, hash);
        assert!(parse_checksums("").is_err());
        assert!(parse_checksums("not-a-hash  f.zip").is_err());
        assert!(parse_checksums(hash).is_err());
        assert!(parse_checksums(&format!("{hash}  ok.zip\nbroken line")).is_err());
    }

    #[test]
    fn sha256_and_verify_download_match_and_mismatch() {
        let dir = std::env::temp_dir().join(format!("nicord-hash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc.bin");
        std::fs::write(&file, b"abc").unwrap();
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_file(&file).unwrap(), hash);
        assert!(verify_download(&file, &format!("{hash}  abc.bin"), "abc.bin").is_ok());
        let wrong = "00".to_string() + &hash[2..];
        assert!(verify_download(&file, &format!("{wrong}  abc.bin"), "abc.bin").is_err());
        assert!(verify_download(&file, &format!("{hash}  other.bin"), "abc.bin").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sh_quote_neutralizes_shell_metacharacters() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(sh_quote("a`id`b"), "'a`id`b'");
        assert_eq!(sh_quote("a\nb"), "'a\nb'");
    }

    #[test]
    fn bat_values_reject_injection_characters() {
        assert!(validate_bat_value(r"C:\Program Files\Nicord").is_ok());
        assert!(validate_bat_value("Nicord.exe").is_ok());
        assert!(validate_bat_value("a\"b").is_err());
        assert!(validate_bat_value("a%b").is_err());
        assert!(validate_bat_value("a\nb").is_err());
        assert!(validate_bat_value("a\rb").is_err());
    }

    /// Serve one fixed HTTP response on 127.0.0.1 and return its URL.
    /// The server thread reads (and ignores) the request, writes the
    /// response, and exits. Tests download caps without network.
    fn serve_once(response: Vec<u8>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/file.zip")
    }

    fn cap_test_dest() -> PathBuf {
        std::env::temp_dir().join(format!("nicord-cap-test-{}.zip", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn download_asset_rejects_oversized_declared_length() {
        let mut response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(&[b'x'; 100]);
        let url = serve_once(response);
        let dest = cap_test_dest();
        let progress = Arc::new(Mutex::new(DownloadProgress::default()));
        let result = download_asset(&url, &dest, progress, 1024).await;
        assert!(result.is_err());
        assert!(
            !dest.exists(),
            "no partial file when the declared size is rejected"
        );
    }

    #[tokio::test]
    async fn download_asset_aborts_when_body_exceeds_cap() {
        let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(&[b'x'; 4096]);
        let url = serve_once(response);
        let dest = cap_test_dest();
        let progress = Arc::new(Mutex::new(DownloadProgress::default()));
        let result = download_asset(&url, &dest, progress, 1024).await;
        assert!(result.is_err());
        assert!(
            !dest.exists(),
            "partial file removed when the body exceeds the cap"
        );
    }

    #[tokio::test]
    async fn download_asset_succeeds_under_cap() {
        let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(&[b'x'; 100]);
        let url = serve_once(response);
        let dest = cap_test_dest();
        let progress = Arc::new(Mutex::new(DownloadProgress::default()));
        let result = download_asset(&url, &dest, progress, 1024).await;
        assert!(result.is_ok());
        assert_eq!(std::fs::read(&dest).unwrap().len(), 100);
        let _ = std::fs::remove_file(&dest);
    }

    /// Build a tiny zip in memory: Nicord.exe + README.txt + a
    /// subfolder file, using the same crate the installer uses.
    fn sample_zip_bytes() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("Nicord.exe", opts).unwrap();
            w.write_all(b"fake-exe-bytes").unwrap();
            w.start_file("README.txt", opts).unwrap();
            w.write_all(b"readme").unwrap();
            w.add_directory("docs/", opts).unwrap();
            w.start_file("docs/note.txt", opts).unwrap();
            w.write_all(b"note").unwrap();
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extract_zip_roundtrips_files() {
        let dir = std::env::temp_dir().join(format!("nicord-test-{}", uuid::Uuid::new_v4()));
        let zip_path = dir.join("r.zip");
        let dest = dir.join("out");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&zip_path, sample_zip_bytes()).unwrap();
        let files = extract_zip(&zip_path, &dest).unwrap();
        assert_eq!(
            std::fs::read(dest.join("Nicord.exe")).unwrap(),
            b"fake-exe-bytes"
        );
        assert_eq!(std::fs::read(dest.join("README.txt")).unwrap(), b"readme");
        assert_eq!(std::fs::read(dest.join("docs/note.txt")).unwrap(), b"note");
        assert!(files.iter().any(|p| p.ends_with("Nicord.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_zip_rejects_zip_slip() {
        let dir = std::env::temp_dir().join(format!("nicord-test-{}", uuid::Uuid::new_v4()));
        let zip_path = dir.join("evil.zip");
        let dest = dir.join("out");
        std::fs::create_dir_all(&dir).unwrap();
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("../evil.txt", opts).unwrap();
            w.write_all(b"evil").unwrap();
            w.finish().unwrap();
        }
        std::fs::write(&zip_path, buf.into_inner()).unwrap();
        let err = extract_zip(&zip_path, &dest).unwrap_err();
        assert!(err.contains("không an toàn"), "unexpected error: {err}");
        assert!(!dir.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_zip_with_limits_rejects_too_many_files() {
        let dir = std::env::temp_dir().join(format!("nicord-test-{}", uuid::Uuid::new_v4()));
        let zip_path = dir.join("many.zip");
        let dest = dir.join("out");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&zip_path, sample_zip_bytes()).unwrap();
        // Sample zip has 4 entries (3 files + 1 directory).
        let limits = ZipLimits {
            max_files: 2,
            ..ZipLimits::default()
        };
        let err = extract_zip_with_limits(&zip_path, &dest, limits).unwrap_err();
        assert!(err.contains("quá nhiều file"), "unexpected error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_zip_with_limits_rejects_oversize_total() {
        let dir = std::env::temp_dir().join(format!("nicord-test-{}", uuid::Uuid::new_v4()));
        let zip_path = dir.join("big.zip");
        let dest = dir.join("out");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&zip_path, sample_zip_bytes()).unwrap();
        let limits = ZipLimits {
            max_total_bytes: 5,
            ..ZipLimits::default()
        };
        let err = extract_zip_with_limits(&zip_path, &dest, limits).unwrap_err();
        assert!(err.contains("vượt quá giới hạn"), "unexpected error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_staged_exe_prefers_nicord_exe() {
        let files = vec![
            PathBuf::from("/tmp/x/README.txt"),
            PathBuf::from("/tmp/x/other.exe"),
            PathBuf::from("/tmp/x/Nicord.exe"),
        ];
        assert_eq!(
            find_staged_exe(&files).unwrap(),
            PathBuf::from("/tmp/x/Nicord.exe")
        );
        let files = vec![PathBuf::from("/tmp/x/other.exe")];
        assert_eq!(
            find_staged_exe(&files).unwrap(),
            PathBuf::from("/tmp/x/other.exe")
        );
        assert!(find_staged_exe(&[PathBuf::from("/tmp/x/README.txt")]).is_none());
    }

    #[test]
    fn helper_script_contains_swap_backup_and_relaunch() {
        let plan = InstallPlan {
            stage_dir: PathBuf::from("/tmp/nicord-update-1"),
            staged_exe_name: "Nicord.exe".to_string(),
            install_dir: PathBuf::from("C:/Nicord"),
            exe_path: PathBuf::from("C:/Nicord/Nicord.exe"),
        };
        let script = helper_script_text(&plan).unwrap();
        // Waits for the old process, backs it up, swaps, relaunches.
        assert!(script.contains("Nicord.exe.bak") || script.contains(".bak"));
        assert!(script.contains("Nicord.exe"));
        assert!(script.contains("/tmp/nicord-update-1") || script.contains("nicord-update-1"));
        #[cfg(target_os = "windows")]
        assert!(script.contains("tasklist"));
        #[cfg(not(target_os = "windows"))]
        assert!(script.contains("kill -0"));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn helper_script_quotes_hostile_paths() {
        let plan = InstallPlan {
            stage_dir: PathBuf::from("/tmp/$(touch /tmp/pwned)"),
            staged_exe_name: "Nicord.exe".to_string(),
            install_dir: PathBuf::from("/home/u/`id`"),
            exe_path: PathBuf::from("/home/u/`id`/Nicord"),
        };
        let script = helper_script_text(&plan).unwrap();
        // The metacharacters survive only inside single quotes, where
        // the shell treats them as literal text.
        assert!(script.contains("STAGE='/tmp/$(touch /tmp/pwned)'"));
        assert!(script.contains("DIR='/home/u/`id`'"));
    }

    #[test]
    fn fmt_bytes_readable() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.0 KB");
        assert_eq!(fmt_bytes(4_551_454), "4.3 MB");
    }
}
