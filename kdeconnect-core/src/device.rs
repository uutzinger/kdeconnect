use ron::ser::PrettyConfig;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Display,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::{
    fs,
    io::AsyncReadExt,
    sync::{
        RwLock,
        mpsc::{self},
    },
};
use tracing::info;

use crate::{config::CONFIG_DIR, event::CoreEvent, transport::DEFAULT_LISTEN_PORT};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);

impl Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl DeviceId {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.0.is_empty()
                && self.0.len() <= 128
                && self
                    .0
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid device ID"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum DeviceState {
    Battery { level: u8, charging: bool },
    Connectivity((String, i32)),
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PairState {
    #[default]
    NotPaired,
    Requesting,
    Requested,
    Paired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Device {
    pub name: String,
    pub device_id: DeviceId,
    pub address: SocketAddr,
    pub pair_state: PairState,
    pub(crate) paired_certificate: Option<Vec<u8>>,
    #[serde(skip)]
    pub(crate) connection_certificate: Option<Vec<u8>>,
}

impl Default for Device {
    fn default() -> Self {
        Self {
            name: String::new(),
            device_id: DeviceId(String::new()),
            address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, DEFAULT_LISTEN_PORT)),
            pair_state: PairState::default(),
            paired_certificate: None,
            connection_certificate: None,
        }
    }
}

impl Device {
    pub async fn new(id: String, name: String, address: SocketAddr) -> anyhow::Result<Self> {
        Self::load_from(
            dirs::config_dir().unwrap().join(CONFIG_DIR),
            id,
            name,
            address,
        )
        .await
    }

    async fn load_from(
        config_dir: std::path::PathBuf,
        id: String,
        name: String,
        address: SocketAddr,
    ) -> anyhow::Result<Self> {
        let device_id = DeviceId(id);
        device_id.validate()?;
        let file_path = config_dir.join(format!("{}.ron", &device_id));

        if file_path.exists() {
            let mut buffer = String::new();
            let mut file = fs::File::open(&file_path).await?;
            file.read_to_string(&mut buffer).await?;

            match ron::de::from_str::<Self>(&buffer) {
                Ok(mut device) => {
                    anyhow::ensure!(device.device_id == device_id, "stored device ID mismatch");
                    device.name = name;
                    device.address = address;
                    // Old installations have no authenticated peer identity. Never
                    // silently trust the first certificate to reconnect after upgrade.
                    if device.pair_state != PairState::Paired
                        || device.paired_certificate.as_ref().is_none_or(Vec::is_empty)
                    {
                        device.pair_state = PairState::NotPaired;
                        device.paired_certificate = None;
                    }
                    return Ok(device);
                }
                Err(e) => {
                    tracing::warn!(
                        "[device] failed to deserialize {}: {}; rebuilding from network identity",
                        file_path.display(),
                        e
                    );
                    // Fall through — device will need to re-pair on next connection
                }
            }
        }

        Ok(Self {
            name,
            device_id,
            address,
            pair_state: PairState::NotPaired,
            paired_certificate: None,
            connection_certificate: None,
        })
    }

    pub(crate) fn bind_certificate(&mut self, certificate: Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(!certificate.is_empty(), "peer supplied no certificate");
        if self.pair_state == PairState::Paired {
            anyhow::ensure!(
                self.paired_certificate.as_ref() == Some(&certificate),
                "paired peer certificate changed; unpair locally before pairing again"
            );
        }
        self.connection_certificate = Some(certificate);
        Ok(())
    }

    pub(crate) fn payload_certificate(&self) -> anyhow::Result<&[u8]> {
        anyhow::ensure!(self.pair_state == PairState::Paired, "device is not paired");
        self.paired_certificate
            .as_deref()
            .filter(|cert| !cert.is_empty())
            .ok_or_else(|| anyhow::anyhow!("device has no pinned certificate"))
    }

    pub async fn store_device_identity(&self, pair_state: PairState) -> anyhow::Result<()> {
        self.store_in(dirs::config_dir().unwrap().join(CONFIG_DIR), pair_state)
            .await
    }

    async fn store_in(
        &self,
        config_dir: std::path::PathBuf,
        pair_state: PairState,
    ) -> anyhow::Result<()> {
        let mut data = self.clone();
        data.pair_state = pair_state;
        data.device_id.validate()?;
        if pair_state != PairState::Paired {
            data.paired_certificate = None;
        }
        let file_content = ron::ser::to_string_pretty(&data, PrettyConfig::new())?;
        let file_path = config_dir.join(format!("{}.ron", self.device_id));
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            use std::io::Write;
            std::fs::create_dir_all(&config_dir)?;
            let mut file = tempfile::NamedTempFile::new_in(config_dir)?;
            file.write_all(file_content.as_bytes())?;
            file.as_file().sync_all()?;
            file.persist(file_path)?;
            Ok(())
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_cannot_select_paths() {
        for id in ["", "../phone", "/tmp/phone", ".", "a/b", "a\\b"] {
            assert!(DeviceId(id.into()).validate().is_err());
        }
        assert!(DeviceId("77f4e7c7_3593-4fa5".into()).validate().is_ok());
    }

    #[tokio::test]
    async fn legacy_pairing_requires_new_approval() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("phone.ron"),
            r#"(name:"old",device_id:("phone"),address:"127.0.0.1:1716",pair_state:Paired)"#,
        )
        .unwrap();
        let current = "127.0.0.2:1716".parse().unwrap();
        let mut device =
            Device::load_from(dir.path().into(), "phone".into(), "new".into(), current)
                .await
                .unwrap();
        assert_eq!(device.pair_state, PairState::NotPaired);
        assert_eq!(device.address, current);
        assert_eq!(device.name, "new");
        device.bind_certificate(vec![1, 2, 3]).unwrap();
        assert!(device.payload_certificate().is_err());
        assert!(device.paired_certificate.is_none());
    }

    #[tokio::test]
    async fn pin_survives_restart_and_is_cleared_on_unpair() {
        let dir = tempfile::tempdir().unwrap();
        let device = Device {
            device_id: DeviceId("phone".into()),
            pair_state: PairState::Paired,
            paired_certificate: Some(vec![1, 2, 3]),
            connection_certificate: Some(vec![1, 2, 3]),
            ..Device::default()
        };
        device
            .store_in(dir.path().into(), PairState::Paired)
            .await
            .unwrap();
        let mut restored = Device::load_from(
            dir.path().into(),
            "phone".into(),
            "phone".into(),
            device.address,
        )
        .await
        .unwrap();
        assert_eq!(restored.pair_state, PairState::Paired);
        assert!(restored.connection_certificate.is_none());
        assert!(restored.bind_certificate(vec![9, 9, 9]).is_err());
        restored.bind_certificate(vec![1, 2, 3]).unwrap();
        assert_eq!(restored.payload_certificate().unwrap(), &[1, 2, 3]);
        restored
            .store_in(dir.path().into(), PairState::NotPaired)
            .await
            .unwrap();
        let unpaired = Device::load_from(
            dir.path().into(),
            "phone".into(),
            "phone".into(),
            device.address,
        )
        .await
        .unwrap();
        assert!(unpaired.paired_certificate.is_none());
        assert!(unpaired.payload_certificate().is_err());
    }

    #[tokio::test]
    async fn pending_approval_does_not_survive_reconnection() {
        let dir = tempfile::tempdir().unwrap();
        let device = Device {
            device_id: DeviceId("phone".into()),
            connection_certificate: Some(vec![1, 2, 3]),
            ..Device::default()
        };
        device
            .store_in(dir.path().into(), PairState::Requesting)
            .await
            .unwrap();
        let restored = Device::load_from(
            dir.path().into(),
            "phone".into(),
            "phone".into(),
            device.address,
        )
        .await
        .unwrap();
        assert_eq!(restored.pair_state, PairState::NotPaired);
        assert!(restored.paired_certificate.is_none());
        assert!(restored.connection_certificate.is_none());
    }
}

#[derive(Debug, Clone)]
pub struct DeviceManager {
    devices: Arc<RwLock<HashMap<DeviceId, Device>>>,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
}

impl DeviceManager {
    pub fn new(event_tx: mpsc::UnboundedSender<CoreEvent>) -> Self {
        Self {
            devices: Default::default(),
            event_tx,
        }
    }

    pub async fn add_or_update_device(&self, device_id: DeviceId, device: Device) {
        info!("updating: {}", device_id);
        let mut guard = self.devices.write().await;
        guard.entry(device_id).insert_entry(device.clone());
    }

    pub async fn get_device(&self, id: &DeviceId) -> Option<Device> {
        let guard = self.devices.read().await;
        guard.get(id).cloned()
    }

    pub async fn get_devices(&self) -> Vec<Device> {
        let guard = self.devices.read().await;
        guard.values().cloned().collect()
    }

    pub async fn set_paired(&self, id: &DeviceId, flag: bool) -> anyhow::Result<()> {
        id.validate()?;
        let mut guard = self.devices.write().await;

        if let Some(device) = guard.get_mut(id) {
            if flag {
                anyhow::ensure!(
                    matches!(
                        device.pair_state,
                        PairState::Requested | PairState::Requesting
                    ),
                    "no pending pairing request"
                );
                let certificate = device
                    .connection_certificate
                    .clone()
                    .filter(|cert| !cert.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("no authenticated connection certificate"))?;
                let mut paired = device.clone();
                paired.paired_certificate = Some(certificate);
                paired.store_device_identity(PairState::Paired).await?;
                device.paired_certificate = paired.paired_certificate;
                device.pair_state = PairState::Paired;
                let _ = self.event_tx.send(CoreEvent::DevicePaired((
                    device.device_id.clone(),
                    device.clone(),
                )));
            } else {
                device.store_device_identity(PairState::NotPaired).await?;
                device.pair_state = PairState::NotPaired;
                device.paired_certificate = None;
                let _ = self
                    .event_tx
                    .send(CoreEvent::DevicePairCancelled(device.device_id.clone()));
            }
        } else if flag {
            anyhow::bail!("cannot pair a device without a live connection");
        } else {
            // A changed certificate is rejected before the device is registered.
            // Local unpair must still revoke that persisted pin after a restart.
            let path = dirs::config_dir()
                .unwrap()
                .join(CONFIG_DIR)
                .join(format!("{}.ron", id));
            match fs::remove_file(path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let _ = self
                .event_tx
                .send(CoreEvent::DevicePairCancelled(id.clone()));
        }
        Ok(())
    }

    pub async fn update_pair_state(&self, id: &DeviceId, state: PairState) {
        let mut guard = self.devices.write().await;

        if let Some(device) = guard.get_mut(id) {
            if let Err(e) = device.store_device_identity(state).await {
                tracing::error!("failed to persist pairing state for {}: {}", id, e);
                return;
            }
            device.pair_state = state;
            if state != PairState::Paired {
                device.paired_certificate = None;
            }
            let _ = self
                .event_tx
                .send(CoreEvent::DevicePairStateChanged((id.clone(), state)));
        }
    }
}
