//! Per-session terminal output ring buffer (MCP PR-M4).
//!
//! Fed from the interactive PTY `on_data` path (post-filter, non-muted only)
//! so MCP `session_read_output` matches what the user sees.

use std::collections::VecDeque;

/// Default capacity per session (~128 KiB).
pub const DEFAULT_RING_CAPACITY: usize = 128 * 1024;

/// Bounded byte ring; oldest data is dropped when full.
#[derive(Debug, Clone)]
pub struct OutputRing {
    buf: VecDeque<u8>,
    capacity: usize,
    /// Total bytes ever pushed (including dropped).
    pub bytes_in: u64,
    /// True if any data was dropped due to capacity.
    pub ever_truncated: bool,
}

impl Default for OutputRing {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_RING_CAPACITY)
    }
}

impl OutputRing {
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            buf: VecDeque::with_capacity(capacity.min(64 * 1024)),
            capacity,
            bytes_in: 0,
            ever_truncated: false,
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.buf.clear();
        self.bytes_in = 0;
        self.ever_truncated = false;
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Append a chunk; drop oldest bytes in bulk when over capacity.
    pub fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.bytes_in = self.bytes_in.saturating_add(data.len() as u64);

        // Chunk alone fills/exceeds capacity — keep only its tail.
        if data.len() >= self.capacity {
            self.buf.clear();
            let start = data.len() - self.capacity;
            self.buf.extend(data[start..].iter().copied());
            self.ever_truncated = true;
            return;
        }

        let need = data.len();
        let room = self.capacity.saturating_sub(self.buf.len());
        if need > room {
            let drop_n = need - room;
            let drop_n = drop_n.min(self.buf.len());
            if drop_n > 0 {
                self.buf.drain(0..drop_n);
                self.ever_truncated = true;
            }
        }
        self.buf.extend(data.iter().copied());
        debug_assert!(self.buf.len() <= self.capacity);
    }

    /// Snapshot as UTF-8 lossy text, at most `max_bytes` from the **end** (recent).
    pub fn snapshot(&self, max_bytes: usize) -> RingSnapshot {
        let max_bytes = max_bytes.max(1);
        let len = self.buf.len();
        let take = len.min(max_bytes);
        let start = len - take;
        let bytes: Vec<u8> = self.buf.iter().skip(start).copied().collect();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let truncated = self.ever_truncated || take < len;
        RingSnapshot {
            text,
            truncated,
            available_bytes: len,
            returned_bytes: take,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RingSnapshot {
    pub text: String,
    pub truncated: bool,
    pub available_bytes: usize,
    pub returned_bytes: usize,
}

/// Push UI-visible terminal bytes into the session ring (no-op if lock poisoned).
pub fn push_session_output(rt: &crate::app_state::SessionRuntime, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    if let Ok(mut g) = rt.output_ring.lock() {
        g.push(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_drops_oldest() {
        let mut r = OutputRing::with_capacity(8);
        r.push(b"abcdefgh");
        assert_eq!(r.len(), 8);
        r.push(b"XY");
        let snap = r.snapshot(100);
        assert_eq!(snap.text, "cdefghXY");
        assert!(snap.truncated);
        assert!(r.ever_truncated);
    }

    #[test]
    fn bulk_chunk_larger_than_capacity() {
        let mut r = OutputRing::with_capacity(4);
        r.push(b"abcdefghij");
        assert_eq!(r.len(), 4);
        assert_eq!(r.snapshot(100).text, "ghij");
        assert!(r.ever_truncated);
    }

    #[test]
    fn snapshot_tail() {
        let mut r = OutputRing::with_capacity(64);
        r.push(b"hello world");
        let snap = r.snapshot(5);
        assert_eq!(snap.text, "world");
        assert!(snap.truncated);
    }

    #[test]
    fn empty_snapshot() {
        let r = OutputRing::default();
        let snap = r.snapshot(100);
        assert!(snap.text.is_empty());
        assert!(!snap.truncated);
    }
}
