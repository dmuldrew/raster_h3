# Rust API migration

The streaming refactor preserves SQL parameter names. Rust callers should use
configuration constructors or `..Default::default()` in struct literals to pick
up newly added fields.

## Separate compaction options

- `MultiResolutionConfig::compact_h3_children` controls hierarchical H3 child
  compaction. Both it and the legacy `compact` field default to `false`; either
  field enables compaction. Adjacent requested resolutions are rejected when
  compaction is enabled because they can produce duplicate parent records.
- `ParquetExportConfig::omit_redundant_columns` controls omission of `h3_hex`,
  `lat`, and `lng`. Both it and the legacy `compact` field default to `true`;
  setting either to `false` retains those columns.

For new callers, set the descriptive option and leave the legacy field at its
default. The two options control different operations.

## Streaming and remote reads

Use streamer methods such as `fetch_next_batch`, `is_finished`, and
`current_lat_horizon` instead of modifying lifecycle, compaction, or output
buffer state. These implementation fields are private. Failures remain latched
and are returned as errors on subsequent reads.

`RemoteHttpSource` owns its private transport and cache. Use `open` to construct
it, `read_range` for reads that may stop at EOF, and `read_exact_range` for fixed
payload sizes. TIFF chunk payload reads use the exact contract and reject ranges
extending beyond EOF before allocating their buffers.

## DuckDB helpers

`ChunkWriter::get_data_slice_mut` and `fill_column` require a mutable writer.
The slice method remains unsafe: callers must provide a live buffer of the
correct physical type and size with no aliases. `BindHelper::add_custom_result_column`
is unsafe because its raw logical-type handle must be live during the call.
