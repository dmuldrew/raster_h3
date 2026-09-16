/// Parse a list of H3 resolutions from a string (comma/whitespace separated, sorted, deduplicated, <= 15)
pub fn parse_resolutions_str(s: &str) -> Vec<u8> {
    let mut list: Vec<u8> = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .filter_map(|item| item.parse::<u8>().ok())
        .filter(|&r| r <= 15)
        .collect();
    list.sort_unstable();
    list.dedup();
    list
}

/// Parse bounding box coordinates from a comma/whitespace separated string `[min_lon, min_lat, max_lon, max_lat]`
pub fn parse_bbox_str(s: &str) -> Option<[f64; 4]> {
    let coords: Vec<f64> = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .filter_map(|item| item.parse::<f64>().ok())
        .collect();
    if coords.len() == 4 {
        Some([coords[0], coords[1], coords[2], coords[3]])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    use crate::functions::bind_utils::lifecycle::{
        delete_boxed, estimate_raster_cardinality, TableFunctionLocalData,
    };
    use crate::functions::bind_utils::record_queue::ConcurrentRecordQueue;

    #[test]
    fn test_parse_resolutions_str() {
        assert_eq!(parse_resolutions_str("7,8"), vec![7, 8]);
        assert_eq!(parse_resolutions_str("8, 7, 8"), vec![7, 8]);
        assert_eq!(parse_resolutions_str("  6  7  8  "), vec![6, 7, 8]);
        assert_eq!(parse_resolutions_str("0,15,16,99"), vec![0, 15]);
        assert_eq!(parse_resolutions_str("invalid,foo"), Vec::<u8>::new());
        assert_eq!(parse_resolutions_str(""), Vec::<u8>::new());
    }

    #[test]
    fn test_parse_bbox_str() {
        assert_eq!(
            parse_bbox_str("-122.5,37.5,-122.0,38.0"),
            Some([-122.5, 37.5, -122.0, 38.0])
        );
        assert_eq!(
            parse_bbox_str("  -122.5   37.5   -122.0   38.0  "),
            Some([-122.5, 37.5, -122.0, 38.0])
        );
        assert_eq!(parse_bbox_str("-122.5,37.5,-122.0"), None);
        assert_eq!(parse_bbox_str("not,a,bbox,coords"), None);
        assert_eq!(parse_bbox_str(""), None);
    }

    #[test]
    fn test_estimate_raster_cardinality_fallback() {
        let empty_paths: Vec<PathBuf> = Vec::new();
        let resolutions = vec![8, 9];
        assert_eq!(
            estimate_raster_cardinality(&empty_paths, &resolutions),
            20_000
        );

        let non_existent = vec![PathBuf::from("/non/existent/path.tif")];
        assert_eq!(
            estimate_raster_cardinality(&non_existent, &resolutions),
            20_000
        );
    }

    #[test]
    fn test_delete_boxed_and_local_data() {
        struct TestDropCounter(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for TestDropCounter {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let boxed = Box::new(TestDropCounter(dropped.clone()));
        let raw = Box::into_raw(boxed) as *mut c_void;
        unsafe {
            delete_boxed::<TestDropCounter>(raw);
            delete_boxed::<TestDropCounter>(std::ptr::null_mut());
        }
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));

        let local = TableFunctionLocalData::default();
        assert_eq!(local.thread_id, 0);
        assert_eq!(local.hex_buf.len(), 16);
        assert_eq!(local.wkb_buf.len(), 128);
    }

    #[test]
    fn test_concurrent_record_queue_pop_and_refill() {
        let queue = ConcurrentRecordQueue::<u64>::new();
        let streamer = Mutex::new(vec![10u64, 20, 30, 40, 50]);

        // First refill
        let b1 = queue.pop_or_refill(&streamer, |s, max_rows, f| {
            let num = max_rows.min(s.len());
            let taken: Vec<u64> = s.drain(..num).collect();
            for (i, v) in taken.into_iter().enumerate() {
                f(i, v);
            }
            num
        });
        assert_eq!(b1, Some(vec![10, 20, 30, 40, 50]));

        // Second refill at EOF
        let b2 = queue.pop_or_refill(&streamer, |s, max_rows, f| {
            let num = max_rows.min(s.len());
            let taken: Vec<u64> = s.drain(..num).collect();
            for (i, v) in taken.into_iter().enumerate() {
                f(i, v);
            }
            num
        });
        assert_eq!(b2, None);
        assert!(queue.is_finished.load(Ordering::Acquire));
    }
}
