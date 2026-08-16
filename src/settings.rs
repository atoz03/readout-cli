//! 持久化用户设置。
//!
//! 配置只保存 readout 自己的设备身份、显示偏好和 SSH 别名；不保存密码、私钥，
//! 也不读取 Claude/Codex 的认证文件。SSH 连接细节继续由用户的 ssh config 管理。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const SCHEMA_VERSION: u32 = 1;
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
const MAX_SSH_HOSTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSettings {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAlias {
    /// transcript 中出现的完整 cwd，精确匹配，不猜测 basename。
    pub path: String,
    /// 跨 Linux/macOS/Windows 共享的显示名。
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub version: u32,
    pub device: DeviceSettings,
    /// 默认页面是否合并已导入的其他设备；关闭后只显示本机观察到的事件。
    #[serde(default = "yes")]
    pub aggregate_devices: bool,
    /// 用户从 SSH config 发现列表中启用的具体 Host 别名。
    #[serde(default)]
    pub ssh_hosts: Vec<String>,
    #[serde(default)]
    pub project_aliases: Vec<ProjectAlias>,
}

const fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        let name = default_device_name();
        Settings {
            version: SCHEMA_VERSION,
            device: DeviceSettings { id: new_device_id(&name), name },
            aggregate_devices: true,
            ssh_hosts: Vec::new(),
            project_aliases: Vec::new(),
        }
    }
}

impl Settings {
    /// 首次运行时创建配置，之后严格读取；损坏配置不会被静默覆盖。
    ///
    /// 覆盖会悄悄丢掉设备身份和已启用的 SSH Host，所以这里选择报错——但错误必须
    /// 说清是哪个文件，否则用户只会看到 `readout summary` 从此打不开。
    pub fn load_or_create() -> Result<Self> {
        let path = crate::paths::settings_file()?;
        if !path.exists() {
            let settings = Settings::default();
            settings.save_to(&path)?;
            return Ok(settings);
        }
        Self::load_from(&path).with_context(|| {
            format!("move or delete {} to start again from defaults", path.display())
        })
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let meta = std::fs::symlink_metadata(path)
            .with_context(|| format!("reading settings metadata {}", path.display()))?;
        anyhow::ensure!(meta.file_type().is_file(), "settings path is not a regular file");
        anyhow::ensure!(meta.len() <= MAX_SETTINGS_BYTES, "settings file is too large");
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let settings: Settings =
            serde_json::from_reader(BufReader::new(file)).context("parsing readout settings")?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&crate::paths::settings_file()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp = path.with_extension(format!("json.tmp.{}.{nonce}", std::process::id()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, self).context("serializing settings")?;
            writer.write_all(b"\n").context("finishing settings")?;
            writer.flush().with_context(|| format!("writing {}", tmp.display()))?;
            writer.get_ref().sync_all().with_context(|| format!("syncing {}", tmp.display()))?;
            std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.version == SCHEMA_VERSION, "unsupported settings schema");
        validate_device_id(&self.device.id)?;
        validate_label(&self.device.name, "device name")?;
        anyhow::ensure!(self.ssh_hosts.len() <= MAX_SSH_HOSTS, "too many SSH hosts");
        let mut hosts = HashSet::new();
        for host in &self.ssh_hosts {
            validate_ssh_alias(host)?;
            anyhow::ensure!(hosts.insert(host.as_str()), "duplicate SSH host");
        }
        let mut alias_paths = HashSet::new();
        for alias in &self.project_aliases {
            validate_project_path(&alias.path)?;
            validate_label(&alias.name, "project alias")?;
            anyhow::ensure!(
                alias_paths.insert(alias.path.as_str()),
                "duplicate project alias path"
            );
        }
        Ok(())
    }

    pub fn enable_ssh_host(&mut self, host: String) -> Result<bool> {
        validate_ssh_alias(&host)?;
        if self.ssh_hosts.iter().any(|item| item == &host) {
            return Ok(false);
        }
        anyhow::ensure!(self.ssh_hosts.len() < MAX_SSH_HOSTS, "too many SSH hosts");
        self.ssh_hosts.push(host);
        self.ssh_hosts.sort();
        Ok(true)
    }

    pub fn disable_ssh_host(&mut self, host: &str) -> bool {
        let before = self.ssh_hosts.len();
        self.ssh_hosts.retain(|item| item != host);
        self.ssh_hosts.len() != before
    }

    pub fn ssh_host_enabled(&self, host: &str) -> bool {
        self.ssh_hosts.iter().any(|item| item == host)
    }

    pub fn has_ssh_hosts(&self) -> bool {
        !self.ssh_hosts.is_empty()
    }

    pub fn set_project_alias(&mut self, path: String, name: String) -> Result<()> {
        validate_project_path(&path)?;
        validate_label(&name, "project alias")?;
        if let Some(alias) = self.project_aliases.iter_mut().find(|alias| alias.path == path) {
            alias.name = name;
        } else {
            self.project_aliases.push(ProjectAlias { path, name });
            self.project_aliases.sort_by(|a, b| a.path.cmp(&b.path));
        }
        Ok(())
    }

    pub fn remove_project_alias(&mut self, path: &str) -> bool {
        let before = self.project_aliases.len();
        self.project_aliases.retain(|alias| alias.path != path);
        self.project_aliases.len() != before
    }

    pub fn project_name<'a>(&'a self, path: &'a str) -> &'a str {
        self.project_aliases
            .iter()
            .find(|alias| alias.path == path)
            .map_or(path, |alias| alias.name.as_str())
    }
}

pub fn validate_ssh_alias(host: &str) -> Result<()> {
    anyhow::ensure!(!host.is_empty() && host.len() <= 255, "SSH Host must be 1-255 bytes");
    anyhow::ensure!(!host.starts_with('-'), "SSH Host cannot start with a dash");
    anyhow::ensure!(
        host.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "SSH Host must be a concrete alias containing only letters, digits, dash, underscore, and dot"
    );
    Ok(())
}

pub(crate) fn validate_device_id(id: &str) -> Result<()> {
    anyhow::ensure!(!id.is_empty() && id.len() <= 96, "invalid device id length");
    anyhow::ensure!(
        id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "invalid device id"
    );
    Ok(())
}

pub(crate) fn validate_label(value: &str, field: &str) -> Result<()> {
    anyhow::ensure!(!value.is_empty() && value.len() <= 128, "invalid {field} length");
    anyhow::ensure!(
        crate::fmt::terminal_text(value) == value,
        "{field} contains unsafe terminal characters"
    );
    Ok(())
}

fn validate_project_path(value: &str) -> Result<()> {
    anyhow::ensure!(!value.is_empty() && value.len() <= 4096, "invalid project path length");
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "project path contains control characters"
    );
    Ok(())
}

fn default_device_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| validate_label(value, "device name").is_ok())
        .unwrap_or_else(|| "this-device".into())
}

fn new_device_id(name: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"readout-device-v1");
    hasher.update(&now.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    format!("dev-{}", &digest.to_hex()[..24])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("readout-settings-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn settings_round_trip_and_remain_private() {
        let path = temp_path("roundtrip");
        let settings = Settings::default();
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn only_concrete_safe_ssh_aliases_can_be_enabled() {
        assert!(validate_ssh_alias("workstation").is_ok());
        assert!(validate_ssh_alias("gpu-01.example").is_ok());
        assert!(validate_ssh_alias("*").is_err());
        assert!(validate_ssh_alias("gpu-*").is_err());
        assert!(validate_ssh_alias("-oProxyCommand").is_err());
        assert!(validate_ssh_alias("host command").is_err());
    }

    #[test]
    fn enabling_a_host_is_idempotent_and_disabling_removes_it() {
        let mut settings = Settings::default();
        assert!(settings.enable_ssh_host("workstation".into()).unwrap());
        assert!(!settings.enable_ssh_host("workstation".into()).unwrap());
        assert!(settings.ssh_host_enabled("workstation"));
        assert!(settings.disable_ssh_host("workstation"));
        assert!(!settings.ssh_host_enabled("workstation"));
    }
}
