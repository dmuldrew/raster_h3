#![no_main]

use libfuzzer_sys::fuzz_target;
use raster_h3_core::columnar::ipc::{read_ipc_stream, write_ipc_stream};

fuzz_target!(|data: &[u8]| {
    let Ok((schema, batches)) = read_ipc_stream(&mut std::io::Cursor::new(data)) else {
        return;
    };
    let mut buf1 = Vec::new();
    write_ipc_stream(&mut buf1, &schema, batches.into_iter().map(Ok)).expect("re-encode");
    let (schema2, batches2) =
        read_ipc_stream(&mut std::io::Cursor::new(&buf1)).expect("re-decoding our own stream");
    let mut buf2 = Vec::new();
    write_ipc_stream(&mut buf2, &schema2, batches2.into_iter().map(Ok)).expect("second re-encode");
    assert_eq!(buf1, buf2, "ipc re-encode is not stable");
});
