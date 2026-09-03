//! Self-update — check GitHub Releases for a newer tstr, and swap this binary
//! for it on request.
//!
//! Two entry points, deliberately separate:
//!
//! - [`spawn_check`] / [`take_nudge`] — the passive path. A background thread
//!   asks GitHub what the newest release is; at the end of a run we print a
//!   one-line nudge *if the answer already arrived*. It never blocks the run,
//!   never fails it, and never speaks up in CI.
//! - [`run`] — `tstr self-update`, which the user is deliberately waiting on,
//!   so it may block, and it reports its errors.
//!
//! The published macOS binaries are ad-hoc signed, not notarized, so a copy
//! downloaded through a browser carries `com.apple.quarantine` and Gatekeeper
//! refuses to run it until the user clears the attribute by hand. A file
//! fetched by this process is not quarantined — LaunchServices applies that,
//! not us — so updating in place also removes that piece of friction.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How often the passive check is allowed to touch the network. Between checks
/// the cached answer in the stamp file is compared instead, so a suite run in a
/// tight loop makes at most one request a day.
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Network budget for the passive check. Small on purpose: this is speculative
/// work nobody asked for, and a slow GitHub must never show up as a slow run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// Network budget for an explicit `tstr self-update` — the user is watching a
/// ~5 MB download, so this is a real timeout rather than a token one.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(180);

/// Set this to anything non-empty to silence the passive check entirely.
const OPT_OUT_ENV: &str = "TSTR_NO_UPDATE_CHECK";

// ---------------------------------------------------------------------------
// Where things live
// ---------------------------------------------------------------------------

/// `owner/repo`, taken from the `repository` field in Cargo.toml so the release
/// location is stated in exactly one place.
fn repo_slug() -> Option<String> {
    let url = option_env!("CARGO_PKG_REPOSITORY")?.trim_end_matches('/');
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?.trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{}/{}", owner, repo))
}

/// `~/.config/tstr/update-check.json` — same directory as the user-global
/// config, so tstr keeps all its per-user state in one place.
fn stamp_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/tstr/update-check.json"))
}

/// The release asset for the machine we are actually running on.
///
/// Only the targets the release workflow publishes are mapped; anything else
/// yields `None` and the caller degrades to "check manually". In particular
/// aarch64 Linux has no published build, so we would rather say nothing than
/// hand someone an x86_64 binary.
fn asset_name() -> Option<String> {
    match (std::env::consts::OS, host_arch()) {
        ("macos", arch) => Some(format!("tstr-{}-apple-darwin.tar.gz", arch)),
        ("linux", "x86_64") => Some("tstr-x86_64-unknown-linux-gnu.tar.gz".to_string()),
        _ => None,
    }
}

/// The architecture of the *hardware*, which is not always the one this binary
/// was built for: an x86_64 tstr on Apple Silicon runs under Rosetta and
/// reports `x86_64` from `std::env::consts::ARCH`. Such an install should be
/// updated to the native arm64 build, so ask the kernel what the machine is
/// rather than trusting the running process.
#[cfg(target_os = "macos")]
fn host_arch() -> &'static str {
    // `sysctl.proc_translated` is 1 when the calling process is being
    // translated by Rosetta, 0 when it is native, and absent on Intel Macs
    // (where the sysctl does not exist and the command fails) — all three
    // cases land on the right answer below.
    let translated = std::process::Command::new("sysctl")
        .args(["-n", "sysctl.proc_translated"])
        .output()
        .map(|o| o.stdout.starts_with(b"1"))
        .unwrap_or(false);

    if translated || std::env::consts::ARCH == "aarch64" {
        "aarch64"
    } else {
        "x86_64"
    }
}

#[cfg(not(target_os = "macos"))]
fn host_arch() -> &'static str {
    std::env::consts::ARCH
}

// ---------------------------------------------------------------------------
// Talking to GitHub
// ---------------------------------------------------------------------------

/// One release asset: the name the workflow gave it and where to fetch it.
struct Asset {
    name: String,
    url: String,
}

/// The newest published release: its version (tag minus the `v`) and assets.
fn latest_release(timeout: Duration) -> Result<(String, Vec<Asset>), String> {
    let slug = repo_slug().ok_or("no GitHub repository recorded in this build")?;
    let url = format!("https://api.github.com/repos/{}/releases/latest", slug);

    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        // GitHub rejects API requests without a User-Agent.
        .user_agent(format!("tstr/{}", crate::version::current()))
        .build()
        .map_err(|e| format!("could not build HTTP client: {}", e))?;

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| format!("could not reach GitHub: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("GitHub returned {}", resp.status()));
    }

    let body: serde_json::Value = resp
        .json()
        .map_err(|e| format!("could not parse GitHub's response: {}", e))?;

    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("GitHub's response had no tag_name")?;
    let version = tag.trim_start_matches('v').to_string();

    let assets = body
        .get("assets")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("browser_download_url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok((version, assets))
}

// ---------------------------------------------------------------------------
// The passive check
// ---------------------------------------------------------------------------

/// What the stamp file remembers between runs.
struct Stamp {
    checked_at: u64,
    latest_seen: String,
}

fn read_stamp() -> Option<Stamp> {
    let text = std::fs::read_to_string(stamp_path()?).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(Stamp {
        checked_at: json.get("checked_at")?.as_u64()?,
        latest_seen: json.get("latest_seen")?.as_str()?.to_string(),
    })
}

/// Best-effort: a stamp we cannot write just means we check again next run.
fn write_stamp(latest: &str) {
    let Some(path) = stamp_path() else { return };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let body = serde_json::json!({ "checked_at": now_secs(), "latest_seen": latest });
    let _ = std::fs::write(path, body.to_string());
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Start the passive check on a background thread.
///
/// Returns a receiver that will *eventually* carry the newest version string,
/// or nothing at all if the check was suppressed or failed. The caller is not
/// expected to wait on it — see [`take_nudge`].
pub fn spawn_check() -> Receiver<String> {
    let (tx, rx) = mpsc::channel();

    // Suppressed: opted out, or output isn't a terminal. The second case is the
    // important one — a nudge in CI logs is noise nobody can act on.
    if std::env::var_os(OPT_OUT_ENV).is_some_and(|v| !v.is_empty()) || !atty::is(atty::Stream::Stderr)
    {
        return rx;
    }

    // Inside the interval, answer from the stamp without touching the network.
    if let Some(stamp) = read_stamp() {
        if now_secs().saturating_sub(stamp.checked_at) < CHECK_INTERVAL_SECS {
            let _ = tx.send(stamp.latest_seen);
            return rx;
        }
    }

    std::thread::spawn(move || {
        if let Ok((latest, _)) = latest_release(CHECK_TIMEOUT) {
            write_stamp(&latest);
            // A closed receiver just means the run finished first; drop it.
            let _ = tx.send(latest);
        }
    });

    rx
}

/// The nudge to print, if the check finished in time and found something newer.
///
/// Uses a non-blocking read, so a check still in flight is simply skipped —
/// the answer will be cached for the next run.
pub fn take_nudge(rx: &Receiver<String>) -> Option<String> {
    let latest = rx.try_recv().ok()?;
    if !crate::version::is_newer_than_current(&latest) {
        return None;
    }
    Some(format!(
        "tstr {} available (you have {}) — run: tstr self-update",
        latest,
        crate::version::current()
    ))
}

// ---------------------------------------------------------------------------
// `tstr self-update`
// ---------------------------------------------------------------------------

/// Replace the running binary with the newest published build.
///
/// Returns a human-readable summary on success, or an error to print. Nothing
/// here touches the filesystem until the download has succeeded and been
/// unpacked, so a failed update leaves the existing binary alone.
pub fn run(check_only: bool) -> Result<String, String> {
    let exe = current_binary()?;
    let (latest, assets) = latest_release(UPDATE_TIMEOUT)?;
    let current = crate::version::current();

    // Reporting is harmless from any build, so the dev-build guard below only
    // gates the part that writes.
    if !crate::version::is_newer_than_current(&latest) {
        write_stamp(&latest);
        return Ok(format!("already up to date (tstr {})", current));
    }
    if check_only {
        write_stamp(&latest);
        return Ok(format!("tstr {} available (you have {})", latest, current));
    }

    // A binary inside a Cargo output directory belongs to the build, not to us:
    // overwriting it would be silently undone by the next `cargo build`, and
    // the user would be left unsure which tstr they are running.
    if is_cargo_artifact(&exe) {
        return Err(format!(
            "{} is a local build, not an installed release.\n\
             tstr {} is out, update it with: git pull && cargo build --release",
            exe.display(),
            latest
        ));
    }

    let wanted = asset_name().ok_or_else(|| {
        format!(
            "no published build for {}-{}; download one from \
             https://github.com/{}/releases/latest",
            std::env::consts::OS,
            host_arch(),
            repo_slug().unwrap_or_else(|| "the project".into())
        )
    })?;
    let asset = assets
        .iter()
        .find(|a| a.name == wanted)
        .ok_or_else(|| format!("release {} has no asset named {}", latest, wanted))?;

    // Stage in the install directory so the final step is a rename within one
    // filesystem — an atomic swap rather than a copy that can be interrupted
    // halfway and leave a truncated binary on PATH.
    let dir = exe
        .parent()
        .ok_or_else(|| format!("cannot determine the directory of {}", exe.display()))?;
    let staging = dir.join(format!(".tstr-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir(&staging).map_err(|e| write_hint(&e, dir))?;

    let result = install(&staging, asset, &exe);
    let _ = std::fs::remove_dir_all(&staging);
    result?;

    write_stamp(&latest);
    Ok(format!(
        "updated {} from {} to {}",
        exe.display(),
        current,
        latest
    ))
}

/// Download, unpack, and swap. Split out so the caller can clean up the staging
/// directory on every path.
fn install(staging: &Path, asset: &Asset, exe: &Path) -> Result<(), String> {
    println!("downloading {} …", asset.name);

    let client = reqwest::blocking::Client::builder()
        .timeout(UPDATE_TIMEOUT)
        .user_agent(format!("tstr/{}", crate::version::current()))
        .build()
        .map_err(|e| format!("could not build HTTP client: {}", e))?;
    let bytes = client
        .get(&asset.url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| format!("download failed: {}", e))?;

    let archive = staging.join(&asset.name);
    std::fs::write(&archive, &bytes).map_err(|e| format!("could not stage the download: {}", e))?;

    // `tar` is present on every macOS and Linux install, and the archive holds
    // a single file — not worth a gzip/tar dependency to keep current for it.
    let status = std::process::Command::new("tar")
        .arg("xzf")
        .arg(&archive)
        .arg("-C")
        .arg(staging)
        .status()
        .map_err(|e| format!("could not run tar: {}", e))?;
    if !status.success() {
        return Err("tar could not unpack the release archive".to_string());
    }

    let unpacked = staging.join("tstr");
    if !unpacked.is_file() {
        return Err("the release archive did not contain a tstr binary".to_string());
    }

    // Verify before installing: a binary that cannot report its own version is
    // one we should not be putting on someone's PATH.
    let reported = std::process::Command::new(&unpacked)
        .arg("--version")
        .output()
        .map_err(|e| format!("the downloaded binary would not run: {}", e))?;
    if !reported.status.success() {
        return Err("the downloaded binary would not run".to_string());
    }

    set_executable(&unpacked)?;
    std::fs::rename(&unpacked, exe).map_err(|e| write_hint(&e, exe))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("could not make the new binary executable: {}", e))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Turn a permission error into advice rather than an errno.
fn write_hint(e: &std::io::Error, target: &Path) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "no permission to write to {} — either re-run with sudo, or \
             reinstall somewhere you own such as ~/.local/bin",
            target.display()
        )
    } else {
        format!("could not write to {}: {}", target.display(), e)
    }
}

/// The real path of the running binary, with symlinks resolved so that an
/// install like `~/.local/bin/tstr -> ~/bin/tstr-0.12.2` replaces the binary
/// rather than turning the symlink into one.
fn current_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("could not determine the running binary: {}", e))?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Whether a path sits in a Cargo output directory (`…/target/debug/tstr`).
fn is_cargo_artifact(path: &Path) -> bool {
    let mut components = path.components().rev().skip(1);
    let profile = components.next();
    let target = components.next();
    matches!(
        (profile, target),
        (Some(p), Some(t))
            if (p.as_os_str() == "debug" || p.as_os_str() == "release")
                && t.as_os_str() == "target"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_slug_comes_from_cargo_metadata() {
        assert_eq!(repo_slug().as_deref(), Some("eightdeekay/tstr"));
    }

    #[test]
    fn cargo_artifacts_are_recognised_and_installs_are_not() {
        assert!(is_cargo_artifact(Path::new("/Users/x/dev/tstr/target/release/tstr")));
        assert!(is_cargo_artifact(Path::new("/Users/x/dev/tstr/target/debug/tstr")));
        assert!(!is_cargo_artifact(Path::new("/Users/x/.local/bin/tstr")));
        assert!(!is_cargo_artifact(Path::new("/usr/local/bin/tstr")));
        // A directory merely *named* release, without a target/ parent, is a
        // normal install location.
        assert!(!is_cargo_artifact(Path::new("/opt/release/tstr")));
    }

    #[test]
    fn asset_name_matches_the_published_archives() {
        // Whatever host the tests run on, the name must be one the release
        // workflow actually produces.
        let published = [
            "tstr-aarch64-apple-darwin.tar.gz",
            "tstr-x86_64-apple-darwin.tar.gz",
            "tstr-x86_64-unknown-linux-gnu.tar.gz",
        ];
        if let Some(name) = asset_name() {
            assert!(published.contains(&name.as_str()), "unpublished asset: {}", name);
        }
    }

    #[test]
    fn nudge_is_silent_when_the_latest_is_not_newer() {
        let (tx, rx) = mpsc::channel();
        tx.send(crate::version::current().to_string()).unwrap();
        assert_eq!(take_nudge(&rx), None);
    }

    #[test]
    fn nudge_fires_for_a_newer_release() {
        let (tx, rx) = mpsc::channel();
        tx.send("999.0.0".to_string()).unwrap();
        let nudge = take_nudge(&rx).expect("expected a nudge");
        assert!(nudge.contains("999.0.0"), "got: {}", nudge);
        assert!(nudge.contains("self-update"), "got: {}", nudge);
    }

    #[test]
    fn nudge_is_skipped_when_the_check_has_not_answered() {
        let (_tx, rx) = mpsc::channel::<String>();
        assert_eq!(take_nudge(&rx), None);
    }
}
