pub mod geotiff;
pub mod geotransform;
pub mod http_range;
pub mod mosaic;
pub mod prefetch;

pub use geotiff::{ChunkDecoder, ChunkLayout, GeoTiffMetadata, GeoTiffStreamReader, RasterSource};
pub use geotransform::GeoTransform;
pub use http_range::{is_remote_url, normalize_url, HttpRangeReader, RemoteHttpSource};
pub use mosaic::{glob_match, resolve_raster_sources, MosaicChunkRef, MosaicReader, OverlapRule, TileDescriptor};
pub use prefetch::{MosaicPrefetchItem, PrefetchedChunkReader, PrefetchedMosaicReader};

/// Represents a 2D chunk / tile window of a raster
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterChunk {
    pub col_offset: u32,
    pub row_offset: u32,
    pub width: u32,
    pub height: u32,
}

impl RasterChunk {
    /// Generate a list of non-overlapping chunks covering width x height
    pub fn partition_grid(width: u32, height: u32, chunk_size: u32) -> Vec<Self> {
        let mut chunks = Vec::new();
        let mut row = 0;
        while row < height {
            let chunk_h = (height - row).min(chunk_size);
            let mut col = 0;
            while col < width {
                let chunk_w = (width - col).min(chunk_size);
                chunks.push(RasterChunk {
                    col_offset: col,
                    row_offset: row,
                    width: chunk_w,
                    height: chunk_h,
                });
                col += chunk_size;
            }
            row += chunk_size;
        }
        chunks
    }
}
