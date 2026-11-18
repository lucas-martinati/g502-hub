//! Self-updater and GitHub Releases integration with SHA-256 verification.
use crate::config;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const REPO: &str = "lucas-martinati/g502-hub";
pub const PROGRESS_BAR_WIDTH: usize = 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub download_url: String,
    pub checksum_url: Option<String>,
    pub asset_size: Option<u64>,
    pub release_url: String,
    pub notes: String,
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Pure-Rust FIPS 180-4 SHA-256 digest (lowercase hex).
pub fn sha256_bytes(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (dst, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *dst = dst.wrapping_add(add);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

/// Parses a semantic version string (e.g. "0.3.0", "v1.2.3", "1.2.3-beta1") into
/// `(major, minor, patch, is_stable)` where stable > prerelease for the same core version.
pub fn parse_semver(s: &str) -> Option<(u64, u64, u64, bool)> {
    let clean = s.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match clean.split_once('-') {
        Some((c, p)) => (c, !p.is_empty()),
        None => (clean.split('+').next().unwrap_or(clean), false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, !pre))
}

pub fn is_newer_version(remote: &str, current: &str) -> bool {
    match (parse_semver(remote), parse_semver(current)) {
        (Some(r), Some(c)) => r > c,
        _ => false,
    }
}

pub fn install_bin_dir() -> PathBuf {
    config::data_dir().join("bin")
}

/// Returns true only when running from a per-user installed location (`~/.local/share/g502-hub-rust/bin`
/// or `~/.local/bin`), never from a Cargo `target/` checkout or `/usr/bin` system package.
pub fn can_auto_update_exe(exe: &Path) -> bool {
    let canonical = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let text = canonical.to_string_lossy();
    if text.contains("/target/debug/")
        || text.contains("/target/release/")
        || text.starts_with("/usr/")
    {
        return false;
    }
    let user_bin = install_bin_dir();
    let local_bin = config::home().join(".local/bin");
    canonical.starts_with(user_bin.canonicalize().unwrap_or(user_bin))
        || canonical.starts_with(local_bin.canonicalize().unwrap_or(local_bin))
}

pub fn can_auto_update_current_exe() -> bool {
    std::env::current_exe().is_ok_and(|p| can_auto_update_exe(&p))
}

fn target_arch_suffix() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "linux-aarch64.tar.gz",
        _ => "linux-x86_64.tar.gz",
    }
}

pub fn parse_release_json(json: &Value, current_version: &str) -> Result<Option<UpdateInfo>> {
    let tag_name = json["tag_name"]
        .as_str()
        .context("Missing tag_name in GitHub release response")?;
    let latest_version = tag_name.trim_start_matches(['v', 'V']).to_owned();
    if !is_newer_version(&latest_version, current_version) {
        return Ok(None);
    }
    let suffix = target_arch_suffix();
    let mut download_url = None;
    let mut asset_size = None;
    let mut matched_name = String::new();
    let mut checksums = std::collections::HashMap::<String, String>::new();

    if let Some(assets) = json["assets"].as_array() {
        for asset in assets {
            let Some(name) = asset["name"].as_str() else {
                continue;
            };
            let Some(url) = asset["browser_download_url"].as_str() else {
                continue;
            };
            if name.ends_with(".sha256") {
                checksums.insert(name.to_owned(), url.to_owned());
                continue;
            }
            if name.ends_with(suffix) {
                let is_versioned = name.contains(&latest_version);
                if download_url.is_none() || is_versioned {
                    download_url = Some(url.to_owned());
                    asset_size = asset["size"].as_u64();
                    matched_name = name.to_owned();
                    if is_versioned {
                        // Prefer the versioned archive and keep scanning only for .sha256 files.
                    }
                }
            }
        }
    }

    let checksum_url = if !matched_name.is_empty() {
        checksums
            .get(&format!("{matched_name}.sha256"))
            .or_else(|| checksums.get(&format!("g502-hub-{suffix}.sha256")))
            .cloned()
    } else {
        None
    };

    let dl_url = download_url.unwrap_or_else(|| {
        format!("https://github.com/{REPO}/releases/latest/download/g502-hub-{suffix}")
    });
    let release_url = json["html_url"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases/tag/{tag_name}"));
    let notes = json["body"].as_str().unwrap_or("").trim().to_owned();

    Ok(Some(UpdateInfo {
        current_version: current_version.to_owned(),
        latest_version,
        download_url: dl_url,
        checksum_url,
        asset_size,
        release_url,
        notes,
    }))
}

pub fn check_for_updates() -> Result<Option<UpdateInfo>> {
    check_for_updates_from(env!("CARGO_PKG_VERSION"))
}

pub fn check_for_updates_from(current_version: &str) -> Result<Option<UpdateInfo>> {
    let api_url = std::env::var("G502_HUB_RELEASE_API")
        .unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"));
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "User-Agent: g502-hub-updater",
            "-H",
            "Accept: application/vnd.github+json",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            &api_url,
        ])
        .output()
        .context("Install curl to check for updates")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Could not reach GitHub Releases{}",
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        );
    }
    let json: Value =
        serde_json::from_slice(&output.stdout).context("Invalid GitHub release JSON")?;
    parse_release_json(&json, current_version)
}

fn fetch_expected_sha256(url: &str) -> Result<String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "User-Agent: g502-hub-updater",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            url,
        ])
        .output()
        .context("Failed to download SHA-256 checksum")?;
    ensure!(output.status.success(), "Checksum download failed");
    let text = String::from_utf8_lossy(&output.stdout);
    let token = text
        .split_whitespace()
        .next()
        .context("Empty SHA-256 checksum file")?;
    ensure!(
        token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()),
        "Malformed SHA-256 checksum"
    );
    Ok(token.to_lowercase())
}

fn verify_elf(path: &Path) -> Result<()> {
    let mut f = File::open(path).with_context(|| format!("Missing binary {}", path.display()))?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)
        .with_context(|| format!("Truncated binary {}", path.display()))?;
    ensure!(
        &magic == b"\x7fELF",
        "{} is not a valid Linux ELF executable",
        path.display()
    );
    Ok(())
}

fn atomic_install_binary(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let staging = dest.with_extension(format!("new-{}", std::process::id()));
    fs::copy(src, &staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
    fs::rename(&staging, dest)?;
    Ok(())
}

fn refresh_desktop_entries(gui_bin: &Path) -> Result<()> {
    let escaped = gui_bin
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let menu = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home().join(".local/share"))
        .join("applications");
    fs::create_dir_all(&menu)?;
    let desktop = menu.join("io.github.lucas_martinati.G502Hub.desktop");
    fs::write(
        &desktop,
        format!(
            "[Desktop Entry]\nType=Application\nName=G502 Hub\nComment=DPI, lighting, buttons and profiles for the Logitech G502 HERO\nExec=\"{escaped}\"\nIcon=input-mouse\nTerminal=false\nCategories=Settings;HardwareSettings;\nStartupWMClass=g502-hub\n"
        ),
    )?;
    let autostart = config::autostart_path();
    if autostart.exists() {
        fs::write(
            &autostart,
            format!(
                "[Desktop Entry]\nType=Application\nName=G502 Hub\nExec=\"{escaped}\" --background\nIcon=input-mouse\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
            ),
        )?;
    }
    let _ = Command::new("update-desktop-database")
        .arg(&menu)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(())
}

/// Downloads, SHA-256-verifies, extracts, and atomically installs `g502-hub` and `g502-hubd`.
/// Returns the path to the installed `g502-hub` GUI executable.
pub fn download_and_install_update(
    info: &UpdateInfo,
    mut on_progress: impl FnMut(u64, Option<u64>, f64),
) -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && exe.starts_with("/usr/")
        && unsafe { libc::geteuid() } != 0
    {
        bail!(
            "G502 Hub is installed system-wide in {}. Update via your package manager or run with privileges.",
            exe.display()
        );
    }

    let work_dir = std::env::temp_dir().join(format!(
        "g502-hub-update-{}-{}-{}",
        unsafe { libc::getuid() },
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&work_dir)?;
    fs::set_permissions(&work_dir, fs::Permissions::from_mode(0o700))?;
    let _guard = TempDirGuard(work_dir.clone());

    let archive_path = work_dir.join("release.tar.gz");
    let mut child = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            "300",
            &info.download_url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Install curl to download updates")?;

    let mut stdout = child.stdout.take().context("Missing curl output stream")?;
    let mut out_file = File::create(&archive_path)?;
    let mut buf = [0u8; 16384];
    let mut downloaded = 0u64;
    let start = Instant::now();
    let mut last_tick = Instant::now();

    on_progress(0, info.asset_size, 0.0);
    loop {
        let n = stdout.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out_file.write_all(&buf[..n])?;
        downloaded += n as u64;
        if last_tick.elapsed() >= Duration::from_millis(33)
            || info.asset_size.is_some_and(|tot| downloaded >= tot)
        {
            let secs = start.elapsed().as_secs_f64().max(1e-3);
            on_progress(downloaded, info.asset_size, downloaded as f64 / secs);
            last_tick = Instant::now();
        }
    }
    out_file.flush()?;
    drop(out_file);

    let status = child.wait()?;
    ensure!(
        status.success() && downloaded > 0,
        "Download failed (connection interrupted or release asset not found)"
    );

    if let Some(cs_url) = &info.checksum_url {
        let expected = fetch_expected_sha256(cs_url)?;
        let actual = sha256_file(&archive_path)?;
        ensure!(
            actual == expected,
            "SHA-256 checksum mismatch (expected {expected}, got {actual})"
        );
    }

    let extract_dir = work_dir.join("extracted");
    fs::create_dir_all(&extract_dir)?;
    let untar = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("--strip-components=1")
        .arg("-C")
        .arg(&extract_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Install tar to extract updates")?;
    ensure!(untar.success(), "Failed to extract release archive");

    let src_gui = extract_dir.join("g502-hub");
    let src_daemon = extract_dir.join("g502-hubd");
    verify_elf(&src_gui)?;
    verify_elf(&src_daemon)?;

    let bin_dir = install_bin_dir();
    let dest_gui = bin_dir.join("g502-hub");
    let dest_daemon = bin_dir.join("g502-hubd");
    atomic_install_binary(&src_gui, &dest_gui)?;
    atomic_install_binary(&src_daemon, &dest_daemon)?;

    // If launched from another writable non-Cargo user directory (e.g. ~/.local/bin), keep it in sync too.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let dir_canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let bin_canon = bin_dir.canonicalize().unwrap_or_else(|_| bin_dir.clone());
        let text = dir_canon.to_string_lossy();
        if dir_canon != bin_canon
            && !text.contains("/target/debug")
            && !text.contains("/target/release")
            && !text.starts_with("/usr/")
        {
            let _ = atomic_install_binary(&src_gui, &dir.join("g502-hub"));
            let _ = atomic_install_binary(&src_daemon, &dir.join("g502-hubd"));
        }
    }

    let data = config::data_dir();
    fs::create_dir_all(&data)?;
    for doc in ["LICENSE", "NOTICE.md", "README.md"] {
        let p = extract_dir.join(doc);
        if p.is_file() {
            let _ = fs::copy(&p, data.join(doc));
        }
    }
    let _ = refresh_desktop_entries(&dest_gui);
    Ok(dest_gui)
}

// ---------------------------------------------------------------------------
// Terminal Updater UI (rcloneDash-style wizard + live progress bar)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct ConsoleStyle {
    pub tty: bool,
    pub color: bool,
    pub ascii: bool,
}

impl ConsoleStyle {
    pub fn detect() -> Self {
        let tty = unsafe { libc::isatty(std::io::stdout().as_raw_fd()) == 1 };
        let no_color = std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").unwrap_or_default() == "dumb";
        let ascii = std::env::var("TERM").unwrap_or_default() == "linux";
        Self {
            tty,
            color: tty && !no_color,
            ascii,
        }
    }
    pub fn plain() -> Self {
        Self {
            tty: false,
            color: false,
            ascii: false,
        }
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }
    pub fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    pub fn bold_cyan(&self, s: &str) -> String {
        self.wrap("1;36", s)
    }
    pub fn bold_green(&self, s: &str) -> String {
        self.wrap("1;32", s)
    }
    pub fn bold_red(&self, s: &str) -> String {
        self.wrap("1;31", s)
    }
    pub fn gray(&self, s: &str) -> String {
        self.wrap("90", s)
    }
    fn vline(&self) -> &'static str {
        if self.ascii { "|" } else { "│" }
    }
    fn diamond(&self) -> &'static str {
        if self.ascii { "o" } else { "◇" }
    }
    fn corner_tl(&self) -> &'static str {
        if self.ascii { "+" } else { "┌" }
    }
    fn corner_bl(&self) -> &'static str {
        if self.ascii { "+" } else { "└" }
    }
    fn hline(&self) -> &'static str {
        if self.ascii { "-" } else { "─" }
    }
    fn fill(&self) -> &'static str {
        if self.ascii { "#" } else { "█" }
    }
    fn empty(&self) -> &'static str {
        if self.ascii { "-" } else { "░" }
    }
}

pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

pub fn progress_line(
    style: &ConsoleStyle,
    downloaded: u64,
    total: Option<u64>,
    speed: f64,
    done: bool,
) -> String {
    let unknown_total = total.is_none_or(|t| t == 0);
    let (pct_raw, bar) = if done && unknown_total {
        ("100%".to_owned(), style.fill().repeat(PROGRESS_BAR_WIDTH))
    } else if let Some(tot) = total.filter(|&t| t > 0) {
        let pct = (downloaded as f64 / tot as f64).clamp(0.0, 1.0);
        let filled = (pct * PROGRESS_BAR_WIDTH as f64).round() as usize;
        let empty = PROGRESS_BAR_WIDTH.saturating_sub(filled);
        (
            format!("{:>3.0}%", pct * 100.0),
            format!(
                "{}{}",
                style.fill().repeat(filled),
                style.empty().repeat(empty)
            ),
        )
    } else {
        ("---%".to_owned(), style.empty().repeat(PROGRESS_BAR_WIDTH))
    };
    let pct = if pct_raw == "100%" {
        style.bold_green(&pct_raw)
    } else {
        style.bold(&pct_raw)
    };
    let size_info = match total {
        Some(tot) if tot > 0 => format!("{} / {}", format_size(downloaded), format_size(tot)),
        _ => format_size(downloaded),
    };
    let speed_info = format!("{}/s", format_size(speed as u64));
    format!(
        "  {}  {}[{}]{} {}  {} ({})",
        style.vline(),
        if style.color { "\x1b[32m" } else { "" },
        bar,
        if style.color { "\x1b[0m" } else { "" },
        pct,
        style.gray(&size_info),
        style.gray(&speed_info),
    )
}

pub fn run_cli_check() -> Result<()> {
    let s = ConsoleStyle::detect();
    let current = env!("CARGO_PKG_VERSION");
    println!(
        "  {}{} {}",
        s.corner_tl(),
        s.hline(),
        s.bold_cyan("G502 Hub Updater")
    );
    println!("  {}", s.vline());
    println!(
        "  {}  Checking GitHub Releases ({REPO})...",
        s.bold_cyan(s.diamond())
    );
    match check_for_updates() {
        Ok(Some(info)) => {
            println!(
                "  {}  Current: {}  →  Latest: {}",
                s.vline(),
                s.gray(&format!("v{}", info.current_version)),
                s.bold_green(&format!("v{}", info.latest_version))
            );
            println!("  {}", s.vline());
            println!(
                "  {}{} {}",
                s.corner_bl(),
                s.hline(),
                s.bold_green(&format!(
                    "Update available: v{}! Run 'g502-hub --update' to install.",
                    info.latest_version
                ))
            );
            Ok(())
        }
        Ok(None) => {
            println!("  {}", s.vline());
            println!(
                "  {}{} {}",
                s.corner_bl(),
                s.hline(),
                s.bold_green(&format!("G502 Hub is up to date (v{current})."))
            );
            Ok(())
        }
        Err(e) => {
            println!("  {}", s.vline());
            eprintln!(
                "  {}{} {}",
                s.corner_bl(),
                s.hline(),
                s.bold_red(&format!("Update check failed: {e}"))
            );
            Err(e)
        }
    }
}

pub fn run_cli_update() -> Result<()> {
    let s = ConsoleStyle::detect();
    let current = env!("CARGO_PKG_VERSION");
    println!(
        "  {}{} {}",
        s.corner_tl(),
        s.hline(),
        s.bold_cyan("G502 Hub Updater")
    );
    println!("  {}", s.vline());
    println!(
        "  {}  Checking for latest release...",
        s.bold_cyan(s.diamond())
    );
    let Some(info) = check_for_updates()? else {
        println!("  {}", s.vline());
        println!(
            "  {}{} {}",
            s.corner_bl(),
            s.hline(),
            s.bold_green(&format!("Already on the latest version (v{current})."))
        );
        return Ok(());
    };
    println!(
        "  {}  Found {} (current: {})",
        s.vline(),
        s.bold(&format!("v{}", info.latest_version)),
        s.gray(&format!("v{current}"))
    );
    println!("  {}", s.vline());
    println!(
        "  {}  Downloading G502 Hub {}...",
        s.bold_cyan(s.diamond()),
        s.bold(&format!("v{}", info.latest_version))
    );

    let mut final_dl = 0u64;
    let mut final_speed = 0.0f64;
    let gui_bin = download_and_install_update(&info, |dl, tot, speed| {
        final_dl = dl;
        final_speed = speed;
        if s.tty {
            print!("\r{}\x1b[K", progress_line(&s, dl, tot, speed, false));
            let _ = std::io::stdout().flush();
        }
    })?;
    if s.tty {
        println!(
            "\r{}\x1b[K",
            progress_line(&s, final_dl, info.asset_size, final_speed, true)
        );
    } else {
        println!("  {}  Downloaded {}", s.vline(), format_size(final_dl));
    }
    if info.checksum_url.is_some() {
        println!("  {}  Verified SHA-256 checksum", s.vline());
    }
    println!(
        "  {}  Installed binaries to {}",
        s.vline(),
        gui_bin.parent().unwrap_or(Path::new("")).display()
    );

    // Restart running daemon if active so the new service takes effect immediately.
    if crate::ipc::request("ping", serde_json::json!({})).is_ok() {
        println!(
            "  {}  Restarting background service...",
            s.bold_cyan(s.diamond())
        );
        let _ = crate::ipc::request("stop", serde_json::json!({}));
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3)
            && crate::ipc::request("ping", serde_json::json!({})).is_ok()
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = Command::new(&gui_bin)
            .arg("--background")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    println!("  {}", s.vline());
    println!(
        "  {}{} {}",
        s.corner_bl(),
        s.hline(),
        s.bold_green(&format!(
            "Successfully updated to v{}!",
            info.latest_version
        ))
    );
    Ok(())
}

pub fn run_cli_demo() {
    let s = ConsoleStyle::detect();
    let current = env!("CARGO_PKG_VERSION");
    let latest = match parse_semver(current) {
        Some((maj, min, patch, _)) => format!("{maj}.{min}.{}", patch + 1),
        None => format!("{current}+demo1"),
    };
    println!(
        "  {}{} {}",
        s.corner_tl(),
        s.hline(),
        s.bold_cyan("G502 Hub Updater (demo)")
    );
    println!("  {}", s.vline());
    println!(
        "  {}  Checking for latest release...",
        s.bold_cyan(s.diamond())
    );
    std::thread::sleep(Duration::from_millis(300));
    println!(
        "  {}  Found {} (current: {})",
        s.vline(),
        s.bold(&format!("v{latest}")),
        s.gray(&format!("v{current}"))
    );
    println!("  {}", s.vline());
    println!(
        "  {}  Downloading G502 Hub {}...",
        s.bold_cyan(s.diamond()),
        s.bold(&format!("v{latest}"))
    );
    let total = 2_420_000u64;
    let start = Instant::now();
    if s.tty {
        let mut dl = 0u64;
        while dl < total {
            std::thread::sleep(Duration::from_millis(35));
            dl = (dl + 155_000).min(total);
            let speed = dl as f64 / start.elapsed().as_secs_f64().max(1e-3);
            print!(
                "\r{}\x1b[K",
                progress_line(&s, dl, Some(total), speed, false)
            );
            let _ = std::io::stdout().flush();
        }
        let speed = total as f64 / start.elapsed().as_secs_f64().max(1e-3);
        println!(
            "\r{}\x1b[K",
            progress_line(&s, total, Some(total), speed, true)
        );
    } else {
        println!(
            "{}",
            progress_line(&s, total, Some(total), 3_800_000.0, true)
        );
    }
    println!("  {}  Verified SHA-256 checksum", s.vline());
    println!("  {}  Updated g502-hub and g502-hubd atomically", s.vline());
    println!("  {}", s.vline());
    println!(
        "  {}{} {}",
        s.corner_bl(),
        s.hline(),
        s.bold_green(&format!("Successfully updated to v{latest}!"))
    );
    println!(
        "  {}  {}",
        s.vline(),
        s.gray("demo mode — nothing was downloaded or modified")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sha256_matches_nist_test_vectors() {
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_bytes(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn semver_ordering_handles_tags_and_prereleases() {
        assert!(is_newer_version("0.3.1", "0.3.0"));
        assert!(is_newer_version("v0.4.0", "0.3.9"));
        assert!(is_newer_version("1.0.0", "0.9.9"));
        assert!(is_newer_version("0.3.0", "0.3.0-beta.1"));
        assert!(!is_newer_version("0.3.0-beta.1", "0.3.0"));
        assert!(!is_newer_version("0.3.0", "0.3.0"));
        assert!(!is_newer_version("0.2.1", "0.3.0"));
        assert!(!is_newer_version("invalid", "0.3.0"));
    }

    #[test]
    fn release_json_selects_matching_archive_and_checksum() {
        let arch = target_arch_suffix();
        let archive = format!("g502-hub-v0.4.0-{arch}");
        let sha = format!("{archive}.sha256");
        let payload = json!({
            "tag_name": "v0.4.0",
            "html_url": "https://github.com/lucas-martinati/g502-hub/releases/tag/v0.4.0",
            "body": "Bug fixes and auto-updater",
            "assets": [
                {
                    "name": archive,
                    "browser_download_url": "https://example.invalid/g502-hub-v0.4.0.tar.gz",
                    "size": 2_500_000
                },
                {
                    "name": sha,
                    "browser_download_url": "https://example.invalid/g502-hub-v0.4.0.tar.gz.sha256",
                    "size": 96
                }
            ]
        });
        let info = parse_release_json(&payload, "0.3.0").unwrap().unwrap();
        assert_eq!(info.latest_version, "0.4.0");
        assert_eq!(
            info.download_url,
            "https://example.invalid/g502-hub-v0.4.0.tar.gz"
        );
        assert_eq!(
            info.checksum_url.as_deref(),
            Some("https://example.invalid/g502-hub-v0.4.0.tar.gz.sha256")
        );
        assert_eq!(info.asset_size, Some(2_500_000));
        assert!(parse_release_json(&payload, "0.4.0").unwrap().is_none());
    }

    #[test]
    fn auto_update_guard_excludes_cargo_target_and_usr() {
        assert!(!can_auto_update_exe(Path::new(
            "/home/user/repo/target/release/g502-hub"
        )));
        assert!(!can_auto_update_exe(Path::new("/usr/bin/g502-hub")));
        let user_bin = install_bin_dir().join("g502-hub");
        assert!(can_auto_update_exe(&user_bin));
    }

    #[test]
    fn progress_bar_renders_clean_plain_output() {
        let s = ConsoleStyle::plain();
        let line = progress_line(&s, 1024, Some(2048), 1024.0, false);
        assert!(line.contains(" 50%"));
        assert!(line.contains("1.0 KB / 2.0 KB"));
        assert!(!line.contains('\x1b'));
    }
}
