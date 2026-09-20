# Code Review Summary

> ✅ = addressed (fixed) or verified and confirmed to need no action. ⏳ = needs addressing or verification.

P1 = high priority. P2 = normal priority. Reviews are ordered oldest first; append new reviews and follow-up items at the end. Earlier status labels record the result at that time; later findings may reopen an issue.

## Review Index

- [First review - 2026-08-05](#first-review---2026-08-05)
- [Second review - 2026-09-20](#second-review---2026-09-20)
- [Incoming image transfer - 2026-09-20](#incoming-image-transfer---2026-09-20)

## First Review - 2026-08-05

### SMS Issues

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

### kdeconnect-core

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

### kdeconnect-service

✅ **SMS cache copied on every D-Bus call** `sms_cache` changed from `Arc<Mutex<Option<String>>>` to `Arc<Mutex<Option<Arc<str>>>>` across `dbus_interface.rs` and `varlink_server.rs`, so `get_cached_sms` now clones an `Arc` (refcount bump) instead of deep-copying the full JSON string on every call. The in-memory cache is still written once per incoming SMS batch, not per request.

✅ **Clipboard worker panics if the compositor changes** — verified, no action needed: both `.expect(...)` calls in `clipboard.rs` are safe. The one in `start()` is only reached after `state.manager.is_none()` already returned an `Err`, so the invariant it relies on is guaranteed; the other is in `#[cfg(test)]` code, not the runtime path. No code change needed.

### cosmic-ext-connect-applet

✅ **Zombie kdeconnect-service process** `cosmic-ext-connect-applet/src/main.rs:577` spawns `kdeconnect-service` and drops the `Child` handle. If the child exits (because the real service is already running and the D-Bus name is taken) it is never reaped.

✅ **Zombie settings process** `cosmic-ext-connect-applet/src/main.rs:378-380` spawns `cosmic-ext-connect-settings` and drops the `Child` handle, so it is never reaped.

✅ **Zombie SMS window process** `cosmic-ext-connect-applet/src/main.rs:175` spawns `cosmic-ext-connect-sms` inside a thread but the original code dropped the `Child` handle without waiting, leaving a zombie on exit.

✅ **Applet service watcher busy-loops** `cosmic-ext-connect-applet/src/backend.rs:528-541` retries `fetch_devices()` every 3 seconds for up to 90 seconds with no exponential back-off or cancellation, keeping the applet awake after the bus disappears. Fixed by switching to exponential backoff (1s → 10s cap) and cancelling the wait loop when the service disappears or the `NameOwnerChanged` stream ends.

## Second Review - 2026-09-20

Reviewed source at `ef47cb9`. No live phone or GUI integration testing was performed.

### SMS Issues

✅ **[P1] Newest messages hidden in long threads**: Fixed by rendering the newest window from the chronologically sorted message list. "Load older messages" now expands toward the older prefix. Covered by a 150-message regression test.

⏳ **Quadratic duplicate detection remains**: `app.rs:727` scans accumulated messages for every incoming message. Replace repeated linear duplicate checks with indexed lookup while preserving outgoing-echo matching. The first review's batching fix reduced sorting and persistence, but did not eliminate quadratic insertion.

### kdeconnect-core

✅ **[P1] Paired identity can be impersonated**: Fixed by pinning the certificate approved during pairing and checking it on control and payload connections. Packets are bound to their connection ID, and replaced connections are closed. Legacy records without a certificate require re-pairing. See the implementation follow-up for validation.

✅ **[P1] Received filenames escape destination directories**: Remote names are restricted to a single filename; device IDs are validated before filesystem use. Share and MMS downloads use temporary-file descriptors and atomic publication, with no-clobber collision handling for shared files. Tests cover traversal, symlinks, duplicate names, and cleanup.

✅ **[P1] Silent TCP client blocks inbound connections**: Identity exchange and TLS run in bounded tasks with a 15-second deadline and at most 32 concurrent handshakes across TCP/UDP discovery. Invalid identities are rejected without inline TLS cleanup. The listener regression test checks another connection remains responsive and the silent peer times out.

✅ **[P2] Reconnection retains an old network address**: Device loading now refreshes address/name from the current connection while retaining trusted pairing data. Covered by a changed-address test; live phone/network-change validation remains pending under FT-06.

⏳ **[P2] Text and URL shares silently discarded**: `plugin_interface.rs:361-366` requires payload metadata for every share. Require it only for file shares and dispatch body-only text/URL shares normally.

⏳ **[P2] Short payload reported successful**: `transport.rs:581-608` ignores the byte count from `tokio::io::copy` and has no expected-size parameter. Pass the announced payload size, validate received bytes, and publish the file only after validation. The first review's I/O error propagation does not detect clean EOF before the expected size.

### kdeconnect-service

✅ **[P1] SMS data mixed between devices**: Fixed by carrying device ID through core events and D-Bus message/attachment signals, using a per-device in-memory cache, saving under the source device ID, and filtering events in each SMS window. D-Bus and Varlink cache reads now select only the requested device. Covered by a two-device cache test.

### Verification

✅ **Existing core tests passed**: `cargo test -p kdeconnect-core --lib --locked --offline` completed with 7 passed and 0 failed. These tests cover SFTP helpers, not the open findings above.

✅ **Earlier changes checked in source**: Unpaired-device dispatch gate, payload I/O error propagation, bounded SMS rendering, cached dropdown/search values, batched message processing, and applet child reaping are present. Remaining gaps are listed above; the first review's pending rich-text allocations and best-effort send logging remain open.

### P1 Core Fixes

#### kdeconnect-core

✅ **Certificate pinning and pairing persistence**: Approved peer certificates are stored atomically with paired state. Changed certificates are rejected before packet dispatch; payload clients and servers must present the same pinned certificate. Pending approvals are tied to the live connection and reset on reconnect. Local unpair can clear a pin even when certificate mismatch prevents reconnection.

✅ **Legacy pairing migration**: Records without a pinned certificate load as unpaired and require explicit approval again. No certificate is silently trusted during upgrade. Device metadata is refreshed from the current connection.

✅ **Contained file reception**: Added `download.rs` for basename validation, temporary-file ownership, cleanup, atomic cache replacement, and no-clobber shared-file publication. Updated share, MMS, and album-art payload callers to use open descriptors instead of reopening remote-selected paths.

✅ **Bounded connection setup**: The entire pre-TLS identity exchange and handshake runs outside the accept loop, limited to 32 concurrent tasks with a 15-second deadline. Connection IDs reject queued stale packets; closing/replacing a writer also closes its reader.

#### Verification

✅ **Core regression tests**: `cargo test -p kdeconnect-core --lib --locked --offline`: 20 passed, including legacy pairing, stored-pin reload/revocation, mismatched certificates in both TLS payload directions, path traversal, symlinks, duplicate names, temporary-file cleanup, and listener responsiveness/deadline. The loopback socket test requires permission outside the execution sandbox.

✅ **Workspace build**: `cargo check --workspace --locked --offline` passed. No dependency changes were needed.

⏳ **Live-device validation**: Install/restart the updated service, explicitly re-pair the phone, verify reconnect and changed-certificate rejection, and test incoming/outgoing image transfers. No service was installed or restarted during implementation. Transfer-size validation, transfer status/error reporting, and the other pending review items remain separate work.

### P1 SMS Fixes

#### SMS UI

✅ **Newest messages shown first**: The bounded message view skips the hidden older prefix and renders the most recent messages in chronological order. Increasing the window exposes older messages without hiding new arrivals.

✅ **SMS windows filter by device**: Message and MMS attachment events include their source device ID. Each SMS window ignores events belonging to other phones.

#### kdeconnect-core

✅ **SMS events retain source identity**: `ConnectionEvent::SmsMessages` carries `(DeviceId, SmsMessages)`; the plugin dispatcher attaches the authenticated source device before forwarding the event.

#### kdeconnect-service

✅ **Per-device SMS cache**: Replaced the single optional in-memory SMS payload with a map keyed by device ID. Incoming messages update memory and disk using their source ID. D-Bus and Varlink cache reads return only the requested device's data.

✅ **Device-scoped D-Bus signals**: SMS message and attachment signals include `device_id`; the D-Bus client exposes it in `ServiceEvent`.

#### Verification

✅ **Service tests**: `cargo test -p kdeconnect-service --locked --offline`: 2 passed, 1 compositor-dependent clipboard test ignored. The new test confirms two phones retain distinct SMS cache values.

✅ **Applet tests**: `cargo test -p cosmic-ext-connect-applet --locked --offline`: 3 passed. The new test confirms a 150-message thread initially renders messages 50 through 149.

⏳ **Live multi-phone validation**: Install/restart the updated service and verify two paired phones cannot populate each other's SMS window or cache. No service was installed or restarted during this implementation.

## Incoming Image Transfer - 2026-09-20

The reported missing image was not reproduced; its historical failure cause remains unknown. The running service's stdout/stderr pointed to `/dev/null`. Items below extend the second review's transfer findings and remain pending.

### kdeconnect-service

⏳ **FT-01: Preserve transfer diagnostics**: Update `kdeconnect-service/src/main.rs` and the applet service launch in `cosmic-ext-connect-applet/src/main.rs:602-614` to retain logs in a bounded/rotated file or journal during normal launches. Verify native and Flatpak logging separately.

### kdeconnect-core

⏳ **FT-02: Log transfer stages and rejection reasons**: In `plugin_interface.rs`, `plugins/share.rs`, and `transport.rs`, record a transfer ID, source device, filename, expected size, current endpoint, destination, received bytes, stage, and full error chain. Report request receipt, invalid bodies, missing file metadata, and pairing/plugin rejection reasons. Do not log packet bodies, file contents, or credentials. Keep text/URL sharing independent of file metadata.

⏳ **FT-04: Add deadlines and size validation**: In `transport.rs`, add connection, TLS-handshake, and read-inactivity deadlines with stage-specific errors. Allow large transfers that keep progressing. Reject short or excess payloads against the announced size.

⏳ **FT-05: Save files safely and report destination failures** (partially implemented: filename checks, temporary files, atomic publication, collision/symlink protection, and cleanup are complete; size validation and Downloads error reporting remain): Reject unsafe filenames and symlink/collision races. Receive into a uniquely created temporary file in the destination directory, publish only after validation, and clean up failed partial files. Report unavailable/unwritable Downloads instead of silently falling back to `/tmp`. Apply equivalent path protections to MMS attachments.

⏳ **FT-06: Refresh the phone's payload address**: Update `device.rs` and connection registration to retain the current connection address while preserving trusted pairing data. Verify file and MMS downloads after DHCP/network changes. Coordinate with the second review's certificate-authentication fix.

### cosmic-ext-connect-applet

⏳ **FT-03: Show incoming transfer status and errors**: Add structured events in `kdeconnect-core/src/event.rs`, propagate them through D-Bus/Varlink and clients, and display receiving/completed/failed states with throttled progress. Include transfer/device IDs, direction, filename, byte counts, and failure stage/reason. Keep a bounded recent-results list and show the saved path. Log notification failures separately; a saved file remains successful even if its notification fails.

### Verification

⏳ **FT-07: Test failures and a real phone transfer**: Cover malformed/missing metadata, connection refusal, TLS failure, stalled reception, unavailable destination, unsafe names, simultaneous duplicate names, short/excess payloads, and notification failure. Confirm one terminal result and matching diagnostic per detected transfer. Share a known image from the phone and verify contents, size, destination, UI status, and logs; record the installed build and timestamps.
