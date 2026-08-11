use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{config::Config, services::server_cmd};

use super::types::{GeoSettings, RuntimeGeo, RuntimeSnapshot, SETTINGS_VERSION};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppliedStatus {
    revision: u64,
}

const MAX_HANDOFF_BYTES: usize = 512 * 1024;
const HANDOFF_SIZE_RESERVE: usize = 64;
const TEMP_PREFIX: &str = ".geo-config-";
const TEMP_SUFFIX: &str = ".json";
const STALE_HANDOFF_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

static APPLY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn data_dir(config: &Config) -> Result<PathBuf, String> {
    let key_file = config.rustdesk.key_file.trim();
    if key_file.is_empty() {
        return Err("rustdesk.key-file is empty; Geo data directory is unavailable".to_owned());
    }
    let path = Path::new(key_file);
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| "rustdesk.key-file must include a parent directory".to_owned())
}

pub async fn apply(
    config: &Config,
    settings: &GeoSettings,
    relay_servers: &[String],
) -> Result<String, String> {
    let _guard = APPLY_LOCK.get_or_init(|| Mutex::new(())).lock().await;
    let directory = data_dir(config)?;
    let snapshot = RuntimeSnapshot {
        version: SETTINGS_VERSION,
        revision: settings.revision,
        relay_servers,
        geo: RuntimeGeo {
            enabled: settings.enabled,
            rules: &settings.rules,
        },
    };
    let encoded = serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_HANDOFF_BYTES {
        return Err(format!(
            "runtime configuration exceeds {MAX_HANDOFF_BYTES} bytes"
        ));
    }
    let file_name = format!("{TEMP_PREFIX}{}{TEMP_SUFFIX}", Uuid::new_v4().simple());
    let path = directory.join(&file_name);
    write_private_file(&path, &encoded)?;
    let _temporary = TemporaryHandoff::new(path);
    let checksum = format!("{:x}", Sha256::digest(&encoded));
    let argument = format!("{file_name} {} {checksum}", encoded.len());
    let result = server_cmd::send_target_cmd(
        config,
        entity::server_cmd::TARGET_ID_SERVER,
        "apply-geo-config",
        &argument,
    )
    .await;
    let response = result?;
    let success_prefix = format!("Geo config revision {} applied with ", settings.revision);
    if response.starts_with(&success_prefix) {
        return Ok(response);
    }
    if let Ok(raw_status) = server_cmd::send_target_cmd(
        config,
        entity::server_cmd::TARGET_ID_SERVER,
        "geo-status",
        "",
    )
    .await
    {
        if serde_json::from_str::<AppliedStatus>(raw_status.trim())
            .map(|status| status.revision == settings.revision)
            .unwrap_or(false)
        {
            return Ok(format!(
                "Geo config revision {} is already applied",
                settings.revision
            ));
        }
    }
    if response.trim().is_empty() {
        return Err("HBBS did not recognize the Geo configuration command".to_owned());
    }
    Err(response)
}

pub fn validate_size(settings: &GeoSettings, relay_servers: &[String]) -> Result<(), String> {
    let snapshot = RuntimeSnapshot {
        version: SETTINGS_VERSION,
        revision: settings.revision,
        relay_servers,
        geo: RuntimeGeo {
            enabled: settings.enabled,
            rules: &settings.rules,
        },
    };
    let encoded = serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_HANDOFF_BYTES.saturating_sub(HANDOFF_SIZE_RESERVE) {
        return Err(format!(
            "runtime configuration is too large; maximum is {} bytes",
            MAX_HANDOFF_BYTES - HANDOFF_SIZE_RESERVE
        ));
    }
    Ok(())
}

pub fn cleanup_stale(config: &Config) -> Result<usize, String> {
    let directory = data_dir(config)?;
    let entries = std::fs::read_dir(&directory)
        .map_err(|error| format!("cannot inspect Geo data directory: {error}"))?;
    let mut removed = 0;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !is_handoff_name(file_name) {
            continue;
        }
        let is_stale = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .map(|age| age >= STALE_HANDOFF_AGE)
            .unwrap_or(false);
        if !is_stale {
            continue;
        }
        std::fs::remove_file(entry.path())
            .map_err(|error| format!("cannot remove stale Geo handoff: {error}"))?;
        removed += 1;
    }
    Ok(removed)
}

fn is_handoff_name(value: &str) -> bool {
    value
        .strip_prefix(TEMP_PREFIX)
        .and_then(|value| value.strip_suffix(TEMP_SUFFIX))
        .map(|nonce| {
            !nonce.is_empty()
                && nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .unwrap_or(false)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot create Geo runtime handoff: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("cannot write Geo runtime handoff: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync Geo runtime handoff: {error}"))
}

struct TemporaryHandoff {
    path: PathBuf,
}

impl TemporaryHandoff {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TemporaryHandoff {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to remove a Geo runtime handoff file: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_names_are_strict() {
        assert!(is_handoff_name(".geo-config-abc123.json"));
        assert!(!is_handoff_name("../.geo-config-abc123.json"));
        assert!(!is_handoff_name(".geo-config-.json"));
        assert!(!is_handoff_name("GeoLite2-City.mmdb"));
    }

    #[test]
    fn temporary_handoff_is_removed_when_guard_drops() {
        let directory = std::env::temp_dir().join(format!(
            "rustdesk-console-handoff-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(".geo-config-test.json");
        write_private_file(&path, b"{}").unwrap();
        {
            let _guard = TemporaryHandoff::new(path.clone());
            assert!(path.is_file());
        }
        assert!(!path.exists());
        std::fs::remove_dir(&directory).unwrap();
    }
}
