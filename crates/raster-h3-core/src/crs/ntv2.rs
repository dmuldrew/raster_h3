//! Bounded reader for single-subgrid NTv2 horizontal shift files.
//!
//! This first step reads grid data only; it does not apply a datum transformation.
//! Multiple and nested subgrids are rejected until their selection is implemented.

use std::io::Read;

use crate::error::{RasterH3Error, Result};

const HEADER: usize = 176;
const RECORD: usize = 16;

fn invalid(message: &str) -> RasterH3Error {
    RasterH3Error::CrsError(format!("invalid NTv2 grid: {message}"))
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u32(self, b: &[u8]) -> u32 {
        let b = b.try_into().expect("four-byte field");
        match self {
            Self::Little => u32::from_le_bytes(b),
            Self::Big => u32::from_be_bytes(b),
        }
    }
    fn f32(self, b: &[u8]) -> f32 {
        f32::from_bits(self.u32(b))
    }
    fn f64(self, b: &[u8]) -> f64 {
        let b = b.try_into().expect("eight-byte field");
        match self {
            Self::Little => f64::from_le_bytes(b),
            Self::Big => f64::from_be_bytes(b),
        }
    }
}

fn labels(header: &[u8; HEADER], expected: &[&[u8; 8]; 11]) -> Result<()> {
    for (i, name) in expected.iter().enumerate() {
        if &header[i * RECORD..i * RECORD + 8] != name.as_slice() {
            return Err(invalid("unexpected header record"));
        }
    }
    Ok(())
}

/// A shift pair in arc-seconds, using NTv2's positive-west longitude convention.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ntv2Shift {
    /// Latitude shift, positive north.
    pub latitude_seconds: f32,
    /// Longitude shift, positive west.
    pub longitude_seconds_west: f32,
}

/// Immutable NTv2 grid. Node order is south-to-north, east-to-west.
#[derive(Debug, Clone)]
pub struct Ntv2Grid {
    /// Source datum name as stored in the overview header.
    pub source: String,
    /// Target datum name as stored in the overview header.
    pub target: String,
    /// Bounds and spacing, in arc-seconds; longitude is positive west.
    pub south: f64,
    /// Northern latitude.
    pub north: f64,
    /// Eastern longitude, positive west.
    pub east: f64,
    /// Western longitude, positive west.
    pub west: f64,
    /// Latitude interval.
    pub latitude_step: f64,
    /// Longitude interval.
    pub longitude_step: f64,
    /// Number of rows.
    pub rows: usize,
    /// Number of columns.
    pub columns: usize,
    nodes: Box<[Ntv2Shift]>,
}

fn field_string(bytes: &[u8]) -> Result<String> {
    let s = std::str::from_utf8(bytes).map_err(|_| invalid("non-ASCII name"))?;
    if !s.is_ascii() {
        return Err(invalid("non-ASCII name"));
    }
    Ok(s.trim_matches(|c: char| c == '\0' || c == ' ').to_owned())
}

fn dimension(low: f64, high: f64, step: f64) -> Result<usize> {
    if !low.is_finite() || !high.is_finite() || !step.is_finite() || step <= 0.0 || high <= low {
        return Err(invalid("invalid extent or spacing"));
    }
    let intervals = (high - low) / step;
    let rounded = intervals.round();
    if !intervals.is_finite()
        || (intervals - rounded).abs() > 1e-7
        || rounded > u32::MAX as f64 - 1.0
    {
        return Err(invalid("extent does not match spacing"));
    }
    Ok(rounded as usize + 1)
}

impl Ntv2Grid {
    /// Read a single-subgrid NTv2 file, with an explicit maximum node count.
    /// Rejects malformed metadata, unsupported hierarchies and non-finite shifts.
    pub fn read(mut input: impl Read, max_nodes: usize) -> Result<Self> {
        let mut overview = [0u8; HEADER];
        input.read_exact(&mut overview)?;
        labels(
            &overview,
            &[
                b"NUM_OREC",
                b"NUM_SREC",
                b"NUM_FILE",
                b"GS_TYPE ",
                b"VERSION ",
                b"SYSTEM_F",
                b"SYSTEM_T",
                b"MAJOR_F ",
                b"MINOR_F ",
                b"MAJOR_T ",
                b"MINOR_T ",
            ],
        )?;
        let endian = if u32::from_le_bytes(overview[8..12].try_into().unwrap()) == 11 {
            Endian::Little
        } else if u32::from_be_bytes(overview[8..12].try_into().unwrap()) == 11 {
            Endian::Big
        } else {
            return Err(invalid("invalid overview record count"));
        };
        if endian.u32(&overview[24..28]) != 11 || endian.u32(&overview[40..44]) != 1 {
            return Err(invalid("only one 11-record subgrid is supported"));
        }
        if field_string(&overview[56..64])? != "SECONDS" {
            return Err(invalid("only arc-second grids are supported"));
        }
        let source = field_string(&overview[88..96])?;
        let target = field_string(&overview[104..112])?;
        if source.is_empty() || target.is_empty() {
            return Err(invalid("source and target datums are required"));
        }
        let mut header = [0u8; HEADER];
        input.read_exact(&mut header)?;
        labels(
            &header,
            &[
                b"SUB_NAME",
                b"PARENT  ",
                b"CREATED ",
                b"UPDATED ",
                b"S_LAT   ",
                b"N_LAT   ",
                b"E_LONG  ",
                b"W_LONG  ",
                b"LAT_INC ",
                b"LONG_INC",
                b"GS_COUNT",
            ],
        )?;
        if field_string(&header[8..16])?.is_empty() || field_string(&header[24..32])? != "NONE" {
            return Err(invalid("expected a named root subgrid"));
        }
        let south = endian.f64(&header[72..80]);
        let north = endian.f64(&header[88..96]);
        let east = endian.f64(&header[104..112]);
        let west = endian.f64(&header[120..128]);
        let latitude_step = endian.f64(&header[136..144]);
        let longitude_step = endian.f64(&header[152..160]);
        if south < -324_000.0 || north > 324_000.0 || east < -648_000.0 || west > 648_000.0 {
            return Err(invalid("extent outside geographic domain"));
        }
        let rows = dimension(south, north, latitude_step)?;
        let columns = dimension(east, west, longitude_step)?;
        let count = rows
            .checked_mul(columns)
            .ok_or_else(|| invalid("node count overflow"))?;
        if count != endian.u32(&header[168..172]) as usize || count > max_nodes {
            return Err(invalid("node count mismatch or limit exceeded"));
        }
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(count)
            .map_err(|_| invalid("node allocation failed"))?;
        for _ in 0..count {
            let mut raw = [0u8; RECORD];
            input.read_exact(&mut raw)?;
            let latitude_seconds = endian.f32(&raw[0..4]);
            let longitude_seconds_west = endian.f32(&raw[4..8]);
            if !latitude_seconds.is_finite() || !longitude_seconds_west.is_finite() {
                return Err(invalid("non-finite node shift"));
            }
            nodes.push(Ntv2Shift {
                latitude_seconds,
                longitude_seconds_west,
            });
        }
        Ok(Self {
            source,
            target,
            south,
            north,
            east,
            west,
            latitude_step,
            longitude_step,
            rows,
            columns,
            nodes: nodes.into_boxed_slice(),
        })
    }

    /// Interpolate a horizontal shift at longitude and latitude in degrees.
    ///
    /// The input is on the grid's source datum. Longitudes use the conventional
    /// positive-east sign; NTv2 grid longitudes are positive west. All four grid
    /// boundaries are inclusive. Points outside the grid are rejected.
    pub fn interpolate(&self, lon: f64, lat: f64) -> Result<(f64, f64)> {
        if !lon.is_finite() || !lat.is_finite() {
            return Err(invalid("non-finite coordinate"));
        }
        if self.rows < 2
            || self.columns < 2
            || !self.latitude_step.is_finite()
            || self.latitude_step <= 0.0
            || !self.longitude_step.is_finite()
            || self.longitude_step <= 0.0
        {
            return Err(invalid("invalid grid dimensions or spacing"));
        }
        let west = -lon * 3600.0;
        let north = lat * 3600.0;
        if west < self.east || west > self.west || north < self.south || north > self.north {
            return Err(invalid("coordinate outside grid coverage"));
        }
        let row_position = (north - self.south) / self.latitude_step;
        let col_position = (west - self.east) / self.longitude_step;
        let row = (row_position.floor() as usize).min(self.rows - 2);
        let col = (col_position.floor() as usize).min(self.columns - 2);
        let dy = (row_position - row as f64).clamp(0.0, 1.0);
        let dx = (col_position - col as f64).clamp(0.0, 1.0);
        let a = self.node(row, col).ok_or_else(|| invalid("invalid grid dimensions"))?;
        let b = self.node(row, col + 1).ok_or_else(|| invalid("invalid grid dimensions"))?;
        let c = self.node(row + 1, col).ok_or_else(|| invalid("invalid grid dimensions"))?;
        let d = self.node(row + 1, col + 1).ok_or_else(|| invalid("invalid grid dimensions"))?;
        let interpolate = |a: f32, b: f32, c: f32, d: f32| {
            let low = f64::from(a) * (1.0 - dx) + f64::from(b) * dx;
            let high = f64::from(c) * (1.0 - dx) + f64::from(d) * dx;
            low * (1.0 - dy) + high * dy
        };
        Ok((
            interpolate(
                a.longitude_seconds_west, b.longitude_seconds_west,
                c.longitude_seconds_west, d.longitude_seconds_west,
            ),
            interpolate(
                a.latitude_seconds, b.latitude_seconds,
                c.latitude_seconds, d.latitude_seconds,
            ),
        ))
    }

    /// Apply the source-to-target grid shift to longitude and latitude in degrees.
    pub fn forward(&self, lon: f64, lat: f64) -> Result<(f64, f64)> {
        let (west_shift, lat_shift) = self.interpolate(lon, lat)?;
        Ok((lon - west_shift / 3600.0, lat + lat_shift / 3600.0))
    }

    /// Invert a shift with a bounded fixed-point iteration.
    ///
    /// The target and each trial coordinate must lie within the source grid's
    /// coverage. This conservative restriction can reject valid inverse points
    /// near the grid boundary. Convergence uses a 1e-11 degree residual.
    pub fn inverse(&self, lon: f64, lat: f64) -> Result<(f64, f64)> {
        const TOLERANCE: f64 = 1e-11;
        const MAX_ITERATIONS: usize = 20;
        let (mut x, mut y) = (lon, lat);
        for _ in 0..MAX_ITERATIONS {
            let (west_shift, lat_shift) = self.interpolate(x, y)?;
            let next_x = lon + west_shift / 3600.0;
            let next_y = lat - lat_shift / 3600.0;
            if (next_x - x).abs().max((next_y - y).abs()) <= TOLERANCE {
                let (out_x, out_y) = self.forward(next_x, next_y)?;
                if (out_x - lon).abs().max((out_y - lat).abs()) <= TOLERANCE {
                    return Ok((next_x, next_y));
                }
            }
            (x, y) = (next_x, next_y);
        }
        Err(invalid("inverse grid shift did not converge"))
    }

    /// Return the shift at a row and column, in the file's native order.
    pub fn node(&self, row: usize, column: usize) -> Option<Ntv2Shift> {
        if row >= self.rows || column >= self.columns {
            return None;
        }
        row.checked_mul(self.columns)
            .and_then(|offset| offset.checked_add(column))
            .and_then(|index| self.nodes.get(index))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(big: bool) -> Vec<u8> {
        let mut data = vec![0u8; HEADER * 2 + RECORD * 4];
        let overview = [
            b"NUM_OREC",
            b"NUM_SREC",
            b"NUM_FILE",
            b"GS_TYPE ",
            b"VERSION ",
            b"SYSTEM_F",
            b"SYSTEM_T",
            b"MAJOR_F ",
            b"MINOR_F ",
            b"MAJOR_T ",
            b"MINOR_T ",
        ];
        let subgrid = [
            b"SUB_NAME",
            b"PARENT  ",
            b"CREATED ",
            b"UPDATED ",
            b"S_LAT   ",
            b"N_LAT   ",
            b"E_LONG  ",
            b"W_LONG  ",
            b"LAT_INC ",
            b"LONG_INC",
            b"GS_COUNT",
        ];
        for (base, names) in [(0, overview), (HEADER, subgrid)] {
            for (i, name) in names.iter().enumerate() {
                data[base + i * RECORD..base + i * RECORD + 8].copy_from_slice(*name);
            }
        }
        let int = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let double = |v: f64| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let float = |v: f32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        for (offset, value) in [(8, 11), (24, 11), (40, 1), (HEADER + 168, 4)] {
            data[offset..offset + 4].copy_from_slice(&int(value));
        }
        data[56..64].copy_from_slice(b"SECONDS ");
        data[88..96].copy_from_slice(b"SRC     ");
        data[104..112].copy_from_slice(b"DST     ");
        data[HEADER + 8..HEADER + 16].copy_from_slice(b"ROOT    ");
        data[HEADER + 24..HEADER + 32].copy_from_slice(b"NONE    ");
        for (offset, value) in [
            (72, 0.0),
            (88, 3600.0),
            (104, 0.0),
            (120, 3600.0),
            (136, 3600.0),
            (152, 3600.0),
        ] {
            data[HEADER + offset..HEADER + offset + 8].copy_from_slice(&double(value));
        }
        for i in 0..4 {
            let offset = HEADER * 2 + RECORD * i;
            data[offset..offset + 4].copy_from_slice(&float(i as f32 + 0.5));
            data[offset + 4..offset + 8].copy_from_slice(&float(-(i as f32)));
        }
        data
    }

    #[test]
    fn reads_both_byte_orders_and_preserves_native_node_order() {
        for big in [false, true] {
            let grid = Ntv2Grid::read(&fixture(big)[..], 4).unwrap();
            assert_eq!((grid.source.as_str(), grid.target.as_str()), ("SRC", "DST"));
            assert_eq!((grid.rows, grid.columns), (2, 2));
            assert_eq!(grid.node(0, 0).unwrap().latitude_seconds, 0.5);
            assert_eq!(grid.node(1, 1).unwrap().longitude_seconds_west, -3.0);
            assert!(grid.node(0, 2).is_none());
            assert!(grid.node(2, 0).is_none());
        }
    }

    #[test]
    fn interpolates_nodes_edges_and_interior_with_ntv2_longitude_sign() {
        for big in [false, true] {
            let grid = Ntv2Grid::read(&fixture(big)[..], 4).unwrap();
            // Native layout: southwest is row 0, column 1; southeast is column 0.
            assert_eq!(grid.interpolate(0.0, 0.0).unwrap(), (0.0, 0.5));
            assert_eq!(grid.interpolate(-1.0, 0.0).unwrap(), (-1.0, 1.5));
            assert_eq!(grid.interpolate(0.0, 1.0).unwrap(), (-2.0, 2.5));
            assert_eq!(grid.interpolate(-1.0, 1.0).unwrap(), (-3.0, 3.5));
            assert_eq!(grid.interpolate(-0.5, 0.5).unwrap(), (-1.5, 2.0));
            let (lon, lat) = grid.forward(-0.5, 0.5).unwrap();
            assert!((lon - (-0.5 + 1.5 / 3600.0)).abs() < 1e-12);
            assert!((lat - (0.5 + 2.0 / 3600.0)).abs() < 1e-12);
            let (back_lon, back_lat) = grid.inverse(lon, lat).unwrap();
            assert!((back_lon + 0.5).abs() < 1e-11);
            assert!((back_lat - 0.5).abs() < 1e-11);
        }
    }

    #[test]
    fn inverse_reports_nonconvergent_grid() {
        let mut data = fixture(false);
        for i in 0..4 {
            let offset = HEADER * 2 + RECORD * i;
            let shift = if i < 2 { -1800.0_f32 } else { 1800.0_f32 };
            data[offset..offset + 4].copy_from_slice(&shift.to_le_bytes());
            data[offset + 4..offset + 8].copy_from_slice(&0.0_f32.to_le_bytes());
        }
        let grid = Ntv2Grid::read(&data[..], 4).unwrap();
        let error = grid.inverse(-0.5, 0.6).unwrap_err().to_string();
        assert!(error.contains("did not converge"), "{error}");
    }

    #[test]
    fn rejects_outside_and_nonfinite_coordinates() {
        let grid = Ntv2Grid::read(&fixture(false)[..], 4).unwrap();
        for (lon, lat) in [(0.0001, 0.5), (-1.0001, 0.5), (-0.5, -0.0001),
                           (-0.5, 1.0001), (f64::NAN, 0.5),
                           (-0.5, f64::INFINITY)] {
            assert!(grid.forward(lon, lat).is_err());
            assert!(grid.inverse(lon, lat).is_err());
        }
    }

    #[test]
    fn rejects_malformed_truncated_and_oversized_files() {
        let data = fixture(false);
        for end in [0, HEADER - 1, HEADER, HEADER * 2 - 1, data.len() - 1] {
            assert!(Ntv2Grid::read(&data[..end], 4).is_err());
        }
        assert!(Ntv2Grid::read(&data[..], 3).is_err());
        for offset in [0, HEADER, HEADER + 64] {
            let mut bad = data.clone();
            bad[offset] ^= 0x40;
            assert!(Ntv2Grid::read(&bad[..], 4).is_err());
        }
        let mut bad = data.clone();
        bad[40..44].copy_from_slice(&2u32.to_le_bytes());
        assert!(Ntv2Grid::read(&bad[..], 4).is_err());
        let mut bad = data.clone();
        bad[HEADER + 136..HEADER + 144].copy_from_slice(&0.0_f64.to_le_bytes());
        assert!(Ntv2Grid::read(&bad[..], 4).is_err());
        let mut bad = data.clone();
        bad[HEADER + 168..HEADER + 172].copy_from_slice(&3u32.to_le_bytes());
        assert!(Ntv2Grid::read(&bad[..], 4).is_err());
        let mut bad = data.clone();
        bad[HEADER * 2..HEADER * 2 + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(Ntv2Grid::read(&bad[..], 4).is_err());
    }
}
