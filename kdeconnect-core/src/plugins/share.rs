use std::{path::PathBuf, str::FromStr};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    device::Device,
    protocol::{PacketPayloadTransferInfo, PacketType, ProtocolPacket},
    transport::receive_payload,
};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ShareRequest {
    File(ShareRequestFile),
    Text { text: String },
    Url { url: String },
}

impl Default for ShareRequest {
    fn default() -> Self {
        ShareRequest::Text {
            text: "Hello From COSMIC Desktop".to_string(),
        }
    }
}

impl ShareRequest {
    pub async fn share_files(content: Vec<String>) -> anyhow::Result<Vec<(Self, String)>> {
        if content.is_empty() {
            return Ok(vec![]);
        }

        let mut requests = Vec::with_capacity(content.len());

        for file in content.iter() {
            let pathbuf = PathBuf::from_str(file)?;

            if !pathbuf.exists() {
                continue;
            }

            let filename = pathbuf
                .file_name()
                .unwrap()
                .to_str()
                .expect("OsString conversion")
                .to_string();

            let body = ShareRequestFile {
                filename,
                open: Some(false),
            };

            requests.push((Self::File(body), pathbuf.to_str().unwrap().to_string()));
        }

        Ok(requests)
    }

    pub async fn receive_share(
        &self,
        device: &Device,
        info: Option<&PacketPayloadTransferInfo>,
        expected_size: Option<u64>,
        transfer: Option<crate::filetransfer::IncomingTransfer>,
    ) -> anyhow::Result<()> {
        match self {
            ShareRequest::File(f) => {
                let info = info.ok_or_else(|| {
                    anyhow::anyhow!(
                        "[share] file share '{}' missing payload transfer info",
                        f.filename
                    )
                })?;
                self.handle_file_request(f, device, info, expected_size, transfer)
                    .await
            }
            ShareRequest::Text { text } => {
                let text = text.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = notify_rust::Notification::new()
                        .appname("KDE Connect")
                        .summary("Text received from phone")
                        .body(&text)
                        .show();
                })
                .await
                .ok();
                Ok(())
            }
            ShareRequest::Url { url } => {
                let url = url.clone();
                tokio::task::spawn_blocking(move || {
                    if std::process::Command::new("xdg-open")
                        .arg(&url)
                        .spawn()
                        .is_err()
                    {
                        let _ = notify_rust::Notification::new()
                            .appname("KDE Connect")
                            .summary("URL received from phone")
                            .body(&url)
                            .show();
                    }
                })
                .await
                .ok();
                Ok(())
            }
        }
    }

    async fn handle_file_request(
        &self,
        request: &ShareRequestFile,
        device: &Device,
        info: &PacketPayloadTransferInfo,
        expected_size: Option<u64>,
        transfer: Option<crate::filetransfer::IncomingTransfer>,
    ) -> anyhow::Result<()> {
        match self
            .receive_file_inner(request, device, info, expected_size, &transfer)
            .await
        {
            Ok((dest, bytes)) => {
                info!(
                    "[share] saved '{}' to {:?} ({} bytes)",
                    request.filename, dest, bytes
                );
                if let Some(t) = transfer {
                    t.completed(dest.clone(), bytes);
                }

                let dest_display = dest.display().to_string();
                let filename = request.filename.clone();
                let notified = tokio::task::spawn_blocking(move || {
                    notify_rust::Notification::new()
                        .appname("KDE Connect")
                        .summary(&format!("File received: {}", filename))
                        .body(&dest_display)
                        .show()
                        .map(|_| ())
                })
                .await;
                // A saved file stays a successful transfer even when its
                // notification fails — log the notification failure only.
                match notified {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!("[share] notification failed for saved file: {}", e),
                    Err(e) => warn!("[share] notification task failed for saved file: {}", e),
                }

                Ok(())
            }
            Err((stage, e)) => {
                if let Some(t) = transfer {
                    t.failed(stage, format!("{:#}", e));
                }
                Err(e)
            }
        }
    }

    /// Receives the payload into a temporary file in the destination
    /// directory and publishes it only after the transfer validates. Each
    /// fallible step is tagged with its failure stage for UI reporting.
    async fn receive_file_inner(
        &self,
        request: &ShareRequestFile,
        device: &Device,
        info: &PacketPayloadTransferInfo,
        expected_size: Option<u64>,
        transfer: &Option<crate::filetransfer::IncomingTransfer>,
    ) -> Result<(PathBuf, u64), (&'static str, anyhow::Error)> {
        // An unavailable/unwritable Downloads directory is reported, not
        // silently replaced with /tmp.
        let download_dir = dirs::download_dir()
            .ok_or_else(|| ("destination", anyhow::anyhow!("Downloads directory is unavailable")))?;

        let download = crate::download::Download::new(&download_dir, &request.filename)
            .map_err(|e| ("destination", e.context("preparing destination file")))?;
        let mut file = download
            .writer()
            .map_err(|e| ("destination", e.context("opening destination file")))?;

        let mut remote_addr = device.address;
        remote_addr.set_port(info.port);

        let progress_fn;
        let progress_cb: Option<&(dyn Fn(u64) + Send + Sync)> = match transfer {
            Some(t) => {
                progress_fn = move |bytes: u64| t.progress(bytes);
                Some(&progress_fn)
            }
            None => None,
        };

        info!(
            "[share] receiving '{}' from {} ({}:{}), expecting {:?} bytes",
            request.filename,
            device.name,
            remote_addr.ip(),
            info.port,
            expected_size
        );

        let received =
            receive_payload(device, &remote_addr, &mut file, expected_size, progress_cb)
                .await
                .map_err(|e| (e.stage, anyhow::Error::new(e)))?;

        drop(file);
        let dest = download
            .finish(true)
            .map_err(|e| ("publish", e.context("publishing file to Downloads")))?;

        Ok((dest, received))
    }
}


#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ShareRequestFile {
    pub filename: String,
    pub open: Option<bool>,
}

impl ShareRequest {
    pub async fn send_file(
        &self,
        writer: &mpsc::UnboundedSender<ProtocolPacket>,
        payload_size: u64,
        payload_transfer_info: Option<PacketPayloadTransferInfo>,
    ) {
        let packet = ProtocolPacket::new_with_payload(
            PacketType::ShareRequest,
            serde_json::to_value(self.clone()).expect("failed serialize packet body"),
            payload_size,
            payload_transfer_info,
        );

        let _ = writer.send(packet);
    }
}
