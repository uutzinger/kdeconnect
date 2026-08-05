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
    pub last_message: String,
    pub timestamp: i64,
    /// Used for future read/unread tracking
    pub unread: bool,
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
