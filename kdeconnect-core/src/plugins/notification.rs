use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, warn};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Notification {
    pub id: Option<String>,
    pub title: Option<String>,
    pub text: Option<String>,
    pub ticker: Option<String>,
    #[serde(rename = "appName")]
    pub app_name: Option<String>,
    #[serde(rename = "isClearable")]
    pub is_clearable: Option<bool>,
    pub silent: Option<bool>,
    #[serde(rename = "isCancel")]
    pub is_cancel: Option<bool>,
    #[serde(rename = "requestAnswer")]
    pub request_answer: Option<bool>,
    #[serde(rename = "requestReplyId")]
    pub request_reply_id: Option<String>,
    pub time: Option<String>,
    pub actions: Option<Vec<String>>,
    #[serde(rename = "payloadHash")]
    pub payload_hash: Option<String>,
}

// Keep history for the service lifetime, including across phone reconnects.
// A time-based expiry would recreate the periodic replay this prevents.
const MAX_TRACKED_NOTIFICATIONS: usize = 4096;

type NotificationKey = (String, String); // authenticated device ID, phone notification ID

#[derive(Default)]
struct NotificationHistory {
    // Retain fingerprints rather than private message contents or icon payloads.
    seen: HashMap<NotificationKey, (u64, u64)>, // content fingerprint, last-seen sequence
    sequence: u64,
}

impl NotificationHistory {
    fn should_display(&mut self, device_id: &str, notification: &Notification) -> bool {
        let Some(id) = notification.id.as_deref().filter(|id| !id.is_empty()) else {
            return false;
        };
        let key = (device_id.to_owned(), id.to_owned());
        if notification.is_cancel == Some(true) {
            self.seen.remove(&key);
            return false;
        }
        // Preserve the existing requirement for displayable title and text.
        let (Some(title), Some(text)) = (&notification.title, &notification.text) else {
            return false;
        };
        let mut hasher = DefaultHasher::new();
        notification
            .app_name
            .as_deref()
            .unwrap_or_default()
            .hash(&mut hasher);
        title.hash(&mut hasher);
        text.hash(&mut hasher);
        let fingerprint = hasher.finish();
        let unchanged = self
            .seen
            .get(&key)
            .is_some_and(|(old, _)| *old == fingerprint);
        if !self.seen.contains_key(&key) && self.seen.len() >= MAX_TRACKED_NOTIFICATIONS {
            if let Some(oldest) = self
                .seen
                .iter()
                .min_by_key(|(_, (_, sequence))| sequence)
                .map(|(key, _)| key.clone())
            {
                self.seen.remove(&oldest);
            }
        }
        self.sequence += 1;
        self.seen.insert(key, (fingerprint, self.sequence));
        // Phone timestamps, icon hashes, reply IDs and actions can change on
        // a resend without changing anything the user sees. Ignore them.
        !unchanged && notification.silent != Some(true) && notification.request_answer != Some(true)
    }

    fn forget(&mut self, device_id: &str, notification: &Notification) {
        if let Some(id) = &notification.id {
            self.seen.remove(&(device_id.to_owned(), id.clone()));
        }
    }
}

#[derive(Default)]
pub(crate) struct NotificationTracker {
    history: Mutex<NotificationHistory>,
}

impl NotificationTracker {
    pub async fn received_packet(&self, device_id: &str, notification: Notification) {
        // Serialize decision + display, so simultaneous dispatches cannot both
        // display the same notification. Registry clones share this tracker.
        let mut history = self.history.lock().await;
        if !history.should_display(device_id, &notification) {
            debug!(
                device_id,
                "[notification] skipped duplicate, silent, cancelled or incomplete packet"
            );
            return;
        }
        let mut notify = notify_rust::Notification::new();
        notify.appname(notification.app_name.as_deref().unwrap_or_default());
        notify.summary(notification.title.as_deref().unwrap_or_default());
        notify.body(notification.text.as_deref().unwrap_or_default());
        // COSMIC does not support wait_for_action; preserve the existing actions.
        for action in notification.actions.as_deref().unwrap_or_default() {
            notify.action(action, action);
        }
        match notify
            .hint(notify_rust::Hint::Resident(true))
            .show_async()
            .await
        {
            Ok(_) => debug!(device_id, "[notification] displayed new or changed content"),
            Err(error) => {
                // A failed display must not suppress a subsequent retry.
                history.forget(device_id, &notification);
                warn!(device_id, %error, "[notification] desktop display failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, text: &str) -> Notification {
        Notification {
            id: Some(id.into()),
            app_name: Some("Messages".into()),
            title: Some("Alice".into()),
            text: Some(text.into()),
            ..Default::default()
        }
    }

    #[test]
    fn replay_ignores_changed_timestamp_and_transport_metadata() {
        let mut history = NotificationHistory::default();
        let mut notification = message("thread-1", "Hello");
        notification.time = Some("1000".into());
        assert!(history.should_display("phone-a", &notification));
        notification.time = Some("3601000".into()); // replay an hour later
        notification.payload_hash = Some("new-icon".into());
        notification.request_reply_id = Some("new-reply-token".into());
        notification.actions = Some(vec!["Reply".into()]);
        assert!(!history.should_display("phone-a", &notification));
    }

    #[test]
    fn changed_message_and_distinct_ids_or_phones_still_display() {
        let mut history = NotificationHistory::default();
        let notification = message("thread-1", "Hello");
        assert!(history.should_display("phone-a", &notification));
        assert!(history.should_display("phone-b", &notification));
        assert!(history.should_display("phone-a", &message("thread-2", "Hello")));
        assert!(history.should_display("phone-a", &message("thread-1", "Another message")));
        assert!(!history.should_display("phone-a", &message("thread-1", "Another message")));
    }

    #[test]
    fn silent_sync_and_updates_do_not_popup_or_replay_as_new() {
        let mut history = NotificationHistory::default();
        let mut notification = message("thread-1", "Already on phone");
        notification.silent = Some(true);
        assert!(!history.should_display("phone-a", &notification));
        notification.silent = Some(false);
        assert!(!history.should_display("phone-a", &notification));
        notification.text = Some("New incoming message".into());
        assert!(history.should_display("phone-a", &notification));
        notification.text = Some("Silent update".into());
        notification.silent = Some(true);
        assert!(!history.should_display("phone-a", &notification));
        notification.silent = Some(false);
        assert!(!history.should_display("phone-a", &notification));
        let mut legacy = message("thread-2", "Old sync packet");
        legacy.request_answer = Some(true);
        assert!(!history.should_display("phone-a", &legacy));
    }

    #[test]
    fn cancellation_releases_only_the_source_phone_id_for_reuse() {
        let mut history = NotificationHistory::default();
        let notification = message("thread-1", "Hello");
        assert!(history.should_display("phone-a", &notification));
        assert!(history.should_display("phone-b", &notification));
        let cancel: Notification = serde_json::from_value(serde_json::json!({
            "id": "thread-1", "isCancel": true,
        }))
        .unwrap();
        assert!(!history.should_display("phone-a", &cancel));
        assert!(!history.should_display("phone-b", &notification));
        assert!(history.should_display("phone-a", &notification));
    }

    #[test]
    fn failed_display_can_retry_and_incomplete_packets_do_not_poison_history() {
        let mut history = NotificationHistory::default();
        assert!(!history.should_display("phone-a", &Notification::default()));
        let mut notification = message("thread-1", "Hello");
        notification.title = None;
        assert!(!history.should_display("phone-a", &notification));
        notification.title = Some("Alice".into());
        assert!(history.should_display("phone-a", &notification));
        history.forget("phone-a", &notification);
        assert!(history.should_display("phone-a", &notification));
    }

    #[test]
    fn history_is_bounded_and_repeated_ids_stay_recent() {
        let mut history = NotificationHistory::default();
        let first = message("0", "Hello");
        assert!(history.should_display("phone-a", &first));
        for id in 1..MAX_TRACKED_NOTIFICATIONS {
            assert!(history.should_display("phone-a", &message(&id.to_string(), "Hello")));
        }
        assert!(!history.should_display("phone-a", &first));
        assert!(history.should_display("phone-a", &message("extra", "Hello")));
        assert_eq!(history.seen.len(), MAX_TRACKED_NOTIFICATIONS);
        assert!(!history.should_display("phone-a", &first));
        assert!(history.should_display("phone-a", &message("1", "Hello")));
    }
}
