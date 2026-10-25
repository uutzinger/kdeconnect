use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::CONFIG_DIR;
use crate::protocol::{PacketType, ProtocolPacket};

// ---------------------------------------------------------------------------
// Local command store
// ---------------------------------------------------------------------------

/// A command defined on this desktop that the phone can trigger.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LocalCommand {
    /// UUID used as the map key in the KDE Connect packet.
    pub id: String,
    pub name: String,
    pub command: String,
}

// dirs::config_dir() already reads XDG_CONFIG_HOME (sandboxed under Flatpak)
// and falls back to ~/.config outside it.
fn commands_config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(CONFIG_DIR)
        .join("runcommand.json")
}

fn load_local_commands() -> Vec<LocalCommand> {
    match std::fs::read_to_string(commands_config_path()) {
        Ok(json_str) => serde_json::from_str::<Vec<LocalCommand>>(&json_str).unwrap_or_default(),
        Err(_) => vec![],
    }
}

// ---------------------------------------------------------------------------
// Protocol structs
// ---------------------------------------------------------------------------

/// Incoming `kdeconnect.runcommand.request` from the phone:
///   - `key` -> execute the named command on this desktop
///   - `requestCommandList` -> send our command list back
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RunCommandRequest {
    pub key: Option<String>,
    #[serde(rename = "requestCommandList")]
    pub request_command_list: Option<bool>,
}

// ---------------------------------------------------------------------------
// Packet handlers
// ---------------------------------------------------------------------------

impl RunCommandRequest {
    pub async fn received_packet(
        &self,
        device: &crate::device::Device,
        core_tx: mpsc::UnboundedSender<crate::event::CoreEvent>,
    ) {
        if let Some(key) = &self.key {
            // Phone is asking us to execute a local command by its UUID key.
            let commands = load_local_commands();
            if let Some(cmd) = commands.iter().find(|c| c.id == *key) {
                info!(
                    "[runcommand] executing '{}': {}",
                    cmd.name, cmd.command
                );
                let result = if std::env::var("FLATPAK_ID").is_ok() {
                    std::process::Command::new("flatpak-spawn")
                        .arg("--host")
                        .arg("sh")
                        .arg("-c")
                        .arg(&cmd.command)
                        .spawn()
                } else {
                    std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&cmd.command)
                        .spawn()
                };
                match result {
                    Ok(mut child) => {
                        // Reap the child on a blocking thread so it doesn't
                        // become a zombie once it exits.
                        tokio::task::spawn_blocking(move || {
                            let _ = child.wait();
                        });
                    }
                    Err(e) => {
                        warn!("[runcommand] failed to spawn '{}': {}", cmd.name, e);
                    }
                }
            } else {
                warn!(
                    "[runcommand] unknown key '{}' from {}",
                    key, device.device_id
                );
            }
        } else if self.request_command_list == Some(true) {
            // Phone is asking for our current command list.
            send_command_list(&device.device_id, core_tx).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Outgoing — send our command list to the phone
// ---------------------------------------------------------------------------

/// Build and send a `kdeconnect.runcommand` packet to the phone containing
/// all locally defined commands and `canAddCommand: false`.
///
/// Called on device connect (kdeconnect-core/src/lib.rs) and when the phone
/// requests the list via `requestCommandList: true`.
pub async fn send_command_list(
    device_id: &crate::device::DeviceId,
    core_tx: mpsc::UnboundedSender<crate::event::CoreEvent>,
) {
    let commands = load_local_commands();

    info!(
        "[runcommand] sending {} command(s) to {}",
        commands.len(),
        device_id
    );

    let _ = core_tx.send(crate::event::CoreEvent::SendPacket {
        device: device_id.clone(),
        packet: command_list_packet(&commands),
    });
}

fn command_list_packet(commands: &[LocalCommand]) -> ProtocolPacket {
    // commandList must be a *JSON-encoded string*, not a nested object.
    // Format: "{\"<uuid>\": {\"name\": \"...\", \"command\": \"...\"}, ...}"
    let mut map = serde_json::Map::new();
    for cmd in commands {
        map.insert(
            cmd.id.clone(),
            serde_json::json!({ "name": cmd.name, "command": cmd.command }),
        );
    }
    let command_list_str =
        serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| "{}".to_string());

    ProtocolPacket::new(
        PacketType::RunCommand,
        serde_json::json!({
            "commandList": command_list_str,
            "canAddCommand": false,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_list_keeps_local_commands_but_disallows_remote_addition() {
        let packet = command_list_packet(&[LocalCommand {
            id: "approved-id".into(),
            name: "Approved".into(),
            command: "true".into(),
        }]);
        assert!(matches!(packet.packet_type, PacketType::RunCommand));
        assert_eq!(packet.body["canAddCommand"], false);
        let advertised: serde_json::Value =
            serde_json::from_str(packet.body["commandList"].as_str().unwrap()).unwrap();
        assert_eq!(advertised["approved-id"]["command"], "true");
    }
}
