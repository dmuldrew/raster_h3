use crate::aggregator::cell_hash::H3BuildHasher;
use std::collections::HashMap;
use tiff::decoder::DecodingResult;

use crate::aggregator::categorical::{CategoricalAccumulator, CategoricalUniformity};
use crate::aggregator::nodata::PixelValidity;
use crate::aggregator::remap::CategoryRemapper;

use super::walker::{
    is_slice_all_native_nodata, is_transparent, walk_direct, walk_interleaved, ScanlineEngine,
    WalkContext,
};
/// Categorical record yielded by the multi-resolution categorical streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiCategoricalRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: CategoricalAccumulator,
}

/// One output row of the long categorical format: a single (cell, category) pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CategoricalLongRow {
    pub cell_u64: u64,
    pub resolution: u8,
    pub category: i64,
    pub count: f64,
    pub fraction: f64,
    pub total_count: f64,
    /// Shannon entropy of the cell, or `NaN` when the cursor was built without entropy.
    pub entropy: f64,
    pub distinct_classes: i64,
}

/// Incrementally expands one categorical record at a time into long-format rows.
///
/// Holding a single cell's sorted categories (instead of eagerly expanding a
/// whole batch of cells) bounds pending output state by the largest cell's
/// cardinality rather than `cells × categories`. The entry buffer is reused
/// across cells.
#[derive(Debug, Default)]
pub struct CategoricalLongCursor {
    with_entropy: bool,
    cell_u64: u64,
    resolution: u8,
    total_count: f64,
    entropy: f64,
    distinct_classes: i64,
    entries: Vec<(i64, f64)>,
    next: usize,
}

impl CategoricalLongCursor {
    /// Create a cursor; `with_entropy` controls whether Shannon entropy is computed per cell.
    pub fn new(with_entropy: bool) -> Self {
        Self {
            with_entropy,
            ..Default::default()
        }
    }

    /// Replace any remaining rows with the categories of `rec`, sorted by category.
    pub fn load(&mut self, rec: &MultiCategoricalRecord) {
        let acc = &rec.accumulator;
        self.cell_u64 = rec.h3_index;
        self.resolution = rec.resolution;
        self.total_count = acc.total_count;
        self.distinct_classes = acc.unique_classes() as i64;
        self.entropy = if self.with_entropy {
            acc.shannon_entropy()
        } else {
            f64::NAN
        };
        self.entries.clear();
        self.next = 0;
        acc.for_each_class(|cat, cnt| self.entries.push((cat, cnt)));
        self.entries.sort_unstable_by_key(|&(cat, _)| cat);
    }

    /// Rows of the current cell not yet emitted.
    pub fn remaining(&self) -> usize {
        self.entries.len() - self.next
    }

    /// Emit up to `max_rows` rows of the current cell, returning how many were emitted.
    pub fn emit_into<F: FnMut(CategoricalLongRow)>(&mut self, max_rows: usize, mut f: F) -> usize {
        let take = max_rows.min(self.remaining());
        let total = self.total_count;
        for &(category, count) in &self.entries[self.next..self.next + take] {
            f(CategoricalLongRow {
                cell_u64: self.cell_u64,
                resolution: self.resolution,
                category,
                count,
                fraction: if total > 0.0 { count / total } else { 0.0 },
                total_count: total,
                entropy: self.entropy,
                distinct_classes: self.distinct_classes,
            });
        }
        self.next += take;
        take
    }
}

/// Process a single typed chunk slice for categorical landcover aggregation across resolutions
fn process_categorical_slice_into_maps<T: CategoricalUniformity + Default>(
    ctx: &WalkContext<'_>,
    slice: &[T],
    native_nodata: Option<T>,
    band: usize,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, H3BuildHasher>],
) {
    let engine = CategoricalEngine::new(native_nodata, remapper);
    let samples_per_pixel = ctx.samples_per_pixel as usize;
    if samples_per_pixel <= 1 {
        walk_direct(
            ctx,
            slice,
            |s| is_slice_all_native_nodata(s, native_nodata),
            &engine,
            chunk_maps,
        );
    } else {
        let b = band.saturating_sub(1).min(samples_per_pixel - 1);
        walk_interleaved(
            ctx,
            slice,
            |px| {
                if is_transparent(ctx, px) {
                    None
                } else {
                    engine.get_sample(px[b])
                }
            },
            &engine,
            chunk_maps,
        );
    }
}

struct CategoricalEngine<'a, T> {
    native_nodata: Option<T>,
    remapper: Option<&'a CategoryRemapper>,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: CategoricalUniformity> CategoricalEngine<'a, T> {
    fn new(native_nodata: Option<T>, remapper: Option<&'a CategoryRemapper>) -> Self {
        Self {
            native_nodata,
            remapper,
            _marker: std::marker::PhantomData,
        }
    }

    #[inline(always)]
    fn resolve_category(&self, val: T) -> Option<i64> {
        let rule = PixelValidity::new(self.native_nodata);
        if !rule.is_valid(val) {
            return None;
        }
        let raw_cat = val.to_category()?;
        if let Some(rem) = self.remapper {
            rem.remap(raw_cat)
        } else {
            Some(raw_cat)
        }
    }
}

impl<'a, T: CategoricalUniformity> ScanlineEngine<T, CategoricalAccumulator>
    for CategoricalEngine<'a, T>
{
    type Sample = i64;

    fn combine_sample_weights(&self) -> bool {
        true
    }

    #[inline(always)]
    fn new_acc(&self) -> CategoricalAccumulator {
        CategoricalAccumulator::default()
    }

    #[inline(always)]
    fn clear_acc(&self, acc: &mut CategoricalAccumulator) {
        *acc = CategoricalAccumulator::default();
    }

    #[inline(always)]
    fn has_samples(&self, acc: &CategoricalAccumulator) -> bool {
        acc.total_count > 0.0
    }

    #[inline(always)]
    fn merge_acc(&self, dest: &mut CategoricalAccumulator, src: &CategoricalAccumulator) {
        dest.merge(src);
    }

    #[inline(always)]
    fn accumulate_span(&self, acc: &mut CategoricalAccumulator, sub_slice: &[T]) {
        if sub_slice.is_empty() {
            return;
        }
        let first_val = sub_slice[0];
        let is_uniform = T::is_uniform(sub_slice);

        if is_uniform {
            if let Some(cat) = self.resolve_category(first_val) {
                acc.update_weighted(cat, sub_slice.len() as f64);
            }
        } else {
            let mut curr_cat: Option<i64> = None;
            let mut curr_cat_count: f64 = 0.0;

            for &val_raw in sub_slice {
                let cat = match self.resolve_category(val_raw) {
                    Some(c) => c,
                    None => continue,
                };
                if Some(cat) == curr_cat {
                    curr_cat_count += 1.0;
                } else {
                    if let Some(prev) = curr_cat {
                        acc.update_weighted(prev, curr_cat_count);
                    }
                    curr_cat = Some(cat);
                    curr_cat_count = 1.0;
                }
            }
            if let Some(last_cat) = curr_cat {
                if curr_cat_count > 0.0 {
                    acc.update_weighted(last_cat, curr_cat_count);
                }
            }
        }
    }

    #[inline(always)]
    fn accumulate_span_multi(&self, run_accs: &mut [CategoricalAccumulator], sub_slice: &[T]) {
        if sub_slice.is_empty() {
            return;
        }
        let first_val = sub_slice[0];
        let is_uniform = T::is_uniform(sub_slice);

        if is_uniform {
            if let Some(cat) = self.resolve_category(first_val) {
                let weight = sub_slice.len() as f64;
                for acc in run_accs.iter_mut() {
                    acc.update_weighted(cat, weight);
                }
            }
        } else {
            let mut curr_cat: Option<i64> = None;
            let mut curr_cat_count: f64 = 0.0;

            for &val_raw in sub_slice {
                let cat = match self.resolve_category(val_raw) {
                    Some(c) => c,
                    None => continue,
                };
                if Some(cat) == curr_cat {
                    curr_cat_count += 1.0;
                } else {
                    if let Some(prev) = curr_cat {
                        for acc in run_accs.iter_mut() {
                            acc.update_weighted(prev, curr_cat_count);
                        }
                    }
                    curr_cat = Some(cat);
                    curr_cat_count = 1.0;
                }
            }

            if let Some(last_cat) = curr_cat {
                if curr_cat_count > 0.0 {
                    for acc in run_accs.iter_mut() {
                        acc.update_weighted(last_cat, curr_cat_count);
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn get_sample(&self, pixel: T) -> Option<i64> {
        self.resolve_category(pixel)
    }

    #[inline(always)]
    fn update_sample(&self, acc: &mut CategoricalAccumulator, sample: i64, weight: f64) {
        acc.update_weighted(sample, weight);
    }
}

/// Process a categorical chunk across all resolutions into thread-local hash maps.
/// Returns false when a single-band chunk is entirely nodata.
pub fn process_categorical_chunk_payload_into(
    window: &WalkContext<'_>,
    decoding_result: &DecodingResult,
    nodata: Option<f64>,
    band: usize,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, H3BuildHasher>],
) -> bool {
    process_categorical_borrowed_into(
        window,
        super::borrowed::BorrowedSamples::from(decoding_result),
        nodata,
        band,
        remapper,
        chunk_maps,
    )
}

/// Borrowed-window form of [`process_categorical_chunk_payload_into`].
pub fn process_categorical_borrowed_into(
    window: &WalkContext<'_>,
    samples: super::borrowed::BorrowedSamples<'_>,
    nodata: Option<f64>,
    band: usize,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, H3BuildHasher>],
) -> bool {
    if window.samples_per_pixel <= 1 && samples.all_nodata(nodata) {
        return false;
    }
    crate::dispatch_samples!(samples, nodata, |slice, nd| {
        process_categorical_slice_into_maps(window, slice, nd, band, remapper, chunk_maps);
    });
    true
}
