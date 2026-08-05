# Changelog

## Changes made

Implemented the SMS performance fixes, hardened `kdeconnect-core`/`kdeconnect-service` against panics and zombie processes, and deployed the new build.

### `kdeconnect-core/src/lib.rs`
- `CoreEvent::PacketReceived` now drops any non-pair packet from a device whose `pair_state != Paired` before dispatching to plugins. Previously an unpaired peer that merely completed the TLS handshake could reach `run_command`'s execute-by-key handler.
- Replaced panic-prone `.expect()`/`.unwrap()` calls on network/filesystem input with graceful error handling in `transport_events` (`Device::new`), `CoreEvent::SendPacket`, and `AppEvent::SendFiles` (per-file share failures no longer abort the whole event loop).

### `kdeconnect-core/src/transport.rs`
- `receive_payload` now propagates file create/copy/flush/shutdown errors instead of swallowing them with `let _ =`, so failed transfers are reported as errors instead of silently "succeeding".

### `kdeconnect-core/src/plugins/sftp.rs`
- `open()` (`xdg-open`) now reaps its child process instead of leaking a zombie.

### `kdeconnect-core/src/plugins/run_command.rs`
- Spawned command execution (`sh -c` / `flatpak-spawn --host sh -c`) now reaps its child via `spawn_blocking` instead of leaking a zombie.

### `kdeconnect-service/src/dbus_interface.rs`, `varlink_server.rs`
- `sms_cache` changed from `Arc<Mutex<Option<String>>>` to `Arc<Mutex<Option<Arc<str>>>>` so `get_cached_sms` clones a refcount instead of the full JSON string on every call.

### `cosmic-ext-connect-applet/src/plugins/sms/app.rs`
- Added **message windowing**: only the most recent 100 messages render by default; older messages load on demand via a "Load older messages" button.
- Added `contacts_by_phone: HashMap<String, String>` for O(1) contact lookups.
- Added `contacts_by_name: Vec<String>` cached on `SmsWindow` and refreshed only in `ContactsLoaded`, so the view no longer rebuilds the dropdown name list from `contacts` on every render.
- Added `contact_photo_handles` to store pre-built `image::Handle`s so avatar RGBA buffers aren't cloned every frame.
- Added `search_query_lower` so the view doesn't reallocate a lowercased query on every keystroke.
- Reset the message window when switching threads.
- Updated `update_conversation_names()` to use the fast HashMap.
- Process incoming messages in batches via `handle_messages_received()`; sorts conversations/messages and persists last-seen timestamps once per batch instead of once per message.

### `cosmic-ext-connect-applet/src/plugins/sms/views.rs`
- Replaced linear contact/photo scans with HashMap lookups.
- Removed the redundant `filtered.sort_by(...)` — `app.conversations` is already sorted in `update()`.
- Used the cached lowercased search query.
- Used pre-built avatar handles instead of cloning RGBA buffers.
- Windowed the message list render and added the load-more control.
- Used the cached `contacts_by_name` list instead of rebuilding it from `contacts` every render.

### `cosmic-ext-connect-applet/src/plugins/sms/actions.rs`
- Added `SmsMessage::LoadMoreMessages`.

### i18n
- Added `sms-load-more-messages` key to English, Polish, and Czech fluent files.

### `cosmic-ext-connect-applet/src/plugins/sms/models.rs`
- Added `ProtocolEvent::MessagesReceived(Vec<Message>)` so incoming message batches can be processed in one pass.

### `cosmic-ext-connect-applet/src/main.rs`
- Reaped spawned `kdeconnect-service`, `cosmic-ext-connect-sms`, and `cosmic-ext-connect-settings` children in background threads so they don't become zombies.

### `cosmic-ext-connect-applet/src/backend.rs`
- Replaced the fixed 3-second device-wait loop with exponential backoff (1s → 10s cap).
- Added cancellation via `tokio::select!` so the wait loop exits immediately when `kdeconnect-service` disappears from the bus or the `NameOwnerChanged` stream ends.

### Verified, no change needed
- `kdeconnect-core/src/plugins/mpris.rs`: `Mpris::default()`'s `.expect(...)` cannot fail in practice; the `serde_json::to_value(...).unwrap()` calls serialize internal structs (effectively infallible); watcher tasks self-terminate and are deduplicated via `watched_players`.
- `kdeconnect-service/src/clipboard.rs`: both `.expect(...)` calls are safe — one is guarded by an earlier `Err` return, the other is `#[cfg(test)]`-only.
- `run_command.rs` executing the persisted command string via `sh -c` with no escaping is inherent to the feature (matches upstream KDE Connect); the real gap was reachability from unpaired devices, closed above.
