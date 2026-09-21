# Code Review Summary

> ✅ = addressed (fixed) or verified and confirmed to need no action. ⏳ = needs addressing or verification.

P1 = high priority. P2 = normal priority. Reviews are ordered oldest first; append new reviews and follow-up items at the end. Earlier status labels record the result at that time; later findings may reopen an issue.

## Review Index

- [First review - 2026-08-05](#first-review---2026-08-05)
- [Second review - 2026-09-20](#second-review---2026-09-20)

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

✅ **[P1] Newest messages hidden in long threads**: Fixed by rendering the newest window from the chronologically sorted message list. "Load older messages" now expands toward the older prefix. Verified by a 150-message regression test.

✅ **[P1] SMS data mixed between devices**: Fixed by carrying device ID through core events and D-Bus message/attachment signals, using a per-device in-memory cache keyed by source device, saving to disk under the source device ID, and filtering events in each SMS window. D-Bus and Varlink cache reads now select only the requested device. Verified by a two-device cache test.

✅ **[P2] Newest messages shown first**: The bounded message view skips the hidden older prefix and renders the most recent messages in chronological order. Increasing the window exposes older messages without hiding new arrivals.

✅ **[P2] SMS windows filter by device**: Message and MMS attachment events include their source device ID. Each SMS window ignores events belonging to other phones.

✅ **[P2] SMS events retain source identity**: `ConnectionEvent::SmsMessages` carries `(DeviceId, SmsMessages)`; the plugin dispatcher attaches the authenticated source device before forwarding the event.

✅ **[P2] Per-device SMS cache**: Replaced the single optional in-memory SMS payload with a map keyed by device ID. Incoming messages update memory and disk using their source ID. D-Bus and Varlink cache reads return only the requested device's data.

✅ **[P2] Device-scoped D-Bus signals**: SMS message and attachment signals include `device_id`; the D-Bus client exposes it in `ServiceEvent`.

✅ **[P2] Quadratic duplicate detection remains**: Fixed. `app.rs` now keeps a `MessageIndex` over the open thread's message list (protocol ID → position, plus body → positions of pending optimistic `sending_*` messages), so duplicate checks and outgoing-echo matching are O(1) per incoming message instead of a full scan. The index is maintained incrementally during a batch and rebuilt after the end-of-batch sort and on optimistic sends; it is cleared with the list on thread switch/delete. Echo matching semantics preserved (same body, type 2, within 300s). Covered by five new unit tests including a sort/rebuild regression test.

✅ **Group SMS names** Root cause found in the user's own caches: the KDE Connect protocol has no group-name field (Android doesn't expose one; Google Messages stores group names privately), so the applet resolved a conversation's name from a single address — the newest message's first address — via an exact digit-only contact lookup. RCS group threads carry an alphanumeric group token as their address, which normalizes to the empty string and collided with a synced "Skype call" contact whose TEL is `+` (also normalizing to ""). Fixed two ways: (1) `Conversation` now keeps the full participant roster (union of all message addresses in the thread) and the display name joins every participant's resolved contact name ("Amy, Clara"), falling back to the raw address; (2) contact lookups now require at least 7 digits (`MIN_LOOKUP_DIGITS`), so degenerate contacts and short-code senders can no longer produce bogus names. Covered by new tests in `utils.rs` and `dbus.rs`.

**SMS Verification**:
- `cargo test -p cosmic-ext-connect-applet --locked --offline`: 3 passed. The new test confirms a 150-message thread initially renders messages 50 through 149.
- `cargo test -p kdeconnect-service --locked --offline`: 2 passed, 1 compositor-dependent clipboard test ignored. The new test confirms two phones retain distinct SMS cache values.

### kdeconnect-core

✅ **[P1] Paired identity can be impersonated**: Fixed by pinning the certificate approved during pairing and checking it on control and payload connections. Packets are bound to their connection ID, and replaced connections are closed. Legacy records without a certificate require re-pairing.

✅ **[P1] Received filenames escape destination directories**: Remote names are restricted to a single filename; device IDs are validated before filesystem use. Share and MMS downloads use temporary-file descriptors and atomic publication, with no-clobber collision handling for shared files. Tests cover traversal, symlinks, duplicate names, and cleanup.

✅ **[P1] Silent TCP client blocks inbound connections**: Identity exchange and TLS run in bounded tasks with a 15-second deadline and at most 32 concurrent handshakes across TCP/UDP discovery. Invalid identities are rejected without inline TLS cleanup. Verified by a listener regression test that checks another connection remains responsive while the silent peer times out.

✅ **[P2] Reconnection retains an old network address**: Device loading now refreshes address/name from the current connection while retaining trusted pairing data. Covered by a changed-address test. Live validation after network changes remains pending under FT-06.

✅ **[P2] Certificate pinning and pairing persistence**: Approved peer certificates are stored atomically with paired state. Changed certificates are rejected before packet dispatch; payload clients and servers must present the same pinned certificate. Pending approvals are tied to the live connection and reset on reconnect. Local unpair can clear a pin even when certificate mismatch prevents reconnection.

✅ **[P2] Legacy pairing migration**: Records without a pinned certificate load as unpaired and require explicit approval again. No certificate is silently trusted during upgrade. Device metadata is refreshed from the current connection.

✅ **[P2] Contained file reception**: Added `download.rs` for basename validation, temporary-file ownership, cleanup, atomic cache replacement, and no-clobber shared-file publication. Updated share, MMS, and album-art payload callers to use open descriptors instead of reopening remote-selected paths.

✅ **[P2] Bounded connection setup**: The entire pre-TLS identity exchange and handshake runs outside the accept loop, limited to 32 concurrent tasks with a 15-second deadline. Connection IDs reject queued stale packets; closing/replacing a writer also closes its reader.

✅ **[P2] Text and URL shares silently discarded**: Fixed. `plugin_interface.rs` no longer requires payload metadata to dispatch a share; `ShareRequest::receive_share` now takes `Option<&PacketPayloadTransferInfo>` and errors only when a file share arrives without transfer info, so body-only text/URL shares are dispatched normally and a malformed file share logs a warning instead of being silently dropped. Verified with `cargo check -p kdeconnect-core`.

✅ **[P2] Short payload reported successful**: Fixed. `receive_payload` in `transport.rs` now takes the announced `payload_size` (`expected_size: Option<u64>`), compares it against the byte count returned by `tokio::io::copy`, and fails on any mismatch (short or excess), so a clean EOF mid-transfer is reported as an error. Dispatch threads `packet.payload_size` through all three receive paths (share, MMS attachment, MPRIS album art); callers only publish via `Download::finish` after validation passes, and a failed transfer drops the temporary file. Covered by the `payload_size_is_validated` unit test.

**Core Verification**:
- `cargo check --workspace --locked --offline` passed. No dependency changes were needed.
- `cargo test -p kdeconnect-core --lib --locked --offline`: 20 passed, including legacy pairing, stored-pin reload/revocation, mismatched certificates in both TLS payload directions, path traversal, symlinks, duplicate names, temporary-file cleanup, and listener responsiveness/deadline. The loopback socket test requires permission outside the execution sandbox.
- Existing core tests passed: 7 passed, 0 failed. These tests cover SFTP helpers.

### Transfer and Logging Follow-up

The reported missing image was not reproduced; its historical failure cause remains unknown. The running service's stdout/stderr pointed to `/dev/null` — that logging gap and the transfer findings below are now addressed; only the live-device portions remain pending.

✅ **[P2] Preserve transfer diagnostics**: `cosmic-ext-connect-applet/src/main.rs` now routes the spawned service's stdout/stderr into `~/.local/share/kdeconnect/service.log` (one rotation generation at launch, 1 MiB cap) instead of `/dev/null`, and sets `RUST_LOG=info` for the child when the user hasn't chosen a level so transfer diagnostics are retained. `kdeconnect-service/src/main.rs` honors `KDECONNECT_LOG_FILE` on all platforms (not just Flatpak), writing to the same path. The XDG autostart entry (`resources/io.github.hepp3n.kdeconnect.daemon.desktop` + justfile `install-autostart`) now launches the service with `KDECONNECT_LOG_FILE=1 RUST_LOG=info`, so the normal login path retains logs too. Flatpak runtime verification remains pending.

✅ **[P2] Log transfer stages and rejection reasons**: Every incoming payload transfer now gets a transfer ID (logged and carried in events) with receipt logged at dispatch, stage-tagged errors (`PayloadError.stage`: connect, tls-handshake, verify-peer, receive, size-validation, finalize, plus destination/publish/metadata in the plugins), and `{:#}` full error chains in failure logs. Invalid share bodies and attachment packets missing payload metadata log warnings; no packet bodies, file contents, or credentials are logged. Text/URL sharing stays independent of file metadata.

✅ **[P2] Show incoming transfer status and errors**: Added structured `TransferStatus` events in `kdeconnect-core/src/event.rs` (transfer/device IDs, direction, filename, expected size, received bytes, receiving/completed/failed state with stage+reason, saved path). An `IncomingTransfer` reporter (`filetransfer.rs`) emits throttled progress (200 ms) and exactly one terminal result on the user-facing receive paths (share and MMS attachment; album art is deliberately excluded as a cache side effect whose reporting would flood the list on every track change — its failures still log with full chain). The service forwards them as a D-Bus `transfer_status` signal plus a varlink broadcast, keeps a bounded 50-entry recent-results list (`record_transfer_status`, deduped by transfer ID) exposed via D-Bus and varlink `GetRecentTransfers`, and the applet seeds from that list, shows per-device receiving/completed/failed states with progress bars, saved paths, and failure stage/reason in the popup, and keeps its own bounded 20-entry list. Notification failures are logged separately; a saved file remains a successful transfer.

✅ **[P2] Add deadlines and size validation**: `receive_payload` in `transport.rs` now enforces a 10 s connection deadline, a 15 s TLS-handshake deadline, and a 30 s read-inactivity deadline via a manual copy loop that only fails when no bytes arrive for the full window — large transfers that keep progressing are unaffected. Failures are stage-tagged (`PayloadError`), and received bytes are validated against the announced `payloadSize` (short and excess both rejected) before the file is published. Covered by copy-loop tests (progress/EOF, stall timeout, slow-but-progressing) and the size-validation test.

✅ **[P2] Save files safely and report destination failures**: Complete. Share receive now errors with stage "destination" when the Downloads directory is unavailable instead of silently falling back to `/tmp`, and temporary-file creation/opening and final publication carry stage-tagged contextual errors ("destination"/"publish"). MMS attachments get the same staging (unsafe names → "metadata", cache-dir/descriptor failures → "destination", publish failure → "publish"). Reception still goes into a uniquely created temporary file in the destination directory and is published only after size validation passes; partial files are cleaned up on failure.

✅ **[P2] Refresh the phone's payload address** (code complete): Already satisfied by the earlier reconnection fix — every new connection rebuilds the `Device` from the current connection address (`lib.rs:307`) while `load_from` refreshes address/name and preserves trusted pairing data (`device.rs:114-115`), and the share/MMS/album-art receive paths all dial that live address. Certificate pinning is unaffected (changed certificates still rejected; active-connection certificate replacement refused). Only the live portion remains: verify file and MMS downloads after DHCP/network changes, covered under Live-device validation.

### Live Validation

⏳ **[P2] Live-device validation**: Install/restart the updated service, explicitly re-pair the phone, verify reconnect and changed-certificate rejection, and test incoming/outgoing image transfers. No service was installed or restarted during implementation. Transfer-size validation, transfer status/error reporting, and the other pending review items remain separate work.

⏳ **[P2] Live multi-phone validation**: Install/restart the updated service and verify two paired phones cannot populate each other's SMS window or cache. No service was installed or restarted during this implementation.

✅ **[P2] Test failures and a real phone transfer** (automated coverage): New tests cover stalled reception (read-inactivity deadline), slow-but-progressing transfers, progress/EOF byte accounting, short/excess/valid/unannounced payload sizes, malformed/unsafe filenames, simultaneous duplicate names, temporary-file cleanup, transfer reporter throttling with exactly one terminal result, `TransferStatus` JSON roundtrip (the D-Bus/varlink contract), and the bounded deduped recent-results list. Notification failure is handled as log-only while the saved file stays a successful transfer. The real-phone portion — sharing a known image and verifying contents, destination, UI status, and logs against an installed build — remains pending under Live-device validation.
