#![no_main]

use std::ffi::c_void;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use raster_h3_core::columnar::{
    ArrowArray, ArrowDataType, ColumnArray, ColumnField, ColumnSchema, RecordBatch,
};

/// Point child `i`'s buffer `slot` at `p`, returning the previous pointer.
///
/// # Safety
/// `arr` must own at least `i + 1` children and child `i` must have at least
/// `slot + 1` buffer-pointer slots.
unsafe fn set_child_buffer(
    arr: &mut ArrowArray,
    i: usize,
    slot: usize,
    p: *const c_void,
) -> *const c_void {
    // SAFETY: the caller guarantees `arr` owns at least `i + 1` children.
    let child = unsafe { &mut **arr.children.add(i) };
    // SAFETY: the caller guarantees child `i` has at least `slot + 1`
    // buffer-pointer slots.
    unsafe { std::mem::replace(&mut *child.buffers.add(slot), p) }
}

fuzz_target!(|data: &[u8]| {
    // Trailing two bytes drive the parent offset/length; the rest supplies offsets.
    if data.len() < 12 {
        return;
    }
    let usable = data.len() - 2;
    let n = (usable / 4 - 1).min(64);
    if n < 1 {
        return;
    }

    let schema = Arc::new(ColumnSchema::new(vec![
        ColumnField::new("s", ArrowDataType::Utf8, false),
        ColumnField::new("b", ArrowDataType::Binary, false),
    ]));
    // One byte per value, so each data buffer holds exactly `n` bytes and
    // forged offsets in [0, n] can never index past it.
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            ColumnArray::Utf8 {
                values: vec!["x".to_string(); n],
                validity: None,
            },
            ColumnArray::Binary {
                values: vec![vec![0u8]; n],
                validity: None,
            },
        ],
    )
    .unwrap();
    let mut arr = ArrowArray::from_record_batch(batch);

    // Map the input into forged offsets in [-2, n]; pad the window so the
    // `parent offset + length + 1` offsets an import may read stay in bounds.
    let window = 2 * n + 2;
    let mut forged: Vec<i32> = Vec::with_capacity(window);
    for i in 0..window {
        let raw = if i <= n {
            i32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap())
        } else {
            i32::from_le_bytes(
                data[(i % (n + 1)) * 4..(i % (n + 1)) * 4 + 4]
                    .try_into()
                    .unwrap(),
            )
        };
        forged.push(raw.rem_euclid(n as i32 + 3) - 2);
    }
    // SAFETY: `arr` has 2 children (Utf8 at 0, Binary at 1), each with 3
    // buffer slots; slot 1 is the offsets buffer. `forged` outlives the call.
    let orig_s = unsafe { set_child_buffer(&mut arr, 0, 1, forged.as_ptr().cast()) };
    // SAFETY: same as above, for the Binary child.
    let orig_b = unsafe { set_child_buffer(&mut arr, 1, 1, forged.as_ptr().cast()) };

    let (off_b, len_b) = (data[usable], data[usable + 1]);
    arr.offset = i64::from(off_b) % (n as i64 + 1);
    arr.length = i64::from(len_b) % (n as i64 + 1);

    // SAFETY: `arr` matches `schema`; only the forged offsets and parent
    // offset/length deviate, which the import must validate, not panic on.
    let result = unsafe { arr.to_record_batch(Arc::clone(&schema)) };
    if let Ok(batch) = result {
        assert_eq!(
            batch.num_rows, arr.length as usize,
            "imported batch row count disagrees with array length"
        );
    }

    // SAFETY: restoring the original offsets pointers so `arr`'s release
    // frees only buffers it owns; `forged` is dropped by us.
    unsafe {
        set_child_buffer(&mut arr, 0, 1, orig_s);
        set_child_buffer(&mut arr, 1, 1, orig_b);
    }
});
