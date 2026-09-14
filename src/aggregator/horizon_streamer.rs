use h3o::CellIndex;
use tiff::decoder::DecodingResult;

use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::RasterChunk;

/// Priority queue entry for H3 cell eviction ordered by southernmost latitude
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HexEvictionEntry {
    pub south_lat: f64,
    pub cell_u64: u64,
}

impl Eq for HexEvictionEntry {}

impl Ord for HexEvictionEntry {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap: highest south_lat (northernmost southern boundary) is popped first
        self.south_lat.total_cmp(&other.south_lat)
    }
}

impl PartialOrd for HexEvictionEntry {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Compute the conservative southernmost latitude for an H3 cell (center latitude minus max circumradius in degrees).
/// This guarantees that any cell with south_lat > lat_horizon lies strictly north of lat_horizon,
/// ensuring zero premature evictions and zero double-counting across batch boundaries.
#[inline(always)]
pub fn compute_cell_south_lat(cell_u64: u64) -> f64 {
    if let Ok(cell) = CellIndex::try_from(cell_u64) {
        let center_lat = h3o::LatLng::from(cell).lat();
        let r = crate::pmtiles::tiler::max_hex_radius_deg(cell.resolution().into());
        center_lat - r
    } else {
        f64::NEG_INFINITY
    }
}

/// Fast check if an entire chunk slice is NoData / NaN
#[inline(always)]
pub fn is_chunk_all_nodata<T, F>(slice: &[T], nodata: Option<f64>, to_f64: F) -> bool
where
    T: Copy,
    F: Fn(T) -> f64,
{
    if slice.is_empty() {
        return true;
    }
    match nodata {
        Some(nd) => {
            let mid = slice.len() / 2;
            let last = slice.len() - 1;
            let s0 = to_f64(slice[0]);
            let s_mid = to_f64(slice[mid]);
            let s_last = to_f64(slice[last]);

            let is_nd = |v: f64| !v.is_finite() || (v - nd).abs() < 1e-6;
            if !is_nd(s0) || !is_nd(s_mid) || !is_nd(s_last) {
                return false;
            }
            slice.iter().all(|&x| is_nd(to_f64(x)))
        }
        None => slice.iter().all(|&x| !to_f64(x).is_finite()),
    }
}

/// Trait for safely casting an optional f64 NoData value to a typed native pixel value within valid range bounds
pub trait NodataCast: Copy + PartialEq + Send + Sync + 'static {
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self>;
}

impl NodataCast for u8 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None })
    }
}

impl NodataCast for u16 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None })
    }
}

impl NodataCast for u32 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None })
    }
}

impl NodataCast for u64 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None })
    }
}

impl NodataCast for i8 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None })
    }
}

impl NodataCast for i16 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None })
    }
}

impl NodataCast for i32 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None })
    }
}

impl NodataCast for i64 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.map(|v| v as i64)
    }
}

impl NodataCast for f32 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.map(|v| v as f32)
    }
}

impl NodataCast for f64 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata
    }
}

/// Fast check if an entire TIFF DecodingResult chunk is NoData / NaN
pub fn is_decoding_result_all_nodata(decoding_result: &DecodingResult, nodata: Option<f64>) -> bool {
    match decoding_result {
        DecodingResult::U8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::U16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::U32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::U64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::I8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::I16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::I32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::I64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::F32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::F64(slice) => is_chunk_all_nodata(slice, nodata, |x| x),
    }
}

/// Check if a 2D chunk intersects the given [min_lon, min_lat, max_lon, max_lat] bounding box
pub fn chunk_intersects_bbox(
    chunk: &RasterChunk,
    gt: &GeoTransform,
    transformer: &CrsTransformer,
    bbox: &[f64; 4],
) -> bool {
    let [b_min_lon, b_min_lat, b_max_lon, b_max_lat] = *bbox;

    let corners = [
        (chunk.col_offset as usize, chunk.row_offset as usize),
        ((chunk.col_offset + chunk.width) as usize, chunk.row_offset as usize),
        (chunk.col_offset as usize, (chunk.row_offset + chunk.height) as usize),
        ((chunk.col_offset + chunk.width) as usize, (chunk.row_offset + chunk.height) as usize),
    ];

    let mut c_min_lon = f64::INFINITY;
    let mut c_max_lon = f64::NEG_INFINITY;
    let mut c_min_lat = f64::INFINITY;
    let mut c_max_lat = f64::NEG_INFINITY;

    for (c, r) in corners {
        let (x, y) = gt.pixel_to_coord(c as f64, r as f64);
        if let Ok((lon, lat)) = transformer.transform_point(x, y) {
            if lon < c_min_lon { c_min_lon = lon; }
            if lon > c_max_lon { c_max_lon = lon; }
            if lat < c_min_lat { c_min_lat = lat; }
            if lat > c_max_lat { c_max_lat = lat; }
        }
    }

    if !c_min_lon.is_finite() {
        return true;
    }

    c_min_lon <= b_max_lon && c_max_lon >= b_min_lon && c_min_lat <= b_max_lat && c_max_lat >= b_min_lat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nodata_cast_ranges() {
        // u8: 0..=255
        assert_eq!(u8::from_nodata_f64(Some(0.0)), Some(0));
        assert_eq!(u8::from_nodata_f64(Some(255.0)), Some(255));
        assert_eq!(u8::from_nodata_f64(Some(256.0)), None);
        assert_eq!(u8::from_nodata_f64(Some(-1.0)), None);
        assert_eq!(u8::from_nodata_f64(None), None);

        // i8: -128..=127
        assert_eq!(i8::from_nodata_f64(Some(-128.0)), Some(-128));
        assert_eq!(i8::from_nodata_f64(Some(127.0)), Some(127));
        assert_eq!(i8::from_nodata_f64(Some(128.0)), None);

        // u16: 0..=65535
        assert_eq!(u16::from_nodata_f64(Some(65535.0)), Some(65535));
        assert_eq!(u16::from_nodata_f64(Some(65536.0)), None);

        // f32 and f64
        assert_eq!(f32::from_nodata_f64(Some(-9999.0)), Some(-9999.0f32));
        assert_eq!(f64::from_nodata_f64(Some(-9999.0)), Some(-9999.0f64));
    }

    #[test]
    fn test_is_decoding_result_all_nodata() {
        let dr_u8 = DecodingResult::U8(vec![255, 255, 255]);
        assert!(is_decoding_result_all_nodata(&dr_u8, Some(255.0)));
        assert!(!is_decoding_result_all_nodata(&dr_u8, Some(0.0)));

        let dr_f32 = DecodingResult::F32(vec![f32::NAN, f32::NAN]);
        assert!(is_decoding_result_all_nodata(&dr_f32, None));

        let dr_mixed = DecodingResult::F32(vec![f32::NAN, 1.0]);
        assert!(!is_decoding_result_all_nodata(&dr_mixed, None));
    }
}
