//! 轻量自更新入口。
//!
//! 下载、平台识别与 SHA-256 校验继续由 release 随附的安装器负责，避免在二进制里
//! 再维护一套 HTTP、归档和校验实现。安装目录固定为当前可执行文件所在目录，所以在
//! 开发检出里跑 `cargo run -- update` 会把 release 二进制装进 `target/debug/`——这是
//! 「更新我正在运行的这一个」的直接后果，不是要修的 bug。

use anyhow::{Context, Result};
use std::process::{Command, Stdio};

#[cfg(any(unix, test))]
const UNIX_INSTALLER: &str =
    "https://github.com/atoz03/readout-cli/releases/latest/download/install.sh";
#[cfg(any(windows, test))]
const WINDOWS_INSTALLER: &str =
    "https://github.com/atoz03/readout-cli/releases/latest/download/install.ps1";

pub fn update() -> Result<()> {
    let executable = std::env::current_exe().context("locating the current readout executable")?;
    let install_dir =
        executable.parent().context("the current readout executable has no parent directory")?;
    update_in(install_dir)
}

#[cfg(unix)]
fn update_in(install_dir: &std::path::Path) -> Result<()> {
    let script = format!(
        "tmp=$(mktemp \"${{TMPDIR:-/tmp}}/readout-update.XXXXXX\") || exit 1; \
         trap 'rm -f \"$tmp\"' EXIT HUP INT TERM; \
         if command -v curl >/dev/null 2>&1 && curl -fsSL '{UNIX_INSTALLER}' -o \"$tmp\"; \
         then :; \
         elif command -v wget >/dev/null 2>&1 && wget -qO \"$tmp\" '{UNIX_INSTALLER}'; \
         then :; \
         else echo 'readout update: could not download the installer with curl or wget' >&2; \
         exit 1; fi; \
         sh \"$tmp\""
    );
    let status = Command::new("sh")
        .args(["-c", &script])
        .env("READOUT_INSTALL_DIR", install_dir)
        .stdin(Stdio::null())
        .status()
        .context("starting the readout updater")?;
    anyhow::ensure!(status.success(), "readout update failed with {status}");
    Ok(())
}

#[cfg(windows)]
fn update_in(install_dir: &std::path::Path) -> Result<()> {
    // Windows 会锁住正在运行的 exe，因此让子进程等待本进程退出后再替换。
    // 子进程继承输出句柄，SSH 会等到安装器完成，而不会把半次升级当成成功。
    let pid = std::process::id();
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         [Net.ServicePointManager]::SecurityProtocol = \
           [Net.ServicePointManager]::SecurityProtocol -bor \
           [Net.SecurityProtocolType]::Tls12; \
         Wait-Process -Id {pid} -ErrorAction SilentlyContinue; \
         Invoke-RestMethod '{WINDOWS_INSTALLER}' -UseBasicParsing | Invoke-Expression"
    );
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("READOUT_INSTALL_DIR", install_dir)
        .stdin(Stdio::null())
        .spawn()
        .context("starting the deferred Windows updater")?;
    println!("Update scheduled; readout will be replaced after this process exits.");
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn update_in(_install_dir: &std::path::Path) -> Result<()> {
    anyhow::bail!("readout update is not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_urls_are_fixed_official_release_assets() {
        assert!(UNIX_INSTALLER.starts_with("https://github.com/atoz03/readout-cli/"));
        assert!(WINDOWS_INSTALLER.starts_with("https://github.com/atoz03/readout-cli/"));
        assert!(!UNIX_INSTALLER.contains(char::is_whitespace));
        assert!(!WINDOWS_INSTALLER.contains(char::is_whitespace));
    }
}
