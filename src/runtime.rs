//! Managed signal-cli runtime: downloaded once on first use so users never
//! install signal-cli (or Java) themselves.
//!
//! signal-cli can't ship inside the plugin: its only Java-free build is the
//! Linux x86_64 GraalVM binary (~120 MB, over Vicuna's 64 MB plugin cap), and
//! every other platform needs a Java 25 runtime. So we fetch pinned, sha256-
//! verified archives (signal-cli + Eclipse Temurin JRE) into
//! `<plugin config>/signal-runtime/` with the system `curl` and `tar`
//! (present on macOS, Linux and Windows 10+; Windows `tar.exe` reads zip).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::process::Command;


#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// Folder name under the runtime root (unique per version).
    pub dir: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
    /// Shown in progress text.
    pub label: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct Plan {
    pub signal: Asset,
    pub jre: Option<Asset>,
}

const SIGNAL_JVM: Asset = Asset {
    dir: "signal-cli-0.14.10",
    url: "https://github.com/AsamK/signal-cli/releases/download/v0.14.10/signal-cli-0.14.10.tar.gz",
    sha256: "706a57d9faae7525cf732936efdabf210146e3c83f9e8ae7e4e62f5946121c33",
    size: 126_454_438,
    label: "signal-cli",
};

#[allow(dead_code)]
const SIGNAL_NATIVE: Asset = Asset {
    dir: "signal-cli-0.14.10-native",
    url: "https://github.com/AsamK/signal-cli/releases/download/v0.14.10/signal-cli-0.14.10-Linux-native.tar.gz",
    sha256: "db9297789c5bbc6131df505d2f37abe43efa0b54200585f5e56b478e1cd93916",
    size: 118_889_585,
    label: "signal-cli",
};


#[allow(dead_code)]
const JRE_MAC_ARM: Asset = Asset {
    dir: "jre-25.0.5.1_2",
    url: "https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5.1%2B2/OpenJDK25U-jre_aarch64_mac_hotspot_25.0.5.1_2.tar.gz",
    sha256: "70adbb3ce5cf924c8b60dd84272fdd5fba6d021014aa0c1f5cf66a198bd270c5",
    size: 56_303_568,
    label: "Java runtime",
};
#[allow(dead_code)]
const JRE_MAC_X64: Asset = Asset {
    dir: "jre-25.0.5.1_2",
    url: "https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5.1%2B2/OpenJDK25U-jre_x64_mac_hotspot_25.0.5.1_2.tar.gz",
    sha256: "fac7e81bba96b45cfc3381dbc8bbca6ffd129e3e30aed70493245b61dde7d639",
    size: 42_215_950,
    label: "Java runtime",
};
#[allow(dead_code)]
const JRE_WIN_X64: Asset = Asset {
    dir: "jre-25.0.5.1_2",
    url: "https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5.1%2B2/OpenJDK25U-jre_x64_windows_hotspot_25.0.5.1_2.zip",
    sha256: "3260a1b9899f30ac80d737daafc053042c525249f52d066101fbf81d78d73f25",
    size: 58_599_439,
    label: "Java runtime",
};

/// The download plan for this build's platform; `None` where signal-cli has
/// no bundled libsignal (e.g. Linux aarch64), which needs a system install.
pub fn plan() -> Option<Plan> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(Plan { signal: SIGNAL_NATIVE, jre: None })
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(Plan { signal: SIGNAL_JVM, jre: Some(JRE_MAC_ARM) })
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(Plan { signal: SIGNAL_JVM, jre: Some(JRE_MAC_X64) })
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(Plan { signal: SIGNAL_JVM, jre: Some(JRE_WIN_X64) })
    } else {
        None
    }
}

/// What to spawn: the signal-cli launcher plus, for JVM builds, `JAVA_HOME`.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub bin: PathBuf,
    pub java_home: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    Downloading { label: &'static str, done: u64, total: u64 },
    Installing,
    Failed(String),
    Ready,
}

impl Status {
    /// One-line progress text for the dialog; `None` when there is nothing to say.
    pub fn describe(&self) -> Option<String> {
        let mb = |b: u64| b / 1_000_000;
        match self {
            Status::Downloading { label, done, total } => Some(format!(
                "Setting up Signal for first use: downloading {label} ({} of {} MB)…",
                mb(*done),
                mb(*total)
            )),
            Status::Installing => Some("Setting up Signal for first use: unpacking…".into()),
            Status::Failed(e) => Some(format!("Signal setup failed: {e} Press Continue to retry.")),
            Status::Idle | Status::Ready => None,
        }
    }
}

pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.parent().unwrap_or(data_dir).join("signal-runtime")
}

fn launcher_name() -> &'static str {
    if cfg!(windows) { "signal-cli.bat" } else { "signal-cli" }
}

fn java_name() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

/// Depth-limited search for `<dir>/bin/<name>`; returns the file.
fn find_in_bin(dir: &Path, name: &str, depth: u8) -> Option<PathBuf> {
    // JVM/JRE archives keep launchers in bin/; the Linux native archive puts
    // the signal-cli binary at its top level.
    for candidate in [dir.join("bin").join(name), dir.join(name)] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if depth == 0 {
        return None;
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    entries.sort();
    entries.into_iter().find_map(|p| find_in_bin(&p, name, depth - 1))
}

/// The already-installed managed runtime, if complete.
pub fn installed(root: &Path) -> Option<Runtime> {
    let plan = plan()?;
    let sig_dir = root.join(plan.signal.dir);
    let bin = find_in_bin(&sig_dir, launcher_name(), 3)
        .or_else(|| find_in_bin(&sig_dir, "signal-cli", 3))?;
    let java_home = match plan.jre {
        Some(jre) => {
            let java = find_in_bin(&root.join(jre.dir), java_name(), 4)?;
            Some(java.parent()?.parent()?.to_path_buf())
        }
        None => None,
    };
    Some(Runtime { bin, java_home })
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd;
}

/// Download, verify and unpack one asset into `root/<asset.dir>` (atomic rename).
async fn fetch(root: &Path, asset: Asset, report: &(dyn Fn(Status) + Sync)) -> Result<(), String> {
    let dest = root.join(asset.dir);
    if dest.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(root).map_err(|e| format!("mkdir {}: {e}", root.display()))?;
    let archive = root.join(format!(".{}.download", asset.dir));
    let _ = std::fs::remove_file(&archive);

    // Test hook: fetch the same file names from another base (e.g. file:///tmp/mirror/).
    let url = match std::env::var("VICUNA_SIGNAL_RUNTIME_BASE") {
        Ok(base) if !base.trim().is_empty() => {
            format!("{}{}", base.trim(), asset.url.rsplit('/').next().unwrap_or_default())
        }
        _ => asset.url.to_string(),
    };
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--retry", "2", "-o"]).arg(&archive).arg(&url).kill_on_drop(true);
    no_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start curl ({e})."))?;
    loop {
        tokio::select! {
            st = child.wait() => {
                let st = st.map_err(|e| e.to_string())?;
                if !st.success() {
                    let _ = std::fs::remove_file(&archive);
                    return Err(format!("downloading {} failed (curl exit {}). Check your internet connection.", asset.label, st.code().unwrap_or(-1)));
                }
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                let done = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
                report(Status::Downloading { label: asset.label, done, total: asset.size });
            }
        }
    }

    report(Status::Installing);
    let path = archive.clone();
    let got = tokio::task::spawn_blocking(move || sha256_file(&path)).await.map_err(|e| e.to_string())??;
    if got != asset.sha256 {
        let _ = std::fs::remove_file(&archive);
        return Err(format!("{} download is corrupted (checksum mismatch).", asset.label));
    }

    let staging = root.join(format!(".{}.staging", asset.dir));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let mut cmd = Command::new("tar");
    cmd.arg("-xf").arg(&archive).arg("-C").arg(&staging).kill_on_drop(true);
    no_window(&mut cmd);
    let out = cmd.output().await.map_err(|e| format!("couldn't start tar ({e})."))?;
    let _ = std::fs::remove_file(&archive);
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "unpacking {} failed: {}",
            asset.label,
            String::from_utf8_lossy(&out.stderr).trim().chars().take(200).collect::<String>()
        ));
    }
    std::fs::rename(&staging, &dest).map_err(|e| format!("install {}: {e}", dest.display()))?;
    Ok(())
}

/// Fetch everything this platform needs and return the runtime.
pub async fn install(root: &Path, report: &(dyn Fn(Status) + Sync)) -> Result<Runtime, String> {
    let plan = plan().ok_or_else(|| {
        "this platform has no ready-made signal-cli build. Install signal-cli yourself (https://github.com/AsamK/signal-cli) or set SIGNAL_CLI_BIN.".to_string()
    })?;
    if let Some(jre) = plan.jre {
        fetch(root, jre, report).await?;
    }
    fetch(root, plan.signal, report).await?;
    installed(root).ok_or_else(|| "the downloaded signal-cli is missing its launcher.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_launcher_and_java_home_in_extracted_layout() {
        let Some(plan) = plan() else { return };
        let root = std::env::temp_dir().join(format!("vicuna-sig-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(installed(&root).is_none());
        let bin = root.join(plan.signal.dir).join("signal-cli-0.14.10").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(launcher_name()), "").unwrap();
        if let Some(jre) = plan.jre {
            assert!(installed(&root).is_none(), "JVM builds need the JRE too");
            let jbin = root.join(jre.dir).join("jdk-25-jre").join("Contents").join("Home").join("bin");
            std::fs::create_dir_all(&jbin).unwrap();
            std::fs::write(jbin.join(java_name()), "").unwrap();
        }
        let rt = installed(&root).expect("installed runtime");
        assert!(rt.bin.ends_with(Path::new("bin").join(launcher_name())));
        assert_eq!(rt.java_home.is_some(), plan.jre.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn describes_progress() {
        let s = Status::Downloading { label: "Java runtime", done: 23_000_000, total: 56_000_000 };
        assert!(s.describe().unwrap().contains("23 of 56 MB"));
        assert!(Status::Ready.describe().is_none());
        assert!(Status::Failed("x.".into()).describe().unwrap().contains("retry"));
    }
}
