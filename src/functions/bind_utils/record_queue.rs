use crate::error::Result;
use crate::ffi::get_vector_size;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Demand-driven batch queue for streaming DuckDB table functions.
/// The drain callback must emit at most its requested row limit. Under that
/// contract, a refill leaves at most three batches queued plus the returned batch.
/// No background refill occurs while DuckDB scan callbacks are paused.
pub struct ConcurrentRecordQueue<R> {
    pub ready_batches: Mutex<VecDeque<Vec<R>>>,
    pub is_finished: AtomicBool,
    pub batch_size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::prefetch::OrderedPrefetchQueue;
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn paused_record_consumer_stalls_prefetch_and_resumes_in_order() {
        const TOTAL: usize = 15_840;
        const CAPACITY: usize = 16;
        const BATCH: usize = 8;
        let prefetch = Arc::new(OrderedPrefetchQueue::new(CAPACITY, TOTAL));
        let producer_queue = Arc::clone(&prefetch);
        let producer = thread::spawn(move || {
            for job in 0..TOTAL {
                if !producer_queue.push(job, job) {
                    break;
                }
            }
        });

        let consumer_queue = Arc::clone(&prefetch);
        let (tx, rx) = mpsc::channel();
        let consumer = thread::spawn(move || {
            let records = ConcurrentRecordQueue::with_batch_size(BATCH);
            let streamer = Mutex::new(consumer_queue.clone());
            // One synthetic output record per decoded chunk isolates the real
            // queue-to-queue flow without TIFF, H3 or DuckDB FFI dependencies.
            let drain = |q: &mut Arc<OrderedPrefetchQueue<usize>>,
                         limit: usize,
                         emit: &mut dyn FnMut(usize, usize)| {
                let mut chunks = Vec::new();
                let count = q.drain_into(&mut chunks, limit, limit);
                for (index, chunk) in chunks.into_iter().enumerate() {
                    emit(index, chunk);
                }
                Ok(count)
            };

            let first = records.pop_or_refill(&streamer, drain).unwrap().unwrap();
            assert_eq!(first, (0..BATCH).collect::<Vec<_>>());
            assert_eq!(records.ready_batches.lock().unwrap().len(), 3);

            // Exactly 4*BATCH chunks were consumed. Without another scan call,
            // the producer can deposit only CAPACITY more before it must wait.
            assert!(consumer_queue.wait_for_blocked_job(4 * BATCH + CAPACITY));
            assert_eq!(records.ready_batches.lock().unwrap().len(), 3);

            let mut expected = BATCH;
            while let Some(batch) = records.pop_or_refill(&streamer, drain).unwrap() {
                assert!(batch.len() <= BATCH);
                assert!(records.ready_batches.lock().unwrap().len() <= 3);
                for value in batch {
                    assert_eq!(value, expected);
                    expected += 1;
                }
            }
            assert_eq!(expected, TOTAL);
            tx.send(()).unwrap();
        });

        let result = rx.recv_timeout(Duration::from_secs(10));
        prefetch.close();
        let consumer_result = consumer.join();
        let producer_result = producer.join();
        result.expect("paused-consumer backpressure test did not complete");
        consumer_result.unwrap();
        producer_result.unwrap();
    }
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
