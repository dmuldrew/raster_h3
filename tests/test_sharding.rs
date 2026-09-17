//! Shard balance and aggregation correctness for structured H3 indices.
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;

use h3o::{LatLng, Resolution};
use raster_h3::aggregator::accumulator::H3Accumulator;
use raster_h3::aggregator::categorical::CategoricalAccumulator;
use raster_h3::aggregator::horizon_streamer::compute_cell_south_lat;
use raster_h3::aggregator::multi_horizon::{
    get_shard, AccumulatorMerge, ShardedResolutionMap, NUM_SHARDS,
};

fn global_cells(res: Resolution) -> HashSet<u64> {
    (-75..75)
        .step_by(3)
        .flat_map(|lat| {
            (-180..180).step_by(3).map(move |lon| {
                LatLng::new(lat as f64 + 0.123, lon as f64 + 0.321)
                    .unwrap()
                    .to_cell(res)
                    .into()
            })
        })
        .collect()
}

fn distribution(cells: &HashSet<u64>) -> [usize; NUM_SHARDS] {
    let mut counts = [0; NUM_SHARDS];
    for &cell in cells {
        counts[get_shard(cell)] += 1;
    }
    counts
}

#[test]
fn shard_distribution_across_all_h3_resolutions() {
    assert!(NUM_SHARDS.is_power_of_two());
    for res in 0..=15 {
        let cells = global_cells(Resolution::try_from(res).unwrap());
        let counts = distribution(&cells);
        let occupied = counts.iter().filter(|&&n| n > 0).count();
        let average_ceiling = cells.len().div_ceil(NUM_SHARDS);
        // Resolution 0 has only 122 cells; a few empty buckets are reasonable.
        assert!(
            occupied >= if res == 0 { 24 } else { NUM_SHARDS },
            "res {res}: {counts:?}"
        );
        assert!(
            *counts.iter().max().unwrap() <= 3 * average_ceiling,
            "res {res}: {counts:?}"
        );
        println!(
            "res {res}: {} distinct cells, {occupied} shards, min {}, max {}",
            cells.len(),
            counts.iter().min().unwrap(),
            counts.iter().max().unwrap()
        );
    }
}

#[test]
fn shard_distribution_in_local_raster_neighborhoods() {
    for res in 2..=15 {
        let res = Resolution::try_from(res).unwrap();
        for (lat, lon) in [
            (37.77, -122.42),
            (80.0, 25.0),
            (-74.877, -153.679),
            (10.0, 179.999),
        ] {
            let center = LatLng::new(lat, lon).unwrap().to_cell(res);
            let cells: HashSet<u64> = center
                .grid_disk::<Vec<_>>(16)
                .into_iter()
                .map(Into::into)
                .collect();
            let counts = distribution(&cells);
            assert!(
                counts.iter().all(|&n| n > 0),
                "{res:?} at {lat},{lon}: {counts:?}"
            );
            assert!(
                *counts.iter().max().unwrap() <= 2 * cells.len().div_ceil(NUM_SHARDS),
                "{res:?} at {lat},{lon}: {counts:?}"
            );
        }
    }
}

fn check_merge_and_eviction<A: AccumulatorMerge + PartialEq + Debug>(first: A, second: A) {
    let cells = global_cells(Resolution::Eight);
    let mut expected_value = first.clone();
    expected_value.merge(&second);
    let expected: HashMap<u64, A> = cells
        .iter()
        .map(|&cell| (cell, expected_value.clone()))
        .collect();
    let results: Vec<_> = [first, second]
        .into_iter()
        .map(|value| {
            let mut shards: [Vec<(u64, A)>; NUM_SHARDS] = std::array::from_fn(|_| Vec::new());
            for &cell in &cells {
                shards[get_shard(cell)].push((cell, value.clone()));
            }
            // A second resolution slot checks that the requested slot is used.
            (vec![std::array::from_fn(|_| Vec::new()), shards], ())
        })
        .collect();

    for threads in [1, 4] {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| {
                let mut map = ShardedResolutionMap::new();
                map.merge_thread_results(&results, 0);
                assert_eq!(map.active_cell_count(), 0);
                map.merge_thread_results(&results, 1);
                assert_eq!(map.active_cell_count(), cells.len());
                assert!(map.shards.iter().all(|shard| !shard.is_empty()));
                for (shard, heap) in map.shards.iter().zip(&map.eviction) {
                    assert_eq!(
                        shard.len(),
                        heap.len(),
                        "one eviction entry per cell after merging"
                    );
                }
                let mut evicted = map.evict_completed(0.0);
                assert!(!evicted.is_empty());
                assert!(evicted
                    .iter()
                    .all(|(cell, _)| compute_cell_south_lat(*cell) > 0.0));
                assert!(evicted.windows(2).all(|w| w[0].0 < w[1].0));
                assert_eq!(map.active_cell_count(), cells.len() - evicted.len());
                assert!(map.evict_completed(0.0).is_empty());
                let remaining = map.drain_all();
                assert!(!remaining.is_empty());
                assert!(remaining
                    .iter()
                    .all(|(cell, _)| compute_cell_south_lat(*cell) <= 0.0));
                assert!(remaining.windows(2).all(|w| w[0].0 < w[1].0));
                evicted.extend(remaining);
                assert_eq!(evicted.len(), cells.len(), "no missing or duplicate cells");
                assert_eq!(evicted.into_iter().collect::<HashMap<_, _>>(), expected);
                assert_eq!(map.active_cell_count(), 0);
                assert!(map.eviction.iter().all(|heap| heap.is_empty()));
            });
    }
}

#[test]
fn continuous_sharded_merge_and_eviction_preserve_statistics() {
    check_merge_and_eviction(H3Accumulator::new(2.0), H3Accumulator::new(4.0));
}

#[test]
fn categorical_sharded_merge_and_eviction_preserve_histograms() {
    let mut first = CategoricalAccumulator::new();
    let mut second = CategoricalAccumulator::new();
    for category in 0..20 {
        first.update(category);
        second.update_weighted(category, 0.5);
    }
    check_merge_and_eviction(first, second);
}
