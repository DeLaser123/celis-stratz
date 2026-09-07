//! Self-update: keeps the installed `stratz` binary current.
//!
//! Two sources, in priority order:
//! 1. **Dev machine** — a source-repo marker (`~/.stratz/source.txt`) points at
//!    the repo; if `target/release/stratz(.exe)` there is newer than the
//!    running binary, it is swapped in (the developer just rebuilt).
//! 2. **Client machines** — GitHub Releases, checked at most once every 24h
//!    (timestamp in `~/.stratz/self-update.json`); a newer tag downloads the
//!    platform binary and swaps it in.
//!
//! The swap uses the Windows-safe rename dance (a running exe cannot be
//! overwritten, but it CAN be renamed) and then re-execs the new binary with
//! the original arguments — so "update before continuing" is literal.

use bt_core::error::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const REPO: &str = "DeLaser123/celis-stratz";
const GUARD_ENV: &str = "STRATZ_SELF_UPDATED";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

fn stratz_home() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".stratz"))
}

fn marker_path() -> Option<PathBuf> {
    stratz_home().map(|h| h.join("source.txt"))
}

fn settings_path() -> Option<PathBuf> {
    stratz_home().map(|h| h.join("self-update.json"))
}

fn platform_asset() -> String {
    format!(
        "stratz-{}-{}{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        exe_suffix()
    )
}

// ---------------------------------------------------------------------------
// settings (~/.stratz/self-update.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateSettings {
    /// Epoch seconds of the last GitHub release check.
    #[serde(default)]
    pub last_check_epoch: Option<u64>,
    /// User opt-out (`stratz self-update --disable`).
    #[serde(default)]
    pub disabled: Option<bool>,
}

fn load_settings() -> UpdateSettings {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_settings(s: &UpdateSettings) -> CoreResult<()> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(bt_core::CoreError::Io)?;
    }
    let text = serde_json::to_string_pretty(s)
        .map_err(|e| CoreError::InvalidData(format!("settings serialize: {e}")))?;
    std::fs::write(&path, text).map_err(bt_core::CoreError::Io)
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// source 1: local dev repo
// ---------------------------------------------------------------------------

/// If a source-repo marker exists and its built binary is newer than the
/// running one, returns that binary's path.
pub fn source_newer() -> CoreResult<Option<PathBuf>> {
    let Some(marker) = marker_path() else {
        return Ok(None);
    };
    let Some(repo) = std::fs::read_to_string(&marker)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    else {
        return Ok(None);
    };
    let src_exe = Path::new(&repo)
        .join("target")
        .join("release")
        .join(format!("stratz{}", exe_suffix()));
    if !src_exe.exists() {
        return Ok(None);
    }
    let cur = std::env::current_exe().map_err(bt_core::CoreError::Io)?;
    // Running the source binary directly: never self-swap.
    if same_file(&cur, &src_exe) {
        return Ok(None);
    }
    let src_mtime = file_mtime(&src_exe);
    let cur_mtime = file_mtime(&cur);
    if let (Some(s), Some(c)) = (src_mtime, cur_mtime) {
        if s > c {
            return Ok(Some(src_exe));
        }
    }
    Ok(None)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

// ---------------------------------------------------------------------------
// source 2: GitHub releases
// ---------------------------------------------------------------------------

fn fetch_latest_release() -> CoreResult<Option<(String, String)>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = ureq::get(&url)
        .set("User-Agent", "stratz")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => CoreError::InvalidData(
                "no published release yet (the release workflow has not produced binaries)".into(),
            ),
            other => CoreError::InvalidData(format!("GitHub release check failed: {other}")),
        })?;
    let body: serde_json::Value = serde_json::from_str(
        &resp
            .into_string()
            .map_err(|e| CoreError::InvalidData(format!("read: {e}")))?,
    )
    .map_err(|e| CoreError::InvalidData(format!("release JSON: {e}")))?;
    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::InvalidData("release response missing tag_name".into()))?;
    let assets = body
        .get("assets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let want = platform_asset();
    for asset in assets {
        let name = asset.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name == want {
            if let Some(url) = asset.get("browser_download_url").and_then(|v| v.as_str()) {
                return Ok(Some((
                    tag.trim_start_matches('v').to_string(),
                    url.to_string(),
                )));
            }
        }
    }
    Ok(None)
}

fn semver_gt(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<(u64, u64, u64)> {
        let mut it = s.trim().split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next().unwrap_or("0").parse().unwrap_or(0);
        let patch = it.next().unwrap_or("0").parse().unwrap_or(0);
        Some((major, minor, patch))
    };
    match (parse(candidate), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn download_to_temp(url: &str) -> CoreResult<PathBuf> {
    let dir = stratz_home()
        .unwrap_or_else(std::env::temp_dir)
        .join("downloads");
    std::fs::create_dir_all(&dir).map_err(bt_core::CoreError::Io)?;
    let dest = dir.join(format!("stratz-update{}", exe_suffix()));
    let resp = ureq::get(url)
        .set("User-Agent", "stratz")
        .timeout(Duration::from_secs(300))
        .call()
        .map_err(|e| CoreError::InvalidData(format!("download failed: {e}")))?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&dest).map_err(bt_core::CoreError::Io)?;
    std::io::copy(&mut reader, &mut file).map_err(bt_core::CoreError::Io)?;
    Ok(dest)
}

// ---------------------------------------------------------------------------
// the swap + re-exec
// ---------------------------------------------------------------------------

/// Swap the running binary with `new_binary` (Windows-safe rename dance).
pub fn swap_from(new_binary: &Path, label: &str) -> CoreResult<()> {
    let cur = std::env::current_exe().map_err(bt_core::CoreError::Io)?;
    let file_name = cur
        .file_name()
        .map(|n| n.to_os_string())
        .ok_or_else(|| CoreError::InvalidData("current exe has no file name".into()))?;
    let old = cur.with_file_name({
        let mut n = file_name.clone();
        n.push(".old");
        n
    });
    if old.exists() {
        let _ = std::fs::remove_file(&old);
    }
    // A running exe cannot be overwritten on Windows, but it can be renamed.
    std::fs::rename(&cur, &old).map_err(bt_core::CoreError::Io)?;
    std::fs::copy(new_binary, &cur).map_err(bt_core::CoreError::Io)?;
    let _ = std::fs::remove_file(new_binary);
    eprintln!("stratz: updated from {label} — continuing on the new version");
    Ok(())
}

/// Re-exec the (possibly just-updated) binary with the original arguments and
/// exit with its status. Runs the child with `GUARD_ENV` set so it never
/// loops back into an update.
fn reexec() -> CoreResult<()> {
    let cur = std::env::current_exe().map_err(bt_core::CoreError::Io)?;
    let status = std::process::Command::new(cur)
        .args(std::env::args_os().skip(1))
        .env(GUARD_ENV, "1")
        .status()
        .map_err(bt_core::CoreError::Io)?;
    std::process::exit(status.code().unwrap_or(0));
}

// ---------------------------------------------------------------------------
// precheck (called at the top of main on every run)
// ---------------------------------------------------------------------------

/// Best-effort, never-blocking update check. Silent on any failure — a broken
/// update path must never prevent the tool from running.
pub fn precheck() {
    if std::env::var_os(GUARD_ENV).is_some() {
        return;
    }
    // `stratz self-update` handles its own logic.
    if std::env::args().nth(1).as_deref() == Some("self-update") {
        return;
    }
    if let Err(e) = precheck_inner() {
        eprintln!("stratz: auto-update skipped ({e})");
    }
}

fn precheck_inner() -> CoreResult<()> {
    let settings = load_settings();
    if settings.disabled.unwrap_or(false) {
        return Ok(());
    }

    // 1) dev-machine source swap — cheap stat, always on.
    if let Some(src) = source_newer()? {
        swap_from(&src, "source build")?;
        reexec()?;
    }

    // 2) GitHub releases — rate-limited to once per day.
    let last = settings.last_check_epoch.unwrap_or(0);
    let elapsed = epoch_now().saturating_sub(last);
    if elapsed < CHECK_INTERVAL.as_secs() {
        return Ok(());
    }
    let mut s = settings;
    s.last_check_epoch = Some(epoch_now());
    let _ = save_settings(&s);

    if let Ok(Some((version, url))) = fetch_latest_release() {
        if semver_gt(&version, env!("CARGO_PKG_VERSION")) {
            let tmp = download_to_temp(&url)?;
            swap_from(&tmp, &format!("GitHub release v{version}"))?;
            reexec()?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `stratz self-update` subcommand
// ---------------------------------------------------------------------------

pub fn report_and_apply(force: bool, check_only: bool) -> CoreResult<()> {
    let mut changed = false;

    // Source build (dev machine).
    let src = source_newer()?;
    match &src {
        Some(p) => {
            if check_only {
                println!("update available: source build at {}", p.display());
            } else {
                swap_from(p, "source build")?;
                changed = true;
            }
        }
        None => println!("source build: up to date (no marker or not newer)"),
    }

    // GitHub release.
    let gh = fetch_latest_release();
    match gh {
        Ok(Some((version, url))) => {
            let current = env!("CARGO_PKG_VERSION");
            if semver_gt(&version, current) {
                println!("update available: v{version} (current v{current})");
                if !check_only {
                    let tmp = download_to_temp(&url)?;
                    swap_from(&tmp, &format!("GitHub release v{version}"))?;
                    changed = true;
                }
            } else if force {
                println!("re-downloading v{version} (--force)");
                if !check_only {
                    let tmp = download_to_temp(&url)?;
                    swap_from(&tmp, &format!("GitHub release v{version}"))?;
                    changed = true;
                }
            } else {
                println!("GitHub release: up to date (v{version})");
            }
        }
        Ok(None) => println!("GitHub: no published release yet"),
        Err(e) => println!("GitHub check failed: {e}"),
    }

    let mut s = load_settings();
    s.last_check_epoch = Some(epoch_now());
    let _ = save_settings(&s);

    if changed && !check_only {
        reexec()?;
    }
    Ok(())
}

pub fn set_disabled(disabled: bool) -> CoreResult<()> {
    let mut s = load_settings();
    s.disabled = Some(disabled);
    save_settings(&s)?;
    if disabled {
        println!("auto-update disabled (re-enable with `stratz self-update --enable`)");
    } else {
        println!("auto-update enabled");
    }
    Ok(())
}

pub fn set_source(repo: &Path) -> CoreResult<()> {
    let Some(marker) = marker_path() else {
        return Err(CoreError::InvalidData("no home directory".into()));
    };
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(bt_core::CoreError::Io)?;
    }
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    std::fs::write(&marker, canonical.display().to_string()).map_err(bt_core::CoreError::Io)?;
    println!("source marker set: {}", marker.display());
    println!("  repo: {}", canonical.display());
    Ok(())
}

pub fn status() -> CoreResult<()> {
    let s = load_settings();
    let disabled = s.disabled.unwrap_or(false);
    println!(
        "auto-update: {}",
        if disabled { "disabled" } else { "enabled" }
    );
    let last = s
        .last_check_epoch
        .and_then(|e| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(e)))
        .map(humantime_like)
        .unwrap_or_else(|| "never".into());
    println!("last GitHub check: {last}");
    match marker_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(repo) => println!("source marker: {repo}"),
        None => println!("source marker: not set (client install)"),
    }
    println!("platform asset: {}", platform_asset());
    Ok(())
}

fn humantime_like(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dt = chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or(chrono::Utc::now());
    dt.format("%Y-%m-%d %H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compare() {
        assert!(semver_gt("0.6.0", "0.5.0"));
        assert!(semver_gt("1.0.0", "0.9.9"));
        assert!(!semver_gt("0.5.0", "0.5.0"));
        assert!(!semver_gt("0.4.9", "0.5.0"));
        assert!(!semver_gt("garbage", "0.5.0"));
        assert!(semver_gt("0.5", "0.4.9"), "missing components default to 0");
    }

    #[test]
    fn platform_asset_naming() {
        let a = platform_asset();
        assert!(a.starts_with("stratz-"), "{a}");
        if cfg!(windows) {
            assert_eq!(a, "stratz-windows-x86_64.exe");
        }
    }

    #[test]
    fn settings_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bt_su_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // settings_path is driven by home; test the struct round-trip directly
        let s = UpdateSettings {
            last_check_epoch: Some(123),
            disabled: Some(true),
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: UpdateSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.last_check_epoch, Some(123));
        assert_eq!(back.disabled, Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
