# Code Review Summary

## Introduction

> ✅ = addressed (fixed) or verified and confirmed to need no action. ⏳ = still pending / not yet addressed.

## SMS Issues

✅ **Unbounded message list re-rendered on every keystroke** Because COSMIC/iced is immediate-mode, every keystroke in the input field triggers a full view() rebuild. With a long conversation (hundreds or thousands of messages), you are rebuilding hundreds of message bubbles, rich-text spans, and contact lookups per character typed. This is the main reason input becomes sluggish over time.

✅ **Redundant conversation sort in the view**: views.rs:185 re-sorts the conversation list on every render:
filtered.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
But app.conversations is already kept sorted in update() (app.rs:443, app.rs:698). This is wasted work every frame.

✅ **O(n × m) contact lookups**
get_contact_name() and get_contact_photo() (views.rs:720-735) do a linear scan of app.contacts for every conversation row and every received message bubble. If you have 500 contacts and 500 conversations, the conversation list alone does 250,000 string comparisons per render.

**Expensive per-frame allocations** (mixed — see each item):
- ⏳ `mixed_emoji_text()` still rebuilds rich-text spans for every visible message preview and body on every keystroke, though now limited by the message window. Not yet fixed.
- ✅ `view_contact_avatar()` no longer clones the entire avatar RGBA buffer; it clones a pre-built `image::Handle` instead.
- ✅ `view_conversations_list()` no longer rebuilds the contacts dropdown name list every frame — `contacts_by_name: Vec<String>` is now cached on `SmsWindow` and refreshed only in `ContactsLoaded`, and the view borrows it (`app.rs`, `views.rs`).
- ✅ `conversation_matches_search()` now uses the cached `search_query_lower` instead of allocating `query.to_lowercase()` every render.

✅ **Message list never trimmed** (verified, accepted, no action needed): `self.messages` holds the full conversation for the currently open thread only (cleared on thread switch), with `messages_window_size` bounding what's rendered. Trimming the backing `Vec` itself isn't currently possible without a paginated fetch API from the service, since "Load older messages" reads from the same in-memory list. Left as-is; not a regression risk given typical SMS thread sizes.

✅ **O(n²) message insertion on incoming cache**: Initial SMS cache and full syncs were processed one `MessageReceived` event at a time, sorting `self.messages` and persisting the last-seen timestamp after every single message (`app.rs`). For a long thread this becomes quadratic and duplicates work.

## kdeconnect-core

✅ **Unpaired devices could trigger plugin side effects** (root cause found during verification, not in the original list): `CoreEvent::PacketReceived` in `kdeconnect-core/src/lib.rs` dispatched every non-pair packet to the plugin registry regardless of `device.pair_state`. Since `run_command.rs` persists commands sent by any connected peer and executes them by key on request, a device that only completed the TLS handshake (KDE Connect accepts any self-signed cert) but was never paired/approved by the user could add and immediately execute arbitrary shell commands. Fixed by dropping any non-pair packet from a device whose `pair_state != Paired` before dispatch. This is the concrete mechanism behind the "unvalidated remote command execution" finding below — the command string itself is still trusted verbatim by design (matches upstream KDE Connect's run-command model), but it can no longer be reached by an unpaired device.

✅ **Panic-prone error handling in the core event loop** Replaced the riskiest `.expect(...)`/`.unwrap()` calls on real network/filesystem input with graceful error handling:
- `Device::new(...)` failures in `transport_events` (`NewConnection`) now log and drop the connection instead of panicking the whole event loop.
- `CoreEvent::SendPacket`'s `sender.send(packet).unwrap()` now logs and drops the packet if the writer task already exited.
- `AppEvent::SendFiles` no longer panics if `ShareRequest::share_files` fails or an individual file can't be opened/serialized; it logs and skips that file instead of aborting the whole share and the event loop.
- Remaining `.expect(...)` calls (e.g. `serde_json::to_value` on internal structs, `GLOBAL_CONFIG.get().unwrap()` after `OnceLock::set`) are on values that cannot fail given the surrounding code and were left as-is.

✅ **Incoming file payloads can be silently corrupted** `receive_payload` in `kdeconnect-core/src/transport.rs` now propagates `File::create`, `copy`, `flush`, and `shutdown` errors via `?` instead of swallowing them with `let _ =`, so a failed transfer is reported as an error to its callers (`share.rs`, `sms.rs`, `mpris.rs`) instead of appearing to succeed.

✅ **Zombie child processes in plugins** Fixed the two remaining fire-and-forget `spawn()` calls that never reaped their child:
- `sftp.rs::open()` (`xdg-open`) now awaits the child on a spawned task.
- `run_command.rs`'s `sh -c`/`flatpak-spawn --host sh -c` execution now reaps the child via `spawn_blocking`.
- (`sshfs` mounting in `sftp.rs` already correctly used `child.wait()`/timeout handling.)

✅ **Unvalidated remote command execution** — verified, accepted: `run_command.rs` still runs the persisted command string through `sh -c` with no escaping, but this is inherent to the run-command feature (the whole point is executing a user-defined shell command) and matches upstream KDE Connect. The actual vulnerability was that this could be reached by unpaired devices; that's now closed by the pairing-gate fix above. No further action planned.

**Silently dropped channel sends** (mixed — see each item):
- ✅ `CoreEvent::SendPacket` now logs when a send fails (see above).
- ⏳ The remaining `let _ = sender.send(...)` sites (pairing acks, contacts/mpris bootstrap requests, `broadcast_tx` fan-out in the service) are still silent. These are believed to be low-risk best-effort notifications (a dropped send just means a peer re-syncs on the next event), but they have not been changed and are left pending should logging be desired.

✅ **MPRIS monitor can panic and leak watchers** — verified, no action needed: `Mpris::default()`'s `MprisPlayer::new(None).expect(...)` cannot actually fail (that path always returns `Ok`), so it's a non-issue. The `serde_json::to_value(...).unwrap()` calls serialize internal structs to JSON, which is effectively infallible here. The per-player watcher tasks (`spawn_blocking`) are detached but self-terminate when `player.events()` ends and are deduplicated via `watched_players`, so they don't grow unbounded.

## kdeconnect-service

✅ **SMS cache copied on every D-Bus call** `sms_cache` changed from `Arc<Mutex<Option<String>>>` to `Arc<Mutex<Option<Arc<str>>>>` across `dbus_interface.rs` and `varlink_server.rs`, so `get_cached_sms` now clones an `Arc` (refcount bump) instead of deep-copying the full JSON string on every call. The in-memory cache is still written once per incoming SMS batch, not per request.

✅ **Clipboard worker panics if the compositor changes** — verified, no action needed: both `.expect(...)` calls in `clipboard.rs` are safe. The one in `start()` is only reached after `state.manager.is_none()` already returned an `Err`, so the invariant it relies on is guaranteed; the other is in `#[cfg(test)]` code, not the runtime path. No code change needed.

## cosmic-ext-connect-applet

✅ **Zombie kdeconnect-service process** `cosmic-ext-connect-applet/src/main.rs:577` spawns `kdeconnect-service` and drops the `Child` handle. If the child exits (because the real service is already running and the D-Bus name is taken) it is never reaped.

✅ **Zombie settings process** `cosmic-ext-connect-applet/src/main.rs:378-380` spawns `cosmic-ext-connect-settings` and drops the `Child` handle, so it is never reaped.

✅ **Zombie SMS window process** `cosmic-ext-connect-applet/src/main.rs:175` spawns `cosmic-ext-connect-sms` inside a thread but the original code dropped the `Child` handle without waiting, leaving a zombie on exit.

✅ **Applet service watcher busy-loops** `cosmic-ext-connect-applet/src/backend.rs:528-541` retries `fetch_devices()` every 3 seconds for up to 90 seconds with no exponential back-off or cancellation, keeping the applet awake after the bus disappears. Fixed by switching to exponential backoff (1s → 10s cap) and cancelling the wait loop when the service disappears or the `NameOwnerChanged` stream ends.
