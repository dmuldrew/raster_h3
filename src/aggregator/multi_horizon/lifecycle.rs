//! Stream lifecycle state management and output record buffering.
//!
//! Provides explicit latched state transitions:
//! - `Running`: normal operation, accepting chunks and yielding records.
//! - `Finished`: stream successfully reached EOF, all chunks processed and all horizons drained.
//! - `Failed(String)`: stream encountered an unrecoverable failure (e.g. corrupt TIFF chunk,
//!   prefetch truncation). The failure reason is latched, and subsequent read attempts return
//!   the exact latched error rather than mistaking failure for normal EOF.

use std::collections::VecDeque;

use crate::error::{RasterH3Error, Result};

/// Latched lifecycle state of a streaming pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamState {
    Running,
    Finished,
    Failed(String),
}

/// Thread-safe / controller-managed lifecycle state machine with latched failure semantics.
#[derive(Debug, Clone)]
pub struct StreamLifecycle {
    state: StreamState,
}

impl StreamLifecycle {
    /// Create a new lifecycle tracker in the `Running` state.
    pub fn new() -> Self {
        Self {
            state: StreamState::Running,
        }
    }

    /// Check if the stream is currently active (neither finished nor failed).
    ///
    /// Returns:
    /// - `Ok(true)` if running
    /// - `Ok(false)` if finished
    /// - `Err(RasterH3Error::StreamFailed)` if latched in failure
    pub fn ensure_runnable(&self) -> Result<bool> {
        match &self.state {
            StreamState::Running => Ok(true),
            StreamState::Finished => Ok(false),
            StreamState::Failed(reason) => Err(RasterH3Error::StreamFailed(reason.clone())),
        }
    }

    /// Transition to `Failed` state and return the corresponding `RasterH3Error::StreamFailed`.
    ///
    /// If already failed, preserves the original failure reason.
    pub fn latch_failure(&mut self, reason: String) -> RasterH3Error {
        match &self.state {
            StreamState::Failed(existing) => RasterH3Error::StreamFailed(existing.clone()),
            _ => {
                self.state = StreamState::Failed(reason.clone());
                RasterH3Error::StreamFailed(reason)
            }
        }
    }

    /// Transition to `Finished` state if currently `Running`.
    pub fn mark_finished(&mut self) {
        if self.state == StreamState::Running {
            self.state = StreamState::Finished;
        }
    }

    /// Returns `true` if the stream completed successfully.
    pub fn is_finished(&self) -> bool {
        matches!(self.state, StreamState::Finished)
    }

    /// Returns `true` if the stream is in a failed state.
    pub fn is_failed(&self) -> bool {
        matches!(self.state, StreamState::Failed(_))
    }

    /// Returns the failure reason if the stream failed.
    pub fn failure_reason(&self) -> Option<&str> {
        match &self.state {
            StreamState::Failed(r) => Some(r.as_str()),
            _ => None,
        }
    }
}

impl Default for StreamLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// Buffer for holding completed output records awaiting consumer pickup.
#[derive(Debug)]
pub struct OutputBuffer<R> {
    records: VecDeque<R>,
}

impl<R> OutputBuffer<R> {
    /// Create a new empty output buffer with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(capacity),
        }
    }

    /// Push an output record onto the back of the buffer.
    pub fn push(&mut self, record: R) {
        self.records.push_back(record);
    }

    /// Number of records currently buffered.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns true if no records are buffered.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Pop the next completed record from the front of the buffer.
    pub fn pop_front(&mut self) -> Option<R> {
        self.records.pop_front()
    }

    /// Clear all records in the buffer.
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// Drain up to `max_rows` records into a vector.
    pub fn take_batch(&mut self, max_rows: usize) -> Vec<R> {
        let n = max_rows.min(self.records.len());
        let mut batch = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(r) = self.records.pop_front() {
                batch.push(r);
            }
        }
        batch
    }

    /// Drain up to `max_rows` records directly into a closure with zero intermediate allocation.
    pub fn drain_into<F>(&mut self, max_rows: usize, mut consumer: F) -> usize
    where
        F: FnMut(usize, R),
    {
        let n = max_rows.min(self.records.len());
        for i in 0..n {
            if let Some(r) = self.records.pop_front() {
                consumer(i, r);
            }
        }
        n
    }
}
