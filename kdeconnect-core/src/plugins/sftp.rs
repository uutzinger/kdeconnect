use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

/// How long a single sshfs mount attempt may take end-to-end. sshfs itself
/// gives up connecting after `ConnectTimeout`; this is the backstop for a
/// hang anywhere else (DNS, auth exchange, FUSE setup).
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a health probe (`stat` on the mount point) may take before the
/// mount is declared stale. A live LAN sshfs answers in milliseconds; a dead
/// one blocks until its own timeouts fire, which is exactly what we can't
/// afford on the browse-click path.
const HEALTH_TIMEOUT_SECS: &str = "3";

/// Backstop timeout for the small auxiliary host commands (reading
/// /proc/mounts, health probe, unmount, rmdir). These are normally
/// instantaneous, but every one of them goes through `flatpak-spawn --host`
/// when sandboxed, and any of `flatpak-spawn`, the shell, or a syscall
/// against a wedged FUSE mount can hang. Since these run on the interactive
/// browse/list paths, a hang here freezes the UI — so we always bound them.
const AUX_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Overall budget for unmounting everything on shutdown. Individual unmounts
/// are already bounded by `AUX_CMD_TIMEOUT`; this caps the *total* so a
/// process with several wedged mounts still exits promptly and the phone
/// registers the disconnect without a long delay.
const SHUTDOWN_UNMOUNT_BUDGET: Duration = Duration::from_secs(15);

/// device_id → actual mount point of a mount this process created or
/// adopted. Purely a cache: the source of truth is the host's mount table,
/// which every user-facing query re-checks (the user can unmount from the
/// file manager at any time, behind our back — that's the point).
static MOUNTS: Lazy<StdMutex<HashMap<String, PathBuf>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

/// device_id → per-device lock serialising the mount/unmount lifecycle.
/// Without this a double-click on "Browse", or a second `kdeconnect.sftp`
/// packet arriving while the first is still mounting, could race two `sshfs`
/// processes onto the same mount point (or a mount against an unmount),
/// leaving a half-mounted, wedged entry. Every operation that touches a
/// device's mount point holds this for the whole critical section.
static OP_LOCKS: Lazy<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

/// Get (or create) the serialisation lock for one device.
fn op_lock(device_id: &str) -> Arc<AsyncMutex<()>> {
    OP_LOCKS
        .lock()
        .unwrap()
        .entry(device_id.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// `kdeconnect.sftp` — login info the phone sends in response to a
/// `kdeconnect.sftp.request { startBrowsing: true }`.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SftpInfo {
    pub ip: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub multi_paths: Option<Vec<String>>,
}

impl SftpInfo {
    /// Mount the phone's SFTP share with `sshfs` (FUSE) and open it in the
    /// file manager — the same mechanism the official kdeconnect-kde daemon
    /// uses. Every command runs via `host_command()`: inside a Flatpak
    /// sandbox a FUSE mount made in-sandbox lives in the sandbox's own
    /// mount namespace and is invisible to the host, so mounting/opening
    /// has to happen as real host processes via `flatpak-spawn --host`.
    /// Outside a sandbox this is just a plain `Command::new`.
    ///
    /// The mount point lives under `$HOME/KDE Connect/<device name>`, not
    /// XDG_RUNTIME_DIR: GIO's mount-display heuristic hides mounts whose
    /// path is under `/run` or contains a dot-directory, but shows mounts
    /// under the home directory — which is what puts the device in the file
    /// manager sidebar with its own unmount/eject button.
    pub async fn browse(&self, device_id: &str, device_name: &str) -> anyhow::Result<PathBuf> {
        preflight().await?;

        // `user` becomes the leading characters of the "user@host:path"
        // argument sshfs receives. A value starting with '-' would make
        // that whole argument look like an option instead of the host
        // string to sshfs's parser — reject it rather than hand a
        // phone-controlled string a chance to be read as an sshfs/ssh flag.
        if self.user.starts_with('-') {
            anyhow::bail!("rejecting sftp user starting with '-': {}", self.user);
        }

        // Serialise the whole mount lifecycle for this device so a second
        // browse (double-click, or a duplicate sftp packet) can't race a
        // second sshfs onto the same point.
        let lock = op_lock(device_id);
        let _guard = lock.lock().await;

        let mount_point = mount_point_for(device_id, device_name);
        let mount_point_str = mount_point.to_string_lossy().into_owned();

        let mounts = host_mount_points().await;

        // A previous version mounted under XDG_RUNTIME_DIR; if such a mount
        // is still around from before an upgrade, retire it rather than
        // leaving two mounts of the same phone.
        let legacy = legacy_mount_point(device_id);
        let legacy_str = legacy.to_string_lossy().into_owned();
        if mounts.iter().any(|m| m == &legacy_str) {
            info!("[sftp] unmounting legacy mount at {}", legacy_str);
            let _ = force_unmount(&legacy_str).await;
        }

        if mounts.iter().any(|m| m == &mount_point_str) {
            if is_healthy(&mount_point_str).await {
                info!("[sftp] {} already mounted", mount_point_str);
                register_mount(device_id, &mount_point);
                open(&mount_point_str).await?;
                return Ok(mount_point);
            }
            // Stale mount ("Transport endpoint is not connected" after the
            // phone dropped off wifi, etc.) — force it out and remount.
            // Best-effort: if the unmount reports an error (e.g. the mount is
            // busy in a file manager) we still fall through to the remount.
            // force_unmount already tries a lazy (-z) unmount, which detaches
            // the wedged mount so a fresh sshfs can take the point over,
            // rather than dead-ending the whole browse on a cleanup hiccup.
            warn!(
                "[sftp] {} is mounted but unresponsive, remounting",
                mount_point_str
            );
            if let Err(e) = force_unmount(&mount_point_str).await {
                warn!(
                    "[sftp] force-unmount of stale {} failed ({}); attempting remount anyway",
                    mount_point_str, e
                );
            }
        }

        run(host_command("mkdir").args(["-p", &mount_point_str])).await?;

        let path = self
            .multi_paths
            .as_ref()
            .and_then(|p| p.first())
            .cloned()
            .or_else(|| self.path.clone())
            .unwrap_or_else(|| "/".to_string());

        info!(
            "[sftp] mounting {}@{}:{} -> {}",
            self.user, self.ip, self.port, mount_point_str
        );

        // A recognisable fsname so the mount is identifiable in the mount
        // table; strip anything that could break `-o` comma parsing.
        let fsname: String = device_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();

        let mut child = host_command("sshfs")
            .arg(format!("{}@{}:{}", self.user, self.ip, path))
            .arg(&mount_point_str)
            .args([
                "-p",
                &self.port.to_string(),
                // Each browse session spins up a brand-new ephemeral SSH
                // host key on the phone, so there's nothing meaningful to
                // pin — host checking would just prompt-and-fail every time.
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "password_stdin",
                // Fail fast instead of hanging the browse click when the
                // phone is unreachable.
                "-o",
                "ConnectTimeout=10",
                // Detect a dead peer within ~45s instead of leaving the
                // mount wedged indefinitely, and transparently re-establish
                // the session when the phone comes back (sshfs keeps the
                // stdin-supplied password in memory for reconnects).
                "-o",
                "reconnect",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                // Cache file data between opens as long as the file is
                // unchanged — repeated browsing/thumbnailing over the phone
                // link is the slow path worth caching.
                "-o",
                "auto_cache",
                "-o",
                "follow_symlinks",
                "-o",
                &format!("fsname=kdeconnect-{}", fsname),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            // If we can't hand sshfs the password it will block on the prompt
            // forever; kill it and clean up rather than leaking a hung
            // process that only surfaces at MOUNT_TIMEOUT.
            let write = async {
                stdin
                    .write_all(format!("{}\n", self.password).as_bytes())
                    .await?;
                stdin.flush().await
            };
            if let Err(e) = write.await {
                let _ = child.kill().await;
                cleanup_mount_dir(&mount_point_str).await;
                return Err(e.into());
            }
            // Close the pipe so sshfs sees EOF after the password line.
            drop(stdin);
        }

        // Drain stderr concurrently so a chatty sshfs can't fill the pipe
        // and deadlock against our wait() below.
        let stderr_pipe = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut stderr) = stderr_pipe {
                let _ = stderr.read_to_end(&mut buf).await;
            }
            buf
        });

        // sshfs daemonizes once the mount is live, so a successful wait()
        // returns quickly; the timeout only fires when something wedged.
        let status = match tokio::time::timeout(MOUNT_TIMEOUT, child.wait()).await {
            Ok(res) => res?,
            Err(_) => {
                let _ = child.kill().await;
                cleanup_mount_dir(&mount_point_str).await;
                anyhow::bail!(
                    "sshfs did not finish mounting within {}s",
                    MOUNT_TIMEOUT.as_secs()
                );
            }
        };

        if !status.success() {
            let stderr = stderr_task.await.unwrap_or_default();
            cleanup_mount_dir(&mount_point_str).await;
            anyhow::bail!("sshfs failed: {}", String::from_utf8_lossy(&stderr).trim());
        }

        register_mount(device_id, &mount_point);
        open(&mount_point_str).await?;
        Ok(mount_point)
    }
}

/// If the device's share is already mounted and responsive, open it in the
/// file manager and return true — the caller can then skip the whole
/// phone round-trip (sftp.request → sftp packet → mount).
pub async fn open_mounted(device_id: &str, device_name: &str) -> bool {
    let lock = op_lock(device_id);
    let _guard = lock.lock().await;

    let mount_point = mount_point_for(device_id, device_name);
    let mount_point_str = mount_point.to_string_lossy().into_owned();

    if !host_mount_points()
        .await
        .iter()
        .any(|m| m == &mount_point_str)
    {
        return false;
    }
    if !is_healthy(&mount_point_str).await {
        return false;
    }
    register_mount(device_id, &mount_point);
    open(&mount_point_str).await.is_ok()
}

/// Unmount a device's share. Returns Ok(true) if a live mount was actually
/// unmounted, Ok(false) if there was nothing mounted.
pub async fn unmount(device_id: &str, device_name: &str) -> anyhow::Result<bool> {
    let lock = op_lock(device_id);
    let _guard = lock.lock().await;

    let mount_point = mount_point_for(device_id, device_name);
    let mount_point_str = mount_point.to_string_lossy().into_owned();

    let mounts = host_mount_points().await;

    // Also retire a pre-upgrade legacy mount if one is lying around.
    let legacy = legacy_mount_point(device_id);
    let legacy_str = legacy.to_string_lossy().into_owned();
    let mut unmounted = false;
    if mounts.iter().any(|m| m == &legacy_str) {
        force_unmount(&legacy_str).await?;
        unmounted = true;
    }

    if mounts.iter().any(|m| m == &mount_point_str) {
        force_unmount(&mount_point_str).await?;
        unmounted = true;
    }

    unregister_mount(device_id);
    cleanup_mount_dir(&mount_point_str).await;
    Ok(unmounted)
}

/// Unmount every share this process knows about — called on service
/// shutdown, after which the mounts would only go stale. Bounded overall so
/// a single wedged mount can't hold up process exit (the phone should see
/// the disconnect promptly).
pub async fn unmount_all() {
    let entries: Vec<(String, PathBuf)> = {
        let guard = MOUNTS.lock().unwrap();
        guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    if entries.is_empty() {
        return;
    }
    let work = async {
        let mounts = host_mount_points().await;
        for (device_id, mount_point) in entries {
            let mount_point_str = mount_point.to_string_lossy().into_owned();
            if mounts.iter().any(|m| m == &mount_point_str) {
                info!("[sftp] unmounting {} on shutdown", mount_point_str);
                let _ = force_unmount(&mount_point_str).await;
            }
            unregister_mount(&device_id);
            cleanup_mount_dir(&mount_point_str).await;
        }
    };
    if tokio::time::timeout(SHUTDOWN_UNMOUNT_BUDGET, work)
        .await
        .is_err()
    {
        warn!("[sftp] shutdown unmount exceeded its time budget; exiting anyway");
    }
}

/// Which of the given (device_id, device_name) pairs currently have a live
/// mount, checked against the host mount table in a single read.
pub async fn mounted_devices(devices: &[(String, String)]) -> Vec<String> {
    if devices.is_empty() {
        return Vec::new();
    }
    let mounts = host_mount_points().await;
    devices
        .iter()
        .filter(|(id, name)| {
            let mp = mount_point_for(id, name).to_string_lossy().into_owned();
            mounts.iter().any(|m| m == &mp)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Specific, actionable reasons `browse()` can't proceed — surfaced to the
/// user as-is, so each variant's message names the actual fix rather than a
/// generic "mount failed".
#[derive(Debug, Clone)]
pub enum BrowsePreflightError {
    /// `sshfs` isn't on the host. A missing-package problem — no sandbox
    /// permission fixes this.
    SshfsMissing,
    /// `flatpak-spawn --host` itself failed. Usually means the
    /// `org.freedesktop.Flatpak` D-Bus name was revoked (e.g. in Flatseal).
    FlatpakSpawnUnavailable,
}

impl std::fmt::Display for BrowsePreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SshfsMissing => write!(
                f,
                "sshfs is not installed on this system. Install it (e.g. `sudo apt install sshfs` or `sudo dnf install fuse-sshfs`) to browse device files."
            ),
            Self::FlatpakSpawnUnavailable => write!(
                f,
                "Can't reach the host to mount the device. Check that this app's \"org.freedesktop.Flatpak\" permission hasn't been revoked (e.g. in Flatseal)."
            ),
        }
    }
}

impl std::error::Error for BrowsePreflightError {}

/// One-way latch: once the environment checks pass they can't un-pass
/// within this process's lifetime, so don't re-spawn two host processes on
/// every browse click. Failures are NOT cached — the user may install sshfs
/// and retry immediately.
static PREFLIGHT_PASSED: AtomicBool = AtomicBool::new(false);

async fn preflight() -> Result<(), BrowsePreflightError> {
    if PREFLIGHT_PASSED.load(Ordering::Relaxed) {
        return Ok(());
    }

    if in_flatpak() {
        let reachable = Command::new("flatpak-spawn")
            .args(["--host", "true"])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !reachable {
            return Err(BrowsePreflightError::FlatpakSpawnUnavailable);
        }
    }

    let has_sshfs = host_command("sh")
        .args(["-c", "command -v sshfs"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !has_sshfs {
        return Err(BrowsePreflightError::SshfsMissing);
    }

    PREFLIGHT_PASSED.store(true, Ordering::Relaxed);
    Ok(())
}

/// True when running inside a Flatpak sandbox.
fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Builds a command that runs on the host when sandboxed (via
/// `flatpak-spawn --host`, already permitted by
/// `--talk-name=org.freedesktop.Flatpak` in the manifest) and runs directly
/// otherwise.
fn host_command(program: &str) -> Command {
    if in_flatpak() {
        let mut cmd = Command::new("flatpak-spawn");
        cmd.arg("--host").arg(program);
        cmd
    } else {
        Command::new(program)
    }
}

fn mounts_base_dir() -> PathBuf {
    // Deliberately a *visible* directory in $HOME: GIO hides mounts whose
    // path contains a dot-component, and showing up in the file manager
    // sidebar (with an eject button) is the whole point of this location.
    // The directory only exists while something is mounted — unmount
    // removes it again.
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("KDE Connect")
}

fn legacy_mount_point(device_id: &str) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir)
        .join("kdeconnect-sftp")
        .join(device_id)
}

/// The directory basename doubles as the label the file manager shows for
/// the mount, so it's the device's human name — sanitized, since it comes
/// from the network.
fn sanitize_name(device_id: &str, device_name: &str) -> String {
    let cleaned: String = device_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();
    if cleaned.is_empty() {
        format!("device-{}", &device_id[..device_id.len().min(8)])
    } else {
        cleaned
    }
}

fn mount_point_for(device_id: &str, device_name: &str) -> PathBuf {
    let guard = MOUNTS.lock().unwrap();
    if let Some(existing) = guard.get(device_id) {
        return existing.clone();
    }
    let mut candidate = mounts_base_dir().join(sanitize_name(device_id, device_name));
    // Two devices with the same human name must not fight over one mount
    // point; disambiguate the second with a device-id suffix.
    let taken_by_other = guard
        .iter()
        .any(|(id, path)| id != device_id && *path == candidate);
    if taken_by_other {
        let suffix: String = device_id.chars().take(6).collect();
        candidate = mounts_base_dir().join(format!(
            "{} ({})",
            sanitize_name(device_id, device_name),
            suffix
        ));
    }
    candidate
}

fn register_mount(device_id: &str, mount_point: &Path) {
    MOUNTS
        .lock()
        .unwrap()
        .insert(device_id.to_string(), mount_point.to_path_buf());
}

fn unregister_mount(device_id: &str) {
    MOUNTS.lock().unwrap().remove(device_id);
}

/// Reads the host's `/proc/mounts` (via `host_command`, so this is the
/// host's mount table even when sandboxed) rather than depending on the
/// `mountpoint` binary just to answer a question the kernel already tracks.
/// Returns unescaped mount-point paths — the kernel octal-escapes spaces
/// (`\040`) etc., and our mount points contain device names.
async fn host_mount_points() -> Vec<String> {
    let fut = host_command("cat").arg("/proc/mounts").output();
    let output = match tokio::time::timeout(AUX_CMD_TIMEOUT, fut).await {
        Ok(Ok(o)) if o.status.success() => o,
        Ok(_) => return Vec::new(),
        Err(_) => {
            warn!("[sftp] reading /proc/mounts timed out");
            return Vec::new();
        }
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(unescape_mount_path))
        .collect()
}

/// Undo the octal escaping /proc/mounts applies to whitespace and
/// backslashes in paths (`\040` space, `\011` tab, `\012` newline,
/// `\134` backslash).
///
/// Works at the byte level and reassembles UTF-8 only at the end: device
/// names can be non-ASCII (Cyrillic, emoji, …), and the kernel escapes
/// individual *bytes*, so a multi-byte character can appear as several
/// `\NNN` escapes. Decoding each escape straight to `char` would corrupt
/// those, making the path fail to match our own mount point and breaking
/// both remount detection and unmount.
fn unescape_mount_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && let Ok(oct) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(oct);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A mount can be present in the mount table yet dead ("Transport endpoint
/// is not connected"). Probe it with a time-boxed stat so the answer comes
/// back fast either way.
async fn is_healthy(mount_point: &str) -> bool {
    let fut = host_command("timeout")
        .args([HEALTH_TIMEOUT_SECS, "stat", "--", mount_point])
        .output();
    match tokio::time::timeout(AUX_CMD_TIMEOUT, fut).await {
        Ok(Ok(o)) => o.status.success(),
        // Command error or an outer timeout (flatpak-spawn itself wedged):
        // treat as unhealthy so the caller remounts rather than trusting a
        // mount we can't verify.
        _ => false,
    }
}

/// Unmount via fusermount (the unprivileged FUSE path), falling back to a
/// lazy unmount if the mount point is busy — e.g. a file manager still has
/// it open, which is exactly the situation after auto-unmount-on-disconnect.
async fn force_unmount(mount_point: &str) -> anyhow::Result<()> {
    const SCRIPT: &str = r#"
if command -v fusermount3 >/dev/null 2>&1; then FM=fusermount3; else FM=fusermount; fi
"$FM" -u -- "$1" 2>/dev/null || "$FM" -u -z -- "$1"
"#;
    let fut = host_command("sh")
        .args(["-c", SCRIPT, "sh", mount_point])
        .output();
    let output = match tokio::time::timeout(AUX_CMD_TIMEOUT, fut).await {
        Ok(res) => res?,
        Err(_) => anyhow::bail!("unmount of {} timed out", mount_point),
    };
    if !output.status.success() {
        anyhow::bail!(
            "unmount of {} failed: {}",
            mount_point,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Remove the (now empty) mount directory, and the shared base directory if
/// this was the last mount. `rmdir` only — never delete contents.
async fn cleanup_mount_dir(mount_point: &str) {
    let base = mounts_base_dir().to_string_lossy().into_owned();
    let fut = host_command("sh")
        .args([
            "-c",
            r#"rmdir -- "$1" 2>/dev/null; rmdir -- "$2" 2>/dev/null; true"#,
            "sh",
            mount_point,
            &base,
        ])
        .status();
    let _ = tokio::time::timeout(AUX_CMD_TIMEOUT, fut).await;
}

async fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let output = cmd.output().await?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

async fn open(mount_point: &str) -> anyhow::Result<()> {
    let mut child = host_command("xdg-open").arg(mount_point).spawn()?;
    // xdg-open forks its handler and exits quickly; reap it in the
    // background so it doesn't linger as a zombie.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_space() {
        assert_eq!(
            unescape_mount_path("/home/user/KDE\\040Connect/Pixel\\0407"),
            "/home/user/KDE Connect/Pixel 7"
        );
    }

    #[test]
    fn unescape_plain() {
        assert_eq!(
            unescape_mount_path("/run/user/1000/doc"),
            "/run/user/1000/doc"
        );
    }

    #[test]
    fn unescape_backslash() {
        assert_eq!(unescape_mount_path("a\\134b"), "a\\b");
    }

    #[test]
    fn unescape_multibyte_utf8() {
        // The kernel escapes non-printable/whitespace bytes octally but a
        // multi-byte UTF-8 char's raw bytes appear verbatim in /proc/mounts.
        // Decoding must reassemble them into the original string, not one
        // Latin-1 char per byte.
        let name = "Мой телефон";
        let path = format!("/home/u/KDE\\040Connect/{}", name);
        assert_eq!(
            unescape_mount_path(&path),
            format!("/home/u/KDE Connect/{}", name)
        );
    }

    #[test]
    fn sanitize_keeps_normal_names() {
        assert_eq!(sanitize_name("id", "Pixel 7 Pro"), "Pixel 7 Pro");
    }

    #[test]
    fn sanitize_replaces_path_chars() {
        assert_eq!(sanitize_name("id", "evil/../name"), "evil-..-name");
    }

    #[test]
    fn sanitize_empty_falls_back_to_id() {
        assert_eq!(sanitize_name("abcdef1234", "///"), "---");
        assert_eq!(sanitize_name("abcdef1234", ""), "device-abcdef12");
    }
}
