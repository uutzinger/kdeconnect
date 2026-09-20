# Changelog

## Changes made

Implemented SMS performance fixes, hardened `kdeconnect-core`/`kdeconnect-service` against panics and zombie processes, added certificate pinning and safe file reception, and scoped SMS data per device.

### Security

- `kdeconnect-core/src/pairing.rs`, `device.rs`
  - Pin the TLS certificate approved during pairing and verify it on control and payload connections.
  - Reject changed certificates before packet dispatch; close replaced connections.
  - Persist pinned certificates atomically with paired state and reload them on startup.
  - Legacy pairing records without a pinned certificate load as unpaired and require explicit re-approval.
  - Refresh device address/name from the current connection while retaining trusted pairing data.

- `kdeconnect-core/src/lib.rs`
  - `CoreEvent::PacketReceived` now drops any non-pair packet from a device whose `pair_state != Paired` before dispatching to plugins. Previously an unpaired peer that merely completed the TLS handshake could reach `run_command`'s execute-by-key handler.

- `kdeconnect-core/src/download.rs` (new)
  - Added `download.rs` for safe inbound file handling: basename validation, temporary-file ownership, cleanup, atomic cache replacement, and no-clobber shared-file publication.

- `kdeconnect-core/src/transport.rs`, `plugins/share.rs`, `plugins/sms.rs`
  - Share and MMS downloads use the new `download.rs` helpers and open file descriptors instead of reopening remote-selected paths.
  - Remote filenames are restricted to a single basename and device IDs are validated before filesystem use.
  - Temporary files are created in the destination directory, published atomically after validation, and partial/failed files are cleaned up.

- `kdeconnect-core/src/transport.rs`
  - Bounded connection setup: identity exchange and TLS run in up to 32 concurrent tasks with a 15-second deadline, so a silent/malformed TCP client can no longer block inbound connections.
  - Connection IDs reject queued stale packets; closing/replacing a writer also closes its reader.

### Reliability

- `kdeconnect-core/src/lib.rs`
  - Replaced panic-prone `.expect()`/`.unwrap()` calls on network/filesystem input with graceful error handling in `transport_events` (`Device::new`), `CoreEvent::SendPacket`, and `AppEvent::SendFiles` (per-file share failures no longer abort the whole event loop).

- `kdeconnect-core/src/transport.rs`
  - `receive_payload` now propagates file create/copy/flush/shutdown errors instead of swallowing them with `let _ =`, so failed transfers are reported as errors instead of silently "succeeding".

- `kdeconnect-core/src/plugins/sftp.rs`
  - `open()` (`xdg-open`) now reaps its child process instead of leaking a zombie.

- `kdeconnect-core/src/plugins/run_command.rs`
  - Spawned command execution (`sh -c` / `flatpak-spawn --host sh -c`) now reaps its child via `spawn_blocking` instead of leaking a zombie.

### SMS performance and correctness

- `cosmic-ext-connect-applet/src/plugins/sms/app.rs`
  - Added **message windowing**: only the most recent 100 messages render by default; older messages load on demand via a "Load older messages" button.
  - Added `contacts_by_phone: HashMap<String, String>` for O(1) contact lookups.
  - Added `contacts_by_name: Vec<String>` cached on `SmsWindow` and refreshed only in `ContactsLoaded`, so the view no longer rebuilds the dropdown name list from `contacts` on every render.
  - Added `contact_photo_handles` to store pre-built `image::Handle`s so avatar RGBA buffers aren't cloned every frame.
  - Added `search_query_lower` so the view doesn't reallocate a lowercased query on every keystroke.
  - Reset the message window when switching threads.
  - Updated `update_conversation_names()` to use the fast HashMap.
  - Process incoming messages in batches via `handle_messages_received()`; sorts conversations/messages and persists last-seen timestamps once per batch instead of once per message.

- `cosmic-ext-connect-applet/src/plugins/sms/views.rs`
  - Replaced linear contact/photo scans with HashMap lookups.
  - Removed the redundant `filtered.sort_by(...)` — `app.conversations` is already sorted in `update()`.
  - Used the cached lowercased search query.
  - Used pre-built avatar handles instead of cloning RGBA buffers.
  - Windowed the message list render and added the load-more control.
  - Used the cached `contacts_by_name` list instead of rebuilding it from `contacts` every render.

- `cosmic-ext-connect-applet/src/plugins/sms/actions.rs`
  - Added `SmsMessage::LoadMoreMessages`.

- `cosmic-ext-connect-applet/src/plugins/sms/models.rs`
  - Added `ProtocolEvent::MessagesReceived(Vec<Message>)` so incoming message batches can be processed in one pass.

- `cosmic-ext-connect-applet/src/main.rs`
  - Reaped spawned `kdeconnect-service`, `cosmic-ext-connect-sms`, and `cosmic-ext-connect-settings` children in background threads so they don't become zombies.

- `cosmic-ext-connect-applet/src/backend.rs`
  - Replaced the fixed 3-second device-wait loop with exponential backoff (1s → 10s cap).
  - Added cancellation via `tokio::select!` so the wait loop exits immediately when `kdeconnect-service` disappears from the bus or the `NameOwnerChanged` stream ends.

### SMS per-device isolation

- `kdeconnect-core/src/event.rs`, `plugins/sms.rs`
  - `ConnectionEvent::SmsMessages` now carries `(DeviceId, SmsMessages)` so SMS events retain their authenticated source identity.

- `kdeconnect-service/src/dbus_interface.rs`, `varlink_server.rs`
  - Replaced the single optional in-memory SMS cache with a map keyed by device ID.
  - Incoming messages update memory and disk using their source device ID.
  - D-Bus and Varlink cache reads return only the requested device's data.
  - `sms_cache` changed from `Arc<Mutex<Option<String>>>` to `Arc<Mutex<Option<Arc<str>>>>` so `get_cached_sms` clones a refcount instead of the full JSON string on every call.

- `kdeconnect-service/src/dbus_interface.rs`
  - SMS message and attachment D-Bus signals include `device_id`.

- `kdeconnect-dbus-client/src/lib.rs`
  - `ServiceEvent` exposes the source `device_id` from SMS message and attachment signals.

- `cosmic-ext-connect-applet/src/plugins/sms/app.rs`
  - Each SMS window filters events by device and ignores messages/attachments belonging to other phones.

### i18n

- Added `sms-load-more-messages` key to English, Polish, and Czech fluent files.

### Verified, no change needed

- `kdeconnect-core/src/plugins/mpris.rs`: `Mpris::default()`'s `.expect(...)` cannot fail in practice; the `serde_json::to_value(...).unwrap()` calls serialize internal structs (effectively infallible); watcher tasks self-terminate and are deduplicated via `watched_players`.
- `kdeconnect-service/src/clipboard.rs`: both `.expect(...)` calls are safe — one is guarded by an earlier `Err` return, the other is `#[cfg(test)]`-only.
- `run_command.rs` executing the persisted command string via `sh -c` with no escaping is inherent to the feature (matches upstream KDE Connect); the real gap was reachability from unpaired devices, closed above.

### Tests

- `kdeconnect-core`: 20 passed, including legacy pairing, stored-pin reload/revocation, mismatched certificates in both TLS payload directions, path traversal, symlinks, duplicate names, temporary-file cleanup, and listener responsiveness/deadline.
- `kdeconnect-service`: 2 passed, 1 compositor-dependent clipboard test ignored. New test confirms two phones retain distinct SMS cache values.
- `cosmic-ext-connect-applet`: 3 passed. New test confirms a 150-message thread initially renders messages 50 through 149.
- `cargo check --workspace --locked --offline` passed.

### Pending / not yet implemented

- Rich-text span rebuilds in `mixed_emoji_text()` on every keystroke.
- Best-effort silent channel sends (pairing acks, contacts/mpris bootstrap, broadcast fan-out).
- Text/URL share dispatch without payload metadata.
- Payload size validation and short-payload detection.
- Transfer diagnostics/logging, UI transfer status, and live-device validation.
