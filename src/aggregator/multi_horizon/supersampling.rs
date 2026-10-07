//! One projection per sample and one H3 lookup per sample/resolution.
//! Scratch storage is reused and bounded by the configured sampling pattern,
//! not the row or raster size. Failed grouping replays stored assignments.
use super::coordinates::{RowCoordinates, RowGeometryContext};
use super::walker::ScanlineEngine;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::raster::{geotransform::GeoTransform, RasterChunk};
use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use std::collections::HashMap;

#[allow(clippy::too_many_arguments)]
pub(super) fn walk_samples<T, Acc, E, F>(
    slice: &[T],
    chunk: &RasterChunk,
    resolutions: &[Resolution],
    crs: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    is_row_all_nodata: F,
    engine: &E,
    maps: &mut [HashMap<u64, Acc, FxBuildHasher>],
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
    F: Fn(&[T]) -> bool,
{
    let geometry = RowGeometryContext::new(chunk, slice.len(), chunk_stride, crs, gt);
    let mut positions = Vec::with_capacity(sampling.points.len());
    let mut assignments = Vec::with_capacity(sampling.points.len());
    let mut run_cells = vec![0; resolutions.len()];
    let mut run_accs: Vec<_> = resolutions.iter().map(|_| engine.new_acc()).collect();
    let combine = engine.combine_sample_weights();
    for row in 0..geometry.actual_rows {
        let start = row * geometry.stride;
        let width = (slice.len() - start).min(chunk.width as usize);
        let values = &slice[start..start + width];
        if is_row_all_nodata(values) {
            continue;
        }
        let coords = RowCoordinates {
            row_idx: chunk.row_offset as usize + row,
            row_c_start: 0,
            row_c_end: width,
        };
        for (col, &value) in values.iter().enumerate() {
            let Some(sample) = engine.get_sample(value) else {
                continue;
            };
            positions.clear();
            coords.for_each_subpixel(
                col,
                gt,
                crs,
                chunk.col_offset as usize,
                sampling,
                bbox,
                |lon, lat, _, _, weight| {
                    if (-90.0..=90.0).contains(&lat) {
                        if let Ok(ll) = LatLng::new(lat, lon) {
                            positions.push((ll, weight));
                        }
                    }
                },
            );
            if positions.is_empty() {
                continue;
            }
            for (i, &res) in resolutions.iter().enumerate() {
                assignments.clear();
                for &(ll, weight) in &positions {
                    assignments.push((u64::from(super::profile::index(ll, res)), weight));
                }
                let first = assignments[0].0;
                // All retained samples must agree. Clipped/invalid samples contribute
                // no weight; custom patterns need not sum to one.
                if combine && assignments.iter().all(|&(cell, _)| cell == first) {
                    let weight = assignments.iter().map(|a| a.1).sum::<f64>();
                    if weight.is_finite() {
                        accumulate(
                            first,
                            weight,
                            sample,
                            engine,
                            &mut run_cells[i],
                            &mut run_accs[i],
                            &mut maps[i],
                        );
                        continue;
                    }
                }
                // Preserve sample order for boundary pixels and quantile sketches.
                for &(cell, weight) in &assignments {
                    accumulate(
                        cell,
                        weight,
                        sample,
                        engine,
                        &mut run_cells[i],
                        &mut run_accs[i],
                        &mut maps[i],
                    );
                }
            }
        }
    }
    for (i, &cell) in run_cells.iter().enumerate() {
        if cell != 0 && engine.has_samples(&run_accs[i]) {
            maps[i]
                .entry(cell)
                .and_modify(|acc| engine.merge_acc(acc, &run_accs[i]))
                .or_insert_with(|| run_accs[i].clone());
        }
    }
}

#[inline]
fn accumulate<T, Acc, E>(
    cell: u64,
    weight: f64,
    sample: E::Sample,
    engine: &E,
    run_cell: &mut u64,
    run_acc: &mut Acc,
    map: &mut HashMap<u64, Acc, FxBuildHasher>,
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
{
    if cell != *run_cell {
        if *run_cell != 0 && engine.has_samples(run_acc) {
            map.entry(*run_cell)
                .and_modify(|acc| engine.merge_acc(acc, run_acc))
                .or_insert_with(|| run_acc.clone());
        }
        engine.clear_acc(run_acc);
        *run_cell = cell;
    }
    engine.update_sample(run_acc, sample, weight);
}
