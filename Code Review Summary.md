# Code Review Summary

> ✅ = addressed (fixed) or verified and confirmed to need no action. ⏳ = needs addressing or verification.

P1 = high priority. P2 = normal priority. Reviews are ordered oldest first; append new reviews and follow-up items at the end. Earlier status labels record the result at that time; later findings may reopen an issue.

## Review Index

- [First review - 2026-08-05](#first-review---2026-08-05)
- [Second review - 2026-09-20](#second-review---2026-09-20)
- [Third review - 2026-09-21](#third-review---2026-09-21)

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


#### Suggested test pass
- tail -F ~/.local/share/kdeconnect/service.log in a terminal.
- Photo share from phone → popup shows receiving progress → completed with saved path; notification fires; log shows transfer <id>: ... completed.
- MMS with image (your original missing-image case) → watch for transfer events/failures.
- Text share and URL share → notification/browser (these were the silently-discarded ones).
- Outgoing file to the phone → existing progress bar still works.
- DHCP/network change: toggle the phone's Wi-Fi off/on (or switch networks), wait for reconnect, share again — should still land (FT-06 path).
- Group SMS: thread name should now be "Amy, Clara"-style, not "skype".

## Third Review - 2026-09-21

Reviewed source at `5e7d117` against the preceding reviews and `Changelog.md`. Findings may reopen items previously marked fixed. The review itself involved no code changes, service installation/restart, or live phone/GUI validation; items 1–3 were fixed afterward as recorded below.

1. ✅ **[P1] Paired phones could install and execute arbitrary desktop commands.** Previously, `kdeconnect-core/src/plugins/run_command.rs` merged phone-supplied commands into the desktop's local command store; a subsequent request could execute them through `sh -c`. The pairing gate blocked unpaired devices but did not require desktop approval of individual commands. Fixed by removing the inbound command-list handler and its write-to-`runcommand.json` path. The desktop now ignores unsolicited command lists and advertises `canAddCommand: false`; phones can still request the approved list and run locally configured commands by key. A packet test covers the advertised flag and command-list format. `cargo test -p kdeconnect-core --lib --locked --offline` passed 28 tests, including the loopback test run outside the sandbox; `cargo check --workspace --locked --offline` passed. Existing command files do not record whether earlier versions accepted an entry from a phone, so review any preexisting commands before trusting them. Live phone behavior remains unverified.

2. ✅ **[P1] Unpaired peers could exhaust memory after completing TLS.** Previously, `kdeconnect-core/src/transport.rs` read control packets with an unbounded `read_line`, allowing a peer to keep sending bytes without a newline before packet parsing or the pairing check. The control reader now caps each newline-framed packet at 16 MiB and closes the connection when that limit is exceeded. The transport-to-core event channel now holds at most 16 queued events and applies backpressure to fast senders. Tests cover consecutive packets, EOF, oversized unterminated input, and delivery through a full queue. `cargo test -p kdeconnect-core --lib --locked --offline` passed 31 tests, including the loopback test run outside the sandbox; `cargo check --workspace --locked --offline` passed. Packets larger than 16 MiB are rejected, including unusually large legitimate sync responses. Live phone behavior remains unverified.

3. ✅ **[P1] Multi-phone isolation was incomplete for contacts and photos.** The core previously emitted contact and photo events without their source device, so the service saved them under the most recently connected phone and every SMS window accepted their signals. Both core events now carry the authenticated `DeviceId`; service cache writes and D-Bus signals use that ID, including cached contacts emitted on reconnect. The D-Bus client exposes the ID, and each SMS window filters contact and photo signals to its selected phone. New `contacts_cache_v2.json` and `contact_photos_cache_v2.json` files prevent possibly mixed legacy caches from being read; the legacy files remain on disk, and each phone must sync contacts again to populate its new cache. A two-device core test verifies contact and photo event identity. That test also exposed and prompted a fix for padded base64 photos being mistaken for quoted-printable continuation. `cargo test --workspace --locked --offline` passed 47 tests, with one compositor-dependent test ignored; `cargo check --workspace --locked --offline` passed. The changed D-Bus signal contract requires the service and SMS windows to restart together. Live two-phone behavior remains unverified.

4. ✅ **[P1] Replying to a displayed group sent to only one participant.** The applet now uses every distinct address in the displayed conversation roster, carries the list through Varlink/D-Bus, and puts all recipients in one `kdeconnect.sms.request` packet. Android excludes the phone owner's address from its message roster and expects the desktop to supply the targets; the send packet has no thread ID from which Android could recover them. The applet disables sending when a roster is empty or contains an unusable RCS/group token, rather than silently addressing one member. Tests verify recipient selection and the two-recipient packet; offline workspace tests passed 51 tests (one compositor-dependent test ignored), and `cargo check --workspace --locked --offline` passed. The changed IPC signature requires restarting the service and SMS window together. Actual group MMS/SMS delivery and RCS behavior still require a phone test.

5. ✅ **[P2] The SMS cache stored only the last response packet.** The service now merges incoming batches with the per-device cache by `(thread_id, message_id)` before updating memory and disk. New messages retain other threads; repeated message IDs replace older copies so read-state and message changes remain current. A cache miss loads the prior disk snapshot before merging, while the live D-Bus signal still carries only the new batch. Tests cover partial updates across threads, repeated IDs, and identical IDs in different threads. `cargo test --workspace --locked --offline` passed 53 tests with one compositor-dependent test ignored; `cargo check --workspace --locked --offline` passed. Previously overwritten history cannot be recovered from the old cache; the phone must resync it. Live reconnect/offline behavior remains unverified.

6. ✅ **[P2] A malformed file share left a permanently "receiving" transfer.** A file share missing payload transfer information now reports a terminal `Failed` status with stage `metadata` through the same transfer reporter that emitted `Receiving`. A regression test verifies the receiving-to-failed sequence and that no additional status follows. `cargo test --workspace --locked --offline` passed 56 tests with one compositor-dependent test ignored; `cargo check --workspace --locked --offline` passed. Live malformed-share behavior remains unverified.

7. ✅ **[P2] Payload-size enforcement happened too late to prevent disk exhaustion.** The receive loop now rejects the first read that would exceed the announced size before writing that chunk, and payload reception rejects a missing announced size before connecting. A clean EOF still must match the expected size. Tests verify over-limit rejection without waiting for EOF or writing past the limit, exact-size reception, and rejection of absent size metadata. `cargo test --workspace --locked --offline` passed 56 tests with one compositor-dependent test ignored; `cargo check --workspace --locked --offline` passed. Payloads without a size are now refused, including from older/nonconforming peers; live phone behavior remains unverified.

8. ✅ **[P2] Partial SMS updates could erase the accumulated group roster.** While fixing item 4, conversation updates were changed to retain known addresses and accept a new preview only when its timestamp is at least as recent. The message-batch path likewise no longer overwrites a newer preview with an older message. This prevents a partial response from shrinking the group recipient list. A regression test covers roster and preview preservation; offline workspace tests and check passed. Live phone behavior remains unverified.

**Initial review verification:** `cargo test --workspace --locked --offline` passed 41 tests, with one Wayland-dependent test ignored. The loopback-network test required execution outside the sandbox. `cargo check --workspace --locked --offline` also passed. The findings were based on source-path review; passing existing tests did not validate the uncovered cases or live-device behavior. Verification of the item 1 fix is recorded in that item.


I’d test in this order:
1. If the phone no longer appears paired, re-pair it once—the certificate-pinning change requires this for older pairing records.
2. Send a short message from a two-recipient group in the desktop SMS window and confirm both recipients receive it.
3. Open two SMS threads, then disconnect the phone and restart the SMS window and service. Confirm both threads still appear from the cache.
4. Share a small known file from phone to desktop. Confirm the saved file is complete, the transfer reaches Completed, and no partial file remains after a failed transfer. If you have a second phone, check that its contacts and SMS stay in its own window.
Watch tail -F ~/.local/share/kdeconnect/service.log during the pass. Ordinary phone sharing checks that transfers still work; the malformed-packet and over-limit cases need a custom sender and are covered by the offline regression tests.

## Upstream Merge - 2026-10-25

✅ Integrated upstream PR #119 (`d038146`, merged as `36a44d9`). KDE Connect uses the TCP initiator as TLS server; the accepted side must act as TLS client and must not send another plaintext identity. Resolved the transport conflict while preserving certificate extraction/pinning, the core acceptance gate, handshake bounds, packet limits, and queue backpressure. The new regression test checks both TLS roles, exact identity framing, both peer certificates, and encrypted traffic in both directions.

✅ Validation with all pre-existing local fixes restored: `cargo test --workspace --locked --offline` passed 57 tests with one compositor-dependent test ignored; `cargo check --workspace --locked --offline` and `git diff --check` passed. The loopback listener test ran outside the sandbox. The merge itself also passed 28 core tests and the workspace check before restoring those fixes.

⏳ Live phone connection/reconnection in both initiation directions remains unverified. No installation, service restart, or push was performed. Existing local changes remain uncommitted, and the pre-merge stash is retained as a backup.

## Repeated Phone Notifications - 2026-10-25

✅ **[P2] Repeated phone packets created repeated desktop popups.** The handler neither tracked notification IDs nor honored `silent`. Inspection of the two available logs found 631 repeated ID/content packets among 699 displayable packets, including a batch repeated about 28 minutes later. Added a registry-owned tracker keyed by authenticated phone ID and notification ID. It suppresses unchanged visible content and silent/legacy sync replies, survives reconnects, and permits changed content and cancelled IDs reused by the phone. Timestamp/icon/action changes alone do not trigger a popup. History holds 4,096 recent IDs for the service lifetime, stores content fingerprints, and resets on restart; identical text under the same active ID is treated as a replay. Display failures are logged and remain retryable. Full notification-body logging was removed.

✅ Six new regression tests cover delayed replay with refreshed metadata, changed content, multiple phones/IDs, silent sync and updates, cancellation/reuse, failed-display retry, incomplete packets, and bounded history. Workspace tests passed 63 tests with one compositor-dependent test ignored; the offline workspace check passed.

⏳ Install/restart and confirm fresh phone messages appear once, reconnect does not replay unchanged messages, and the 30–60 minute repeat no longer occurs. No installation or live notification-daemon validation was performed. Phone cancellation does not yet close a desktop notification; history is not persisted across service restarts.
