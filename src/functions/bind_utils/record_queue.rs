use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Multi-threaded batch queue for streaming DuckDB table functions
pub struct ConcurrentRecordQueue<R> {
    pub ready_batches: Mutex<VecDeque<Vec<R>>>,
    pub is_finished: AtomicBool,
}

impl<R> Default for ConcurrentRecordQueue<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> ConcurrentRecordQueue<R> {
    pub fn new() -> Self {
        Self {
            ready_batches: Mutex::new(VecDeque::new()),
            is_finished: AtomicBool::new(false),
        }
    }

    /// Retrieve next batch from pre-batched queue (~10ns lock) or refill from streamer under lock
    pub fn pop_or_refill<S, F>(&self, streamer: &Mutex<S>, mut drain_into: F) -> Option<Vec<R>>
    where
        F: FnMut(&mut S, usize, &mut dyn FnMut(usize, R)) -> usize,
    {
        // Fast path: check ready_batches
        let mut ready_q = match self.ready_batches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(b) = ready_q.pop_front() {
            return Some(b);
        }
        if self.is_finished.load(Ordering::Acquire) {
            return None;
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
            return Some(b);
        }
        if self.is_finished.load(Ordering::Acquire) {
            return None;
        }

        const BATCH_SIZE: usize = 2048;
        const REFILL_SIZE: usize = BATCH_SIZE * 4;

        let mut current_chunk = Vec::with_capacity(BATCH_SIZE);
        let mut my_batch = None;

        drain_into(&mut *streamer_guard, REFILL_SIZE, &mut |_i, rec| {
            current_chunk.push(rec);
            if current_chunk.len() == BATCH_SIZE {
                if my_batch.is_none() {
                    my_batch = Some(std::mem::replace(
                        &mut current_chunk,
                        Vec::with_capacity(BATCH_SIZE),
                    ));
                } else {
                    ready_q.push_back(std::mem::replace(
                        &mut current_chunk,
                        Vec::with_capacity(BATCH_SIZE),
                    ));
                }
            }
        });

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

        my_batch
    }
}
