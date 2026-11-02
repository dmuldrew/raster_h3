//! Transform failures are observable through the providers: counted through a
//! shared counter, or fatal under `with_strict_transforms(true)`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use raster_h3_datafusion::{
    CategoricalRasterTableProvider, ContinuousRasterTableProvider, ExecutionPlan, TableProvider,
};

use crate::helpers::TestGeoTiffBuilder;

/// Rows from a full scan, or the first scan error.
fn run(plan: Box<dyn ExecutionPlan>) -> Result<usize, String> {
    let mut rows = 0;
    for batch in plan.execute().map_err(|e| e.to_string())? {
        rows += batch.map_err(|e| e.to_string())?.num_rows;
    }
    Ok(rows)
}

#[test]
fn providers_count_or_reject_transform_failures() {
    // 10 x 20 one-degree pixels from 95N: the top 5 rows (50 pixels) lie beyond the pole.
    let (_tmp, path) = TestGeoTiffBuilder::new(10, 20)
        .origin(0.0, 95.0)
        .pixel_size(1.0)
        .create_constant_tempfile(3.0f32);
    let source = path.to_str().unwrap();

    let counter = Arc::new(AtomicU64::new(0));
    let continuous = ContinuousRasterTableProvider::try_new(source, vec![3])
        .unwrap()
        .with_transform_failure_counter(Arc::clone(&counter));
    assert!(run(continuous.scan(None, &[], None).unwrap()).unwrap() > 0);
    assert_eq!(counter.swap(0, Ordering::Relaxed), 50);

    let categorical = CategoricalRasterTableProvider::try_new(source, vec![3])
        .unwrap()
        .with_transform_failure_counter(Arc::clone(&counter));
    assert!(run(categorical.scan(None, &[], None).unwrap()).unwrap() > 0);
    assert_eq!(counter.load(Ordering::Relaxed), 50);

    let err = run(continuous
        .with_strict_transforms(true)
        .scan(None, &[], None)
        .unwrap())
    .unwrap_err();
    assert!(err.contains("strict_transforms"), "{err}");
    let err = run(categorical
        .with_strict_transforms(true)
        .scan(None, &[], None)
        .unwrap())
    .unwrap_err();
    assert!(err.contains("strict_transforms"), "{err}");
}
