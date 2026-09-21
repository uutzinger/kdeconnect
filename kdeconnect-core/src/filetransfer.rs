use pin_project::pin_project;
use tokio::{
    io::AsyncRead,
    sync::mpsc::{self},
    time::{Duration, Interval, interval},
};

use crate::event::{ConnectionEvent, TransferDirection, TransferState, TransferStatus};

/// Minimum interval between progress events for one transfer, so a fast
/// receive loop can't flood the UI with per-chunk updates.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Reports one incoming transfer's progress and terminal result as
/// structured `TransferStatus` events. Progress sends are throttled to
/// `PROGRESS_MIN_INTERVAL`; terminal results are always sent exactly once.
pub struct IncomingTransfer {
    status: TransferStatus,
    tx: mpsc::UnboundedSender<ConnectionEvent>,
    last_progress_at: std::sync::Mutex<std::time::Instant>,
}

impl IncomingTransfer {
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<ConnectionEvent>,
        device_id: &crate::device::DeviceId,
        filename: Option<String>,
        expected_size: Option<u64>,
    ) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let id = format!(
            "{millis:x}-{:04x}",
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        Self {
            status: TransferStatus {
                transfer_id: id,
                device_id: device_id.0.clone(),
                direction: TransferDirection::Incoming,
                filename,
                expected_size,
                received_bytes: 0,
                state: TransferState::Receiving,
                saved_path: None,
            },
            tx,
            last_progress_at: std::sync::Mutex::new(
                std::time::Instant::now() - PROGRESS_MIN_INTERVAL,
            ),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.status.transfer_id
    }

    fn send(&self) {
        let _ = self.tx.send(ConnectionEvent::TransferStatus(Box::new(
            self.status.clone(),
        )));
    }

    /// First event: the request was accepted and reception is starting.
    pub(crate) fn started(&self) {
        self.send();
    }

    /// Throttled progress update; callable from a hot receive loop.
    pub(crate) fn progress(&self, received_bytes: u64) {
        let now = std::time::Instant::now();
        {
            let mut last = self.last_progress_at.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(*last) < PROGRESS_MIN_INTERVAL {
                return;
            }
            *last = now;
        }
        let mut status = self.status.clone();
        status.received_bytes = received_bytes;
        let _ = self
            .tx
            .send(ConnectionEvent::TransferStatus(Box::new(status)));
    }

    /// Terminal success. `saved_path` is the published destination.
    pub(crate) fn completed(mut self, saved_path: std::path::PathBuf, received_bytes: u64) {
        self.status.state = TransferState::Completed;
        self.status.saved_path = Some(saved_path.display().to_string());
        self.status.received_bytes = received_bytes;
        self.send();
    }

    /// Terminal failure at `stage` (machine-readable) with the full error
    /// chain in `reason`.
    pub(crate) fn failed(mut self, stage: &str, reason: String) {
        self.status.state = TransferState::Failed {
            stage: stage.to_string(),
            reason,
        };
        self.send();
    }
}

#[pin_project]
pub(crate) struct TransferAdapter<R: AsyncRead> {
    #[pin]
    inner: R,
    transfer_interval: Interval,
    transfer_bytes: usize,
    total_size: u64,
    processed_percent: u8,
    pub(crate) notify_tx: mpsc::UnboundedSender<ConnectionEvent>,
}

impl<R: AsyncRead> TransferAdapter<R> {
    pub fn new(
        inner: R,
        total_size: u64,
        connection_tx: mpsc::UnboundedSender<ConnectionEvent>,
    ) -> Self {
        Self {
            inner,
            transfer_interval: interval(Duration::from_millis(100)),
            transfer_bytes: 0,
            total_size,
            processed_percent: 0,
            notify_tx: connection_tx,
        }
    }
}

impl<R: AsyncRead> AsyncRead for TransferAdapter<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.project();
        let before = buf.filled().len();
        let result = this.inner.poll_read(cx, buf);
        let filled_len = buf.filled().len() - before;

        *this.transfer_bytes += filled_len;
        *this.processed_percent =
            calculate_progress(*this.transfer_bytes as f64, *this.total_size as f64);

        // Emit 100% immediately on EOF (zero-byte read) so small/fast
        // transfers always surface a completion update regardless of the
        // 100 ms ticker cadence.
        if matches!(result, std::task::Poll::Ready(Ok(()))) && filled_len == 0 {
            send_progress(100, this.notify_tx.clone());
            return result;
        }

        match this.transfer_interval.poll_tick(cx) {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(_) => {
                send_progress(*this.processed_percent, this.notify_tx.clone());
            }
        }

        result
    }
}

fn calculate_progress(transferred: f64, total: f64) -> u8 {
    if total > 0.0 && transferred > 0.0 {
        (transferred / total * 100.0).round().min(100.0) as u8
    } else {
        0
    }
}

pub(crate) fn send_progress(percent: u8, notify_tx: mpsc::UnboundedSender<ConnectionEvent>) {
    let _ = notify_tx.send(ConnectionEvent::UpdateTransferProgress(percent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceId;
    use crate::event::TransferState;

    fn collect(rx: &mut mpsc::UnboundedReceiver<ConnectionEvent>) -> Vec<TransferStatus> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let ConnectionEvent::TransferStatus(status) = event {
                out.push(*status);
            }
        }
        out
    }

    #[test]
    fn reporter_throttles_progress_but_always_delivers_terminal_states() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let transfer = IncomingTransfer::new(
            tx,
            &DeviceId("dev".into()),
            Some("photo.jpg".into()),
            Some(100),
        );

        transfer.started();
        // First progress send goes through (initialized past the throttle
        // window); the immediate second one is throttled away.
        transfer.progress(10);
        transfer.progress(20);
        transfer.completed(std::path::PathBuf::from("/downloads/photo.jpg"), 100);

        let events = collect(&mut rx);
        assert_eq!(events.len(), 3, "{events:?}");

        assert_eq!(events[0].state, TransferState::Receiving);
        assert_eq!(events[0].received_bytes, 0);

        assert_eq!(events[1].state, TransferState::Receiving);
        assert_eq!(events[1].received_bytes, 10);

        assert_eq!(events[2].state, TransferState::Completed);
        assert_eq!(events[2].received_bytes, 100);
        assert_eq!(
            events[2].saved_path.as_deref(),
            Some("/downloads/photo.jpg")
        );

        // Every event shares one transfer ID and carries device/filename.
        for event in &events {
            assert_eq!(event.device_id, "dev");
            assert_eq!(event.filename.as_deref(), Some("photo.jpg"));
            assert_eq!(event.transfer_id, events[0].transfer_id);
        }
    }

    #[test]
    fn reporter_failure_carries_stage_and_reason_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let transfer = IncomingTransfer::new(tx, &DeviceId("dev".into()), None, None);

        transfer.started();
        transfer.failed("receive", "connection refused".into());

        let events = collect(&mut rx);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].state,
            TransferState::Failed {
                stage: "receive".into(),
                reason: "connection refused".into()
            }
        );
        assert_eq!(events[1].filename, None);
    }
}
