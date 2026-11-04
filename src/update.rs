//! Noticing new releases and updating the way Scarlet was installed.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::{process::Command, time::timeout};

const LATEST: &str = "https://github.com/joakimunge/scarlet/releases/latest";
const INSTALL_SCRIPT: &str = "https://raw.githubusercontent.com/joakimunge/scarlet/main/install.sh";

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The newest release's version when it is newer than this one. Asks GitHub
/// where its "latest release" link leads, so there is no API limit to run into.
pub async fn newer_release() -> Option<String> {
    if std::env::var_os("SCARLET_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let output = Command::new("curl")
        .args(["-fsSL", "-o", "/dev/null", "-w", "%{url_effective}", "--max-time", "5", LATEST])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = timeout(Duration::from_secs(6), output).await.ok()?.ok()?;
    let url = String::from_utf8(output.stdout).ok()?;
    let latest = url.rsplit_once("/tag/v")?.1.trim().to_owned();
    is_newer(&latest, CURRENT).then_some(latest)
}

/// Compares dotted version numbers: 0.1.10 is newer than 0.1.9.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Option<Vec<u64>> { v.split('.').map(|p| p.parse().ok()).collect() };
    match (parts(candidate), parts(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// How this copy of Scarlet was installed, which decides how it updates.
pub enum Method {
    Homebrew { bin: PathBuf },
    Cargo { bin: PathBuf },
    Script { bin: PathBuf },
}

impl Method {
    pub fn detect() -> Method {
        let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).unwrap_or_default();
        Method::from_path(&exe)
    }

    fn from_path(exe: &Path) -> Method {
        let text = exe.to_string_lossy();
        if let Some((prefix, _)) = text.split_once("/Cellar/scarlet/") {
            return Method::Homebrew { bin: Path::new(prefix).join("bin").join("scarlet") };
        }
        if exe.parent().is_some_and(|dir| dir.ends_with(".cargo/bin")) {
            return Method::Cargo { bin: exe.to_path_buf() };
        }
        Method::Script { bin: exe.to_path_buf() }
    }

    /// The command that updates this copy, as a person would type it.
    pub fn command(&self) -> String {
        match self {
            Method::Homebrew { .. } => "brew upgrade scarlet".into(),
            Method::Cargo { .. } => "cargo install --git https://github.com/joakimunge/scarlet --locked".into(),
            // An update replaces the program where it is and leaves shell startup files alone.
            Method::Script { bin } => {
                let dir = bin.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
                let default = dirs::home_dir().map(|h| h.join(".local").join("bin"));
                if default.is_some_and(|d| d == Path::new(&dir)) {
                    format!("curl -fsSL {INSTALL_SCRIPT} | SCARLET_NO_MODIFY_PATH=1 sh")
                } else {
                    format!("curl -fsSL {INSTALL_SCRIPT} | SCARLET_NO_MODIFY_PATH=1 SCARLET_INSTALL_DIR='{dir}' sh")
                }
            }
        }
    }

    /// Where the updated program will be, to start it again.
    pub fn program(&self) -> &Path {
        match self {
            Method::Homebrew { bin } | Method::Cargo { bin } | Method::Script { bin } => bin,
        }
    }
}

/// The update command for this copy, worked out once.
pub fn command() -> &'static str {
    static COMMAND: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    COMMAND.get_or_init(|| Method::detect().command())
}

/// Runs the update in the terminal, with its output showing. Returns whether it worked.
pub fn run(method: &Method) -> bool {
    let command = method.command();
    println!("Updating Scarlet: {command}\n");
    let status = std::process::Command::new("sh").args(["-c", &command]).status();
    let ok = status.is_ok_and(|s| s.success());
    if !ok {
        println!("\nThe update didn't finish. Press enter to go back to Scarlet.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_by_number() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("0.2.0", "0.1.12"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1"));
    }

    #[test]
    fn updates_the_way_it_was_installed() {
        let brew = Method::from_path(Path::new("/opt/homebrew/Cellar/scarlet/0.1.1/bin/scarlet"));
        assert_eq!(brew.command(), "brew upgrade scarlet");
        assert_eq!(brew.program(), Path::new("/opt/homebrew/bin/scarlet"));
        let cargo = Method::from_path(Path::new("/home/me/.cargo/bin/scarlet"));
        assert!(cargo.command().starts_with("cargo install"));
        let script = Method::from_path(Path::new("/opt/tools/scarlet"));
        assert!(script.command().ends_with("SCARLET_NO_MODIFY_PATH=1 SCARLET_INSTALL_DIR='/opt/tools' sh"));
    }
}
