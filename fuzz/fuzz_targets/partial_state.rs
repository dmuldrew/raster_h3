#![no_main]

use libfuzzer_sys::fuzz_target;
use raster_h3_core::aggregator::multi_horizon::{decode_partial_state, encode_partial_state};
use raster_h3_core::aggregator::{CategoricalAccumulator, H3Accumulator};

/// Byte budget handed to the decoder, as the aggregation budget would be.
const LIMIT: usize = 1 << 20;

fuzz_target!(|data: &[u8]| {
    // Partial states arrive from other workers (Spark, Python `merge_partials`): decoding
    // must reject or accept arbitrary bytes without panicking, and an accepted state must
    // survive its own encoding unchanged.
    if let Ok(acc) = decode_partial_state::<H3Accumulator>(data, LIMIT) {
        let again = decode_partial_state::<H3Accumulator>(&encode_partial_state(&acc), LIMIT)
            .expect("re-decode");
        let bits =
            |a: &H3Accumulator| [a.sum, a.count, a.sum_w2, a.min, a.max, a.m2].map(f64::to_bits);
        assert_eq!(bits(&acc), bits(&again));
        assert_eq!(acc.quantiles, again.quantiles);
    }
    if let Ok(acc) = decode_partial_state::<CategoricalAccumulator>(data, LIMIT) {
        let again =
            decode_partial_state::<CategoricalAccumulator>(&encode_partial_state(&acc), LIMIT)
                .expect("re-decode");
        assert_eq!(acc.total_count.to_bits(), again.total_count.to_bits());
        assert_eq!(acc.unique_classes(), again.unique_classes());
        acc.for_each_class(|class, weight| assert_eq!(again.get_class_count(class), weight));
    }
});
