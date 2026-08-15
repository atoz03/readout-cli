//! 多设备 usage bundle 与 SSH 拉取。
//!
//! Bundle 只包含计费元数据，不包含 prompt、消息、工具参数或结果。远端 session 的
//! Replay 因此保持不可用；中心设备只聚合 usage，并按稳定事件 ID 去重。

use crate::model::{Source, Tokens, UsageEvent};
use crate::scan::{self, Progress, ScanResult};
use crate::settings::{DeviceSettings, Settings};
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const BUNDLE_SCHEMA_VERSION: u32 = 1;
pub const MAX_BUNDLE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BUNDLE_EVENTS: usize = 1_000_000;
const MAX_EVENT_STRING_BYTES: usize = 16 * 1024;
const MAX_EVENT_TEXT_BYTES: usize = 128 * 1024 * 1024;
const MAX_MERGED_EVENTS: usize = 2_000_000;
const MAX_MERGED_EVENT_TEXT_BYTES: usize = 256 * 1024 * 1024;
const MAX_SSH_ERROR_BYTES: usize = 64 * 1024;
const SSH_TIMEOUT: Duration = Duration::from_secs(45);
const SSH_UPDATE_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_SSH_CONFIG_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SSH_CONFIG_TOTAL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SSH_CONFIG_FILES: usize = 64;
const MAX_SSH_CONFIG_DEPTH: usize = 8;
const MAX_SSH_WORKERS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleEvent {
    pub id: String,
    pub source: Source,
    pub ts: i64,
    pub model: String,
    pub session: String,
    pub project: String,
    pub tokens: Tokens,
    #[serde(default)]
    pub dedup_rank: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageBundle {
    pub version: u32,
    pub exporter_version: String,
    pub generated_at: i64,
    pub device: DeviceSettings,
    pub events: Vec<BundleEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    /// `~/.ssh/config` 中用于连接此设备的 Host 别名；本机为 None。
    pub host: Option<String>,
    pub exporter_version: Option<String>,
    pub generated_at: i64,
    pub is_local: bool,
    pub available: bool,
    pub enabled: bool,
    pub discovered: bool,
}

#[derive(Debug)]
pub struct LoadedUsage {
    pub scan: ScanResult,
    pub devices: Vec<DeviceRecord>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub synced: Vec<String>,
    pub failed: Vec<String>,
}

/// 懒加载用户 SSH config，只返回可以直接传给 `ssh` 的具体 Host 别名。
/// HostName、User、IdentityFile、ProxyCommand 等连接细节从不进入 readout 配置。
pub fn discover_ssh_hosts() -> Result<Vec<String>> {
    let path = match std::env::var("READOUT_SSH_CONFIG") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            let Some(home) = dirs::home_dir() else { return Ok(Vec::new()) };
            home.join(".ssh").join("config")
        }
    };
    if !path.exists() {
        return Ok(Vec::new());
    }
    let base = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let mut state = SshDiscovery::default();
    state.read_file(&path, &base, 0)?;
    let mut hosts: Vec<_> = state.hosts.into_iter().collect();
    hosts.sort();
    Ok(hosts)
}

#[derive(Default)]
struct SshDiscovery {
    hosts: HashSet<String>,
    files: HashSet<PathBuf>,
    total_bytes: u64,
}

impl SshDiscovery {
    fn read_file(&mut self, path: &Path, base: &Path, depth: usize) -> Result<()> {
        anyhow::ensure!(depth <= MAX_SSH_CONFIG_DEPTH, "SSH config Include nesting is too deep");
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving SSH config {}", path.display()))?;
        if !self.files.insert(canonical.clone()) {
            return Ok(());
        }
        anyhow::ensure!(self.files.len() <= MAX_SSH_CONFIG_FILES, "too many SSH config files");
        let metadata = std::fs::metadata(&canonical)
            .with_context(|| format!("reading SSH config metadata {}", canonical.display()))?;
        anyhow::ensure!(metadata.is_file(), "SSH config is not a regular file: {}", path.display());
        anyhow::ensure!(
            metadata.len() <= MAX_SSH_CONFIG_FILE_BYTES,
            "SSH config file is too large: {}",
            path.display()
        );
        self.total_bytes = self.total_bytes.saturating_add(metadata.len());
        anyhow::ensure!(
            self.total_bytes <= MAX_SSH_CONFIG_TOTAL_BYTES,
            "SSH config Include files exceed the total size limit"
        );
        let bytes = std::fs::read(&canonical)
            .with_context(|| format!("reading SSH config {}", canonical.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            anyhow::ensure!(line.len() <= 16 * 1024, "SSH config line is too long");
            let words = ssh_config_words(line);
            let Some((keyword, values)) = ssh_directive(&words) else { continue };
            if keyword.eq_ignore_ascii_case("host") {
                for host in values {
                    if crate::settings::validate_ssh_alias(host).is_ok() {
                        self.hosts.insert(host.to_string());
                    }
                }
            } else if keyword.eq_ignore_ascii_case("include") {
                for pattern in values {
                    for included in include_paths(pattern, base)? {
                        self.read_file(&included, base, depth + 1)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn ssh_directive(words: &[String]) -> Option<(&str, Vec<&str>)> {
    let first = words.first()?;
    if let Some((keyword, value)) = first.split_once('=') {
        let mut values = Vec::with_capacity(words.len());
        if !value.is_empty() {
            values.push(value);
        }
        values.extend(words[1..].iter().map(String::as_str));
        Some((keyword, values))
    } else {
        Some((first, words[1..].iter().map(String::as_str).collect()))
    }
}

/// OpenSSH 的引号规则只用于切分 Host/Include；其他配置内容不会被保存或解释。
fn ssh_config_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else {
                word.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '#' => break,
            ch if ch.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(ch),
        }
    }
    if escaped {
        word.push('\\');
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn include_paths(pattern: &str, base: &Path) -> Result<Vec<PathBuf>> {
    let expanded = if pattern == "~" {
        dirs::home_dir().unwrap_or_else(|| base.to_path_buf())
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_else(|| base.to_path_buf()).join(rest)
    } else {
        let path = PathBuf::from(pattern);
        if path.is_absolute() { path } else { base.join(path) }
    };
    let Some(pattern) = expanded.to_str() else { return Ok(Vec::new()) };
    let mut paths = Vec::new();
    for entry in glob::glob(pattern).context("parsing SSH Include pattern")? {
        let path = entry.context("expanding SSH Include pattern")?;
        if path.is_file() {
            paths.push(path);
            anyhow::ensure!(
                paths.len() <= MAX_SSH_CONFIG_FILES,
                "SSH Include matched too many files"
            );
        }
    }
    paths.sort();
    Ok(paths)
}

impl UsageBundle {
    pub fn from_events(device: DeviceSettings, events: &[UsageEvent]) -> Self {
        let mut fallback_counts: HashMap<String, u64> = HashMap::new();
        let events = events
            .iter()
            .map(|event| {
                let id = canonical_event_id(event, &mut fallback_counts);
                BundleEvent {
                    id,
                    source: event.source,
                    ts: event.ts,
                    model: event.model.clone(),
                    session: event.session.clone(),
                    project: event.project.clone(),
                    tokens: event.tokens,
                    dedup_rank: event.dedup_rank,
                }
            })
            .collect();
        UsageBundle {
            version: BUNDLE_SCHEMA_VERSION,
            exporter_version: env!("CARGO_PKG_VERSION").into(),
            generated_at: chrono::Utc::now().timestamp(),
            device,
            events,
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let meta = std::fs::symlink_metadata(path)
            .with_context(|| format!("reading bundle metadata {}", path.display()))?;
        anyhow::ensure!(meta.file_type().is_file(), "bundle path is not a regular file");
        anyhow::ensure!(meta.len() <= MAX_BUNDLE_BYTES, "bundle exceeds size limit");
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let bundle: UsageBundle =
            serde_json::from_reader(BufReader::new(file)).context("parsing usage bundle")?;
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        anyhow::ensure!(bytes.len() as u64 <= MAX_BUNDLE_BYTES, "bundle exceeds size limit");
        let bundle: UsageBundle = serde_json::from_slice(bytes).context("parsing usage bundle")?;
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn write_json(&self, mut writer: impl Write) -> Result<()> {
        self.validate()?;
        serde_json::to_writer(&mut writer, self).context("serializing usage bundle")?;
        writer.write_all(b"\n").context("finishing usage bundle")?;
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
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
            self.write_json(&mut writer)?;
            writer.flush().with_context(|| format!("writing {}", tmp.display()))?;
            anyhow::ensure!(
                writer.get_ref().metadata()?.len() <= MAX_BUNDLE_BYTES,
                "bundle exceeds size limit"
            );
            writer.get_ref().sync_all().with_context(|| format!("syncing {}", tmp.display()))?;
            std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.version == BUNDLE_SCHEMA_VERSION, "unsupported usage bundle schema");
        crate::settings::validate_device_id(&self.device.id)?;
        crate::settings::validate_label(&self.device.name, "device name")?;
        validate_bounded_text(&self.exporter_version, "exporter version")?;
        anyhow::ensure!(self.events.len() <= MAX_BUNDLE_EVENTS, "bundle has too many events");
        let mut text_bytes = self.device.id.len().saturating_add(self.device.name.len());
        for event in &self.events {
            validate_bounded_text(&event.id, "event id")?;
            validate_bounded_text(&event.model, "model")?;
            validate_bounded_text(&event.session, "session")?;
            validate_bounded_text(&event.project, "project")?;
            text_bytes = text_bytes
                .saturating_add(event.id.len())
                .saturating_add(event.model.len())
                .saturating_add(event.session.len())
                .saturating_add(event.project.len());
            anyhow::ensure!(text_bytes <= MAX_EVENT_TEXT_BYTES, "bundle text exceeds size limit");
        }
        Ok(())
    }
}

fn validate_bounded_text(value: &str, field: &str) -> Result<()> {
    anyhow::ensure!(!value.is_empty(), "{field} cannot be empty");
    anyhow::ensure!(value.len() <= MAX_EVENT_STRING_BYTES, "{field} is too long");
    anyhow::ensure!(
        crate::fmt::terminal_text(value) == value,
        "{field} contains unsafe terminal characters"
    );
    Ok(())
}

/// 扫描本机并加载所有启用 remote 的最近 bundle；网络同步由单独后台任务触发。
pub fn load_usage(
    sources: &[Source],
    use_cache: bool,
    settings: &Settings,
    on_progress: Option<&(dyn Fn(Progress) + Sync)>,
) -> Result<LoadedUsage> {
    let mut scan = scan::scan_with_cache(sources, use_cache, on_progress)?;
    let mut merger = UsageMerger::default();
    let mut local_events = std::mem::take(&mut scan.events);
    apply_project_aliases(&mut local_events, settings);
    merger.add_batch(settings.device.clone(), local_events)?;

    let mut devices = vec![DeviceRecord {
        id: settings.device.id.clone(),
        name: settings.device.name.clone(),
        host: None,
        exporter_version: Some(env!("CARGO_PKG_VERSION").into()),
        generated_at: chrono::Utc::now().timestamp(),
        is_local: true,
        available: true,
        enabled: true,
        discovered: true,
    }];
    for host in &settings.ssh_hosts {
        let path = snapshot_path(host)?;
        if !path.exists() {
            devices.push(DeviceRecord {
                id: format!("ssh:{host}"),
                name: host.clone(),
                host: Some(host.clone()),
                exporter_version: None,
                generated_at: 0,
                is_local: false,
                available: false,
                enabled: true,
                discovered: false,
            });
            continue;
        }
        let bundle = UsageBundle::read(&path)
            .with_context(|| format!("loading snapshot for SSH Host `{host}`"))?;
        anyhow::ensure!(
            bundle.device.id != settings.device.id,
            "SSH Host `{host}` reports the local device id"
        );
        let mut remote_events: Vec<_> = bundle
            .events
            .into_iter()
            .filter(|event| sources.contains(&event.source))
            .map(bundle_event)
            .collect();
        apply_project_aliases(&mut remote_events, settings);
        devices.push(DeviceRecord {
            id: bundle.device.id.clone(),
            name: bundle.device.name.clone(),
            host: Some(host.clone()),
            exporter_version: Some(bundle.exporter_version.clone()),
            generated_at: bundle.generated_at,
            is_local: false,
            available: true,
            enabled: true,
            discovered: false,
        });
        merger.add_batch(bundle.device, remote_events)?;
    }

    scan.events = merger.finish();
    // ScanStats 描述本机 transcript 扫描（文件、字节和耗时）。跨设备总量与去重结果
    // 由 Summary 表达；混在这里会让 events 与本机文件统计失去一致语义。
    devices.sort_by(|a, b| b.is_local.cmp(&a.is_local).then_with(|| a.name.cmp(&b.name)));
    Ok(LoadedUsage { scan, devices })
}

fn bundle_event(event: BundleEvent) -> UsageEvent {
    UsageEvent {
        source: event.source,
        ts: event.ts,
        model: event.model,
        session: event.session,
        project: event.project,
        tokens: event.tokens,
        observed_on: Vec::new(),
        dedup_key: Some(event.id),
        dedup_rank: event.dedup_rank,
    }
}

#[cfg(test)]
fn merge_batches(batches: Vec<(DeviceSettings, Vec<UsageEvent>)>) -> Vec<UsageEvent> {
    let mut merger = UsageMerger::default();
    for (device, events) in batches {
        merger.add_batch(device, events).expect("test batches stay inside merge limits");
    }
    merger.finish()
}

#[derive(Default)]
struct UsageMerger {
    events: Vec<UsageEvent>,
    index: HashMap<String, usize>,
    text_bytes: usize,
}

impl UsageMerger {
    fn add_batch(&mut self, device: DeviceSettings, events: Vec<UsageEvent>) -> Result<()> {
        let mut fallback_counts = HashMap::new();
        for mut event in events {
            let key = canonical_event_id(&event, &mut fallback_counts);
            event.dedup_key = Some(key.clone());
            event.observed_on = vec![device.id.clone()];
            if let Some(&slot) = self.index.get(&key) {
                let before = event_text_bytes(&self.events[slot]);
                {
                    let previous = &mut self.events[slot];
                    if !previous.observed_on.contains(&device.id) {
                        previous.observed_on.push(device.id.clone());
                        previous.observed_on.sort();
                    }
                    if (event.dedup_rank, event.tokens.output)
                        > (previous.dedup_rank, previous.tokens.output)
                    {
                        event.observed_on = std::mem::take(&mut previous.observed_on);
                        *previous = event;
                    }
                }
                self.text_bytes = self
                    .text_bytes
                    .saturating_sub(before)
                    .saturating_add(event_text_bytes(&self.events[slot]));
            } else {
                anyhow::ensure!(
                    self.events.len() < MAX_MERGED_EVENTS,
                    "merged device usage exceeds the event limit"
                );
                self.text_bytes = self.text_bytes.saturating_add(event_text_bytes(&event));
                self.index.insert(key, self.events.len());
                self.events.push(event);
            }
            anyhow::ensure!(
                self.text_bytes <= MAX_MERGED_EVENT_TEXT_BYTES,
                "merged device usage exceeds the text limit"
            );
        }
        Ok(())
    }

    fn finish(self) -> Vec<UsageEvent> {
        self.events
    }
}

fn event_text_bytes(event: &UsageEvent) -> usize {
    event
        .model
        .len()
        .saturating_add(event.session.len())
        .saturating_add(event.project.len())
        .saturating_add(event.observed_on.iter().map(String::len).sum::<usize>())
        .saturating_add(event.dedup_key.as_ref().map_or(0, String::len))
}

fn apply_project_aliases(events: &mut [UsageEvent], settings: &Settings) {
    for event in events {
        let mapped = settings.project_name(&event.project);
        if mapped != event.project {
            event.project = mapped.to_string();
        }
    }
}

fn canonical_event_id(event: &UsageEvent, fallback_counts: &mut HashMap<String, u64>) -> String {
    if let Some(key) = event.dedup_key.as_deref() {
        return match event.source {
            Source::Claude if !key.starts_with("claude:") => format!("claude:v1:{key}"),
            _ => key.to_string(),
        };
    }

    let mut base = blake3::Hasher::new();
    base.update(b"readout:keyless-usage:v1");
    base.update(event.source.short().as_bytes());
    base.update(&event.ts.to_le_bytes());
    for value in [&event.model, &event.session, &event.project] {
        base.update(&(value.len() as u64).to_le_bytes());
        base.update(value.as_bytes());
    }
    for value in [
        event.tokens.input,
        event.tokens.output,
        event.tokens.cache_read,
        event.tokens.cache_write_5m,
        event.tokens.cache_write_1h,
    ] {
        base.update(&value.to_le_bytes());
    }
    let fingerprint = base.finalize().to_hex().to_string();
    let occurrence = fallback_counts.entry(fingerprint.clone()).or_default();
    let id = format!("fallback:v1:{fingerprint}:{occurrence}");
    *occurrence = occurrence.saturating_add(1);
    id
}

pub fn export_local(
    sources: &[Source],
    use_cache: bool,
    settings: &Settings,
) -> Result<UsageBundle> {
    let mut result = scan::scan_with_cache(sources, use_cache, None)?;
    apply_project_aliases(&mut result.events, settings);
    Ok(UsageBundle::from_events(settings.device.clone(), &result.events))
}

pub fn sync_all(settings: &Settings, only: Option<&str>) -> Result<SyncReport> {
    if let Some(host) = only {
        anyhow::ensure!(settings.ssh_host_enabled(host), "SSH Host `{host}` is not enabled");
    }
    let hosts: Vec<_> = settings
        .ssh_hosts
        .iter()
        .filter(|host| only.is_none_or(|name| host.as_str() == name))
        .collect();
    anyhow::ensure!(!hosts.is_empty(), "no SSH devices are enabled");
    let discovered: HashSet<_> = discover_ssh_hosts()?.into_iter().collect();

    let results: Vec<_> = ssh_pool().install(|| {
        hosts
            .par_iter()
            .map(|host| {
                let result = if discovered.contains(host.as_str()) {
                    fetch_and_save(settings, host)
                } else {
                    Err(anyhow::anyhow!("SSH Host is no longer present in ~/.ssh/config"))
                };
                ((*host).clone(), result)
            })
            .collect()
    });
    let mut report = SyncReport::default();
    for (host, result) in results {
        match result {
            Ok(()) => report.synced.push(host),
            Err(error) => report
                .failed
                .push(format!("{host}: {}", crate::fmt::terminal_text(&format!("{error:#}")))),
        }
    }
    Ok(report)
}

fn ssh_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let available = std::thread::available_parallelism().map_or(1, usize::from);
        rayon::ThreadPoolBuilder::new()
            .num_threads(available.min(MAX_SSH_WORKERS))
            .build()
            .expect("building the bounded SSH sync pool")
    })
}

/// 首次连接只在远端成功导出兼容 bundle 后保存快照；是否启用由调用方随后持久化。
pub fn sync_host(settings: &Settings, host: &str) -> Result<SyncReport> {
    crate::settings::validate_ssh_alias(host)?;
    ensure_ssh_host_discovered(host)?;
    fetch_and_save(settings, host).with_context(|| format!("validating SSH Host `{host}`"))?;
    Ok(SyncReport { synced: vec![host.to_string()], failed: Vec::new() })
}

fn fetch_and_save(settings: &Settings, host: &str) -> Result<()> {
    let bundle = fetch_remote(host).with_context(|| format!("syncing SSH Host `{host}`"))?;
    anyhow::ensure!(
        bundle.device.id != settings.device.id,
        "SSH Host `{host}` reports the local device id"
    );
    bundle.save(&snapshot_path(host)?)?;
    Ok(())
}

/// 远端升级是用户在 Devices 页明确选择的操作；升级后调用方会再次执行协议握手。
pub fn update_remote(host: &str) -> Result<()> {
    crate::settings::validate_ssh_alias(host)?;
    ensure_ssh_host_discovered(host)?;
    run_ssh(host, "readout update", MAX_SSH_ERROR_BYTES, SSH_UPDATE_TIMEOUT)
        .with_context(|| {
            format!(
                "updating `{host}`; older releases without `readout update` need one manual installer run"
            )
        })?;
    Ok(())
}

fn ensure_ssh_host_discovered(host: &str) -> Result<()> {
    anyhow::ensure!(
        discover_ssh_hosts()?.iter().any(|item| item == host),
        "SSH Host `{host}` is not present in ~/.ssh/config"
    );
    Ok(())
}

fn snapshot_path(host: &str) -> Result<PathBuf> {
    crate::settings::validate_ssh_alias(host)?;
    let digest = blake3::hash(host.as_bytes()).to_hex();
    Ok(crate::paths::remote_bundles_dir()?.join(format!("{host}-{}.json", &digest[..12])))
}

fn fetch_remote(host: &str) -> Result<UsageBundle> {
    crate::settings::validate_ssh_alias(host)?;
    let stdout = run_ssh(host, "readout export", MAX_BUNDLE_BYTES as usize, SSH_TIMEOUT)
        .with_context(|| {
            format!(
                "remote readout on `{host}` is missing or incompatible; install the current release once, or run `readout update` there"
            )
        })?;
    UsageBundle::from_slice(&stdout).with_context(|| {
        format!("`{host}` returned an incompatible usage bundle; update readout on that device")
    })
}

/// 统一的有界 SSH 执行器，避免同步和升级各自维护一套超时、排空与输出限制。
fn run_ssh(
    host: &str,
    remote_command: &str,
    output_limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut child = Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg("--")
        .arg(host)
        .arg(remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ssh")?;

    let stdout = child.stdout.take().context("capturing ssh stdout")?;
    let stderr = child.stderr.take().context("capturing ssh stderr")?;
    let (overflow_tx, overflow_rx) = mpsc::channel();
    let out_thread =
        std::thread::spawn(move || read_capped(stdout, output_limit, Some(overflow_tx)));
    let err_thread = std::thread::spawn(move || read_capped(stderr, MAX_SSH_ERROR_BYTES, None));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if overflow_rx.try_recv().is_ok() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            anyhow::bail!("remote command output exceeds the {output_limit} byte limit");
        }
        if let Some(status) = child.try_wait().context("waiting for ssh")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            anyhow::bail!("ssh command timed out after {} seconds", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out_thread.join().map_err(|_| anyhow::anyhow!("ssh stdout reader stopped"))??;
    let stderr = err_thread.join().map_err(|_| anyhow::anyhow!("ssh stderr reader stopped"))??;
    if !status.success() {
        let detail = crate::fmt::terminal_text(String::from_utf8_lossy(&stderr).trim());
        if detail.is_empty() {
            anyhow::bail!("ssh exited with {status}");
        }
        anyhow::bail!("ssh exited with {status}: {detail}");
    }
    Ok(stdout)
}

/// 持续排空管道但只保留有限字节，避免远端输出控制内存或堵住子进程。
fn read_capped(
    mut reader: impl Read,
    limit: usize,
    overflow: Option<mpsc::Sender<()>>,
) -> Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut reported = false;
    loop {
        let read = reader.read(&mut buf).context("reading ssh output")?;
        if read == 0 {
            break;
        }
        let room = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..read.min(room)]);
        if read > room && !reported {
            if let Some(sender) = overflow.as_ref() {
                let _ = sender.send(());
            }
            reported = true;
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, output: u64) -> UsageEvent {
        UsageEvent {
            source: Source::Codex,
            ts: 1_000,
            model: "gpt-test".into(),
            session: "session-1".into(),
            project: "/work/project".into(),
            tokens: Tokens { output, ..Default::default() },
            observed_on: Vec::new(),
            dedup_key: Some(id.into()),
            dedup_rank: 0,
        }
    }

    fn device(id: &str, name: &str) -> DeviceSettings {
        DeviceSettings { id: id.into(), name: name.into() }
    }

    #[test]
    fn copied_events_count_once_and_keep_both_observers() {
        let events = merge_batches(vec![
            (device("dev-a", "A"), vec![event("codex:v1:same", 10)]),
            (device("dev-b", "B"), vec![event("codex:v1:same", 10)]),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.output, 10);
        assert_eq!(events[0].observed_on, vec!["dev-a", "dev-b"]);
    }

    #[test]
    fn bundle_round_trip_excludes_replay_content() {
        let bundle = UsageBundle::from_events(device("dev-a", "A"), &[event("codex:v1:a", 10)]);
        let mut bytes = Vec::new();
        bundle.write_json(&mut bytes).unwrap();
        let back = UsageBundle::from_slice(&bytes).unwrap();
        assert_eq!(back.events.len(), 1);
        assert_eq!(back.events[0].session, "session-1");
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("detail"));
        assert!(!text.contains("tool"));
    }

    #[test]
    fn oversized_or_control_character_fields_are_rejected() {
        let mut bundle = UsageBundle::from_events(device("dev-a", "A"), &[event("id", 1)]);
        bundle.events[0].project = "bad\nproject".into();
        assert!(bundle.validate().is_err());
        assert!(UsageBundle::from_slice(&vec![b' '; MAX_BUNDLE_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn remote_bundle_cannot_impersonate_the_shared_bucket() {
        let mut bundle = UsageBundle::from_events(device("dev-a", "A"), &[event("id", 1)]);
        bundle.device.id = crate::agg::SHARED_DEVICE_ID.into();
        assert!(bundle.validate().is_err());
        bundle.device.id = "dev-a".into();
        bundle.device.name = "safe\u{202e}spoof".into();
        assert!(bundle.validate().is_err());
    }

    #[test]
    fn capped_reader_drains_but_never_keeps_more_than_the_limit() {
        let bytes = vec![7u8; 10_000];
        let kept = read_capped(bytes.as_slice(), 100, None).unwrap();
        assert_eq!(kept.len(), 100);
    }

    #[test]
    fn project_aliases_unify_cross_platform_working_directories() {
        let mut settings = Settings::default();
        settings.set_project_alias("/work/readout".into(), "readout-cli".into()).unwrap();
        settings.set_project_alias(r"C:\work\readout".into(), "readout-cli".into()).unwrap();
        let mut events = vec![event("a", 1), event("b", 1)];
        events[0].project = "/work/readout".into();
        events[1].project = r"C:\work\readout".into();
        apply_project_aliases(&mut events, &settings);
        assert!(events.iter().all(|event| event.project == "readout-cli"));
    }

    #[test]
    fn ssh_config_discovery_keeps_only_concrete_hosts_and_follows_includes() {
        let root = std::env::temp_dir().join(format!(
            "readout-ssh-config-{}-{}",
            std::process::id(),
            line!()
        ));
        let included = root.join("config.d");
        std::fs::create_dir_all(&included).unwrap();
        std::fs::write(
            root.join("config"),
            "Host workstation gpu-01\nHost *.example !blocked\nInclude config.d/*.conf\n",
        )
        .unwrap();
        std::fs::write(included.join("more.conf"), "Host=windows-box\n  HostName 192.0.2.1\n")
            .unwrap();

        let mut state = SshDiscovery::default();
        state.read_file(&root.join("config"), &root, 0).unwrap();
        let mut hosts: Vec<_> = state.hosts.into_iter().collect();
        hosts.sort();
        assert_eq!(hosts, vec!["gpu-01", "windows-box", "workstation"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ssh_config_tokenizer_handles_quotes_escapes_and_comments() {
        assert_eq!(
            ssh_config_words(r#"Host "gpu-01" work\ station # ignored"#),
            vec!["Host", "gpu-01", "work station"]
        );
        assert_eq!(
            ssh_directive(&ssh_config_words("Include=conf.d/*.conf")).unwrap(),
            ("Include", vec!["conf.d/*.conf"])
        );
    }
}
