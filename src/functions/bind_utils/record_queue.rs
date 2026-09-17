use crate::error::Result;
use crate::ffi::get_vector_size;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Multi-threaded batch queue for streaming DuckDB table functions
pub struct ConcurrentRecordQueue<R> {
    pub ready_batches: Mutex<VecDeque<Vec<R>>>,
    pub is_finished: AtomicBool,
    pub batch_size: usize,
}

impl<R> Default for ConcurrentRecordQueue<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> ConcurrentRecordQueue<R> {
    pub fn new() -> Self {
        Self::with_batch_size(get_vector_size())
    }

    pub fn with_batch_size(batch_size: usize) -> Self {
        Self {
            ready_batches: Mutex::new(VecDeque::new()),
            is_finished: AtomicBool::new(false),
            batch_size: batch_size.max(1),
        }
    }

    /// Retrieve next batch from pre-batched queue (~10ns lock) or refill from streamer under lock
    pub fn pop_or_refill<S, F>(
        &self,
        streamer: &Mutex<S>,
        mut drain_into: F,
    ) -> Result<Option<Vec<R>>>
    where
        F: FnMut(&mut S, usize, &mut dyn FnMut(usize, R)) -> Result<usize>,
    {
        // Fast path: check ready_batches
        let mut ready_q = match self.ready_batches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(b) = ready_q.pop_front() {
            return Ok(Some(b));
        }
        if self.is_finished.load(Ordering::Acquire) {
            return Ok(None);
        }
        drop(ready_q);

        // Slow path: acquire streamer lock to refill
        let mut streamer_guard = match streamer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let mut ready_q = match self.ready_batches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(b) = ready_q.pop_front() {
            return Ok(Some(b));
        }
        if self.is_finished.load(Ordering::Acquire) {
            return Ok(None);
        }

        let batch_size = self.batch_size;
        let refill_size = batch_size * 4;

        let mut current_chunk = Vec::with_capacity(batch_size);
        let mut my_batch = None;

        let result = drain_into(&mut *streamer_guard, refill_size, &mut |_i, rec| {
            current_chunk.push(rec);
            if current_chunk.len() == batch_size {
                if my_batch.is_none() {
                    my_batch = Some(std::mem::replace(
                        &mut current_chunk,
                        Vec::with_capacity(batch_size),
                    ));
                } else {
                    ready_q.push_back(std::mem::replace(
                        &mut current_chunk,
                        Vec::with_capacity(batch_size),
                    ));
                }
            }
        });
        if let Err(error) = result {
            ready_q.clear();
            return Err(error);
        }

        if !current_chunk.is_empty() {
            if my_batch.is_none() {
                my_batch = Some(current_chunk);
            } else {
                ready_q.push_back(current_chunk);
            }
        }

        if my_batch.is_none() {
            self.is_finished.store(true, Ordering::Release);
        }

        Ok(my_batch)
    }
}
