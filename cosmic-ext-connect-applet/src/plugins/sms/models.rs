//! Data models for the SMS feature.

// #[allow(dead_code)] = Placeholder for code that will be used once features are fully integrated

#![allow(dead_code)]

use std::collections::HashMap;

/// Represents an SMS conversation thread.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub thread_id: String,
    pub contact_name: String,
    pub phone_number: String,
    /// All distinct participant addresses seen in this thread (more than
    /// one for group MMS/RCS), in first-seen order. The KDE Connect
    /// protocol has no group-name field, so group conversations are named
    /// from this list.
    pub addresses: Vec<String>,
    pub last_message: String,
    pub timestamp: i64,
    /// Used for future read/unread tracking
    pub unread: bool,
}

impl Conversation {
    /// Display name: the resolved contact/group name when available, the
    /// primary number otherwise.
    pub fn display_name(&self) -> String {
        if self.contact_name.is_empty() {
            self.phone_number.clone()
        } else {
            self.contact_name.clone()
        }
    }

    /// Android excludes the device's own address from each SMS message's
    /// address list. Do not silently send a displayed group to a subset when
    /// an RCS token or empty address makes its roster unusable.
    pub fn send_addresses(&self) -> Option<Vec<String>> {
        let mut recipients = Vec::new();
        for address in &self.addresses {
            let address = address.trim();
            let looks_like_phone = address.chars().any(|c| c.is_ascii_digit())
                && address.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | '(' | ')' | ' '));
            if address.is_empty() || !(address.contains('@') || looks_like_phone) {
                return None;
            }
            if !recipients.iter().any(|existing| existing == address) {
                recipients.push(address.to_string());
            }
        }
        if recipients.is_empty() { None } else { Some(recipients) }
    }

    pub fn merge_partial_update(&mut self, incoming: &Conversation) {
        for address in &incoming.addresses {
            if !self.addresses.contains(address) {
                self.addresses.push(address.clone());
            }
        }
        if incoming.timestamp >= self.timestamp {
            self.phone_number = incoming.phone_number.clone();
            self.last_message = incoming.last_message.clone();
            self.timestamp = incoming.timestamp;
        }
        self.unread |= incoming.unread;
    }
}

#[cfg(test)]
mod tests {
    use super::Conversation;

    fn conversation(addresses: &[&str]) -> Conversation {
        Conversation {
            thread_id: "1".into(), contact_name: String::new(),
            phone_number: String::new(), addresses: addresses.iter().map(|s| s.to_string()).collect(),
            last_message: String::new(), timestamp: 0, unread: false,
        }
    }

    #[test]
    fn group_send_uses_every_distinct_recipient() {
        assert_eq!(conversation(&["+1 555 111 0001", "+1 555 111 0002", "+1 555 111 0001"]).send_addresses(),
            Some(vec!["+1 555 111 0001".into(), "+1 555 111 0002".into()]));
    }

    #[test]
    fn group_send_rejects_unusable_roster() {
        assert_eq!(conversation(&["+15551110001", "rcs-group-123"]).send_addresses(), None);
        assert_eq!(conversation(&[]).send_addresses(), None);
    }

    #[test]
    fn partial_update_preserves_group_and_newest_preview() {
        let mut existing = conversation(&["+15551110001", "+15551110002"]);
        existing.timestamp = 200;
        existing.last_message = "newer".into();
        let mut partial = conversation(&["+15551110001"]);
        partial.timestamp = 100;
        partial.last_message = "older".into();
        existing.merge_partial_update(&partial);
        assert_eq!(existing.send_addresses().unwrap().len(), 2);
        assert_eq!(existing.timestamp, 200);
        assert_eq!(existing.last_message, "newer");
    }
}

/// One MMS attachment on a message. Starts out with just a thumbnail
/// preview (if the phone sent one); `full_path` is filled in later, once
/// `RequestFullAttachment` round-trips through `request_attachment` /
/// `attachment_file`.
#[derive(Debug, Clone)]
pub struct MessageAttachment {
    pub part_id: i64,
    /// Doubles as the filename used to correlate the async
    /// `attachment_file` response back to this attachment — see
    /// `kdeconnect_core::plugins::sms::SmsAttachmentFile`. Some
    /// attachments don't have one, in which case the full file can't be
    /// requested at all.
    pub unique_identifier: Option<String>,
    pub mime_type: String,
    /// Decoded base64 preview, if the phone sent one (already
    /// base64-decoded raw image bytes).
    pub thumbnail: Option<Vec<u8>>,
    /// Local path once the full-resolution file has been downloaded.
    pub full_path: Option<std::path::PathBuf>,
}

impl MessageAttachment {
    #[inline]
    pub fn is_video(&self) -> bool {
        self.mime_type.starts_with("video/")
    }
}

/// Represents an individual SMS message.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub body: String,
    pub address: String,
    pub date: i64,
    pub attachments: Vec<MessageAttachment>,
    /// Message type: 1 = received, 2 = sent
    pub type_: i32,
    /// Used for future read receipt tracking
    pub read: bool,
}

impl Message {
    /// Returns true if this is a sent message.
    #[inline]
    pub fn is_sent(&self) -> bool {
        self.type_ == 2
    }
}

/// Events received from the native protocol adapter.
#[derive(Debug, Clone)]
pub enum ProtocolEvent {
    /// Single live message (e.g. a real-time push from the phone).
    MessageReceived(Message),
    /// Bulk messages, typically the initial cache or a full thread sync.
    /// Processed in one batch to avoid O(n²) per-message sorts.
    MessagesReceived(Vec<Message>),
    ConversationsReceived(Vec<Conversation>),
    /// Used for error handling in event processing
    Error(String),
}

/// Type alias for contacts map (phone_number -> contact_name).
pub type ContactsMap = HashMap<String, String>;
