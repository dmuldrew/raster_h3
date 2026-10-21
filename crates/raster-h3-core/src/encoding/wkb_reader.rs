//! Well-Known Binary (WKB) reader for polygonal geometries (`POLYGON` / `MULTIPOLYGON`).
//!
//! Accepts OGC/ISO WKB (2D, Z, M and ZM type codes) and PostGIS EWKB (Z/M/SRID flag bits)
//! in either byte order. Z and M ordinates are skipped; only X (longitude) and Y (latitude)
//! are kept.

use geo_types::{Coord, LineString, MultiPolygon, Polygon};

const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOLYGON: u32 = 6;

const EWKB_Z_FLAG: u32 = 0x8000_0000;
const EWKB_M_FLAG: u32 = 0x4000_0000;
const EWKB_SRID_FLAG: u32 = 0x2000_0000;

/// Upper bound on nested ring/point counts accepted before allocation, so a corrupt count
/// cannot request an absurd preallocation (the remaining byte length is checked as well).
const MAX_PREALLOC: usize = 1 << 16;

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let end = self.pos.checked_add(N).ok_or("truncated WKB")?;
        let slice = self.bytes.get(self.pos..end).ok_or("truncated WKB")?;
        self.pos = end;
        Ok(slice.try_into().expect("slice length matches N"))
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take::<1>()?[0])
    }

    fn u32(&mut self, little: bool) -> Result<u32, &'static str> {
        let b = self.take::<4>()?;
        Ok(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self, little: bool) -> Result<f64, &'static str> {
        let b = self.take::<8>()?;
        Ok(if little {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }
}

/// Geometry header: byte order, base type code and number of ordinates per point.
struct Header {
    little: bool,
    base_type: u32,
    dims: usize,
}

fn read_header(cur: &mut Cursor<'_>) -> Result<Header, &'static str> {
    let little = match cur.u8()? {
        0 => false,
        1 => true,
        _ => return Err("invalid WKB byte order marker"),
    };
    let raw = cur.u32(little)?;
    let mut dims = 2;
    if raw & EWKB_Z_FLAG != 0 {
        dims += 1;
    }
    if raw & EWKB_M_FLAG != 0 {
        dims += 1;
    }
    if raw & EWKB_SRID_FLAG != 0 {
        cur.u32(little)?;
    }
    let iso = raw & 0x0FFF_FFFF;
    // ISO codes: 1000 = Z, 2000 = M, 3000 = ZM.
    match iso / 1000 {
        0 => {}
        1 | 2 => dims += 1,
        3 => dims += 2,
        _ => return Err("unsupported WKB geometry type"),
    }
    Ok(Header {
        little,
        base_type: iso % 1000,
        dims,
    })
}

fn read_ring(cur: &mut Cursor<'_>, h: &Header) -> Result<LineString<f64>, &'static str> {
    let n = cur.u32(h.little)? as usize;
    if n.saturating_mul(8 * h.dims) > cur.remaining() {
        return Err("truncated WKB");
    }
    let mut coords = Vec::with_capacity(n.min(MAX_PREALLOC));
    for _ in 0..n {
        let x = cur.f64(h.little)?;
        let y = cur.f64(h.little)?;
        for _ in 2..h.dims {
            cur.f64(h.little)?;
        }
        coords.push(Coord { x, y });
    }
    Ok(LineString::new(coords))
}

/// Read a polygon body (after its header). `None` for `POLYGON EMPTY`.
fn read_polygon_body(
    cur: &mut Cursor<'_>,
    h: &Header,
) -> Result<Option<Polygon<f64>>, &'static str> {
    let n_rings = cur.u32(h.little)? as usize;
    if n_rings.saturating_mul(4) > cur.remaining() {
        return Err("truncated WKB");
    }
    let mut rings = Vec::with_capacity(n_rings.min(MAX_PREALLOC));
    for _ in 0..n_rings {
        rings.push(read_ring(cur, h)?);
    }
    let mut rings = rings.into_iter();
    let Some(exterior) = rings.next() else {
        return Ok(None);
    };
    if exterior.0.is_empty() {
        if rings.any(|r| !r.0.is_empty()) {
            return Err("polygon has interior rings but an empty exterior ring");
        }
        return Ok(None);
    }
    // `Polygon::new` closes any unclosed ring.
    Ok(Some(Polygon::new(
        exterior,
        rings.filter(|r| !r.0.is_empty()).collect(),
    )))
}

/// Parse a WKB/EWKB `POLYGON` or `MULTIPOLYGON` into a [`MultiPolygon`] (a single polygon
/// becomes a one-element collection, empty geometries an empty one).
///
/// Errors on malformed input, trailing bytes, or any other geometry type.
pub fn wkb_to_multipolygon(bytes: &[u8]) -> Result<MultiPolygon<f64>, &'static str> {
    let mut cur = Cursor { bytes, pos: 0 };
    let h = read_header(&mut cur)?;
    let polygons = match h.base_type {
        WKB_POLYGON => read_polygon_body(&mut cur, &h)?.into_iter().collect(),
        WKB_MULTIPOLYGON => {
            let n = cur.u32(h.little)? as usize;
            if n.saturating_mul(9) > cur.remaining() {
                return Err("truncated WKB");
            }
            let mut polygons = Vec::with_capacity(n.min(MAX_PREALLOC));
            for _ in 0..n {
                let inner = read_header(&mut cur)?;
                if inner.base_type != WKB_POLYGON {
                    return Err("MULTIPOLYGON member is not a POLYGON");
                }
                polygons.extend(read_polygon_body(&mut cur, &inner)?);
            }
            polygons
        }
        _ => return Err("expected a WKB POLYGON or MULTIPOLYGON"),
    };
    if cur.remaining() != 0 {
        return Err("trailing bytes after WKB geometry");
    }
    Ok(MultiPolygon::new(polygons))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_bytes(out: &mut Vec<u8>, pts: &[(f64, f64)], little: bool) {
        let put_u32 = |out: &mut Vec<u8>, v: u32| {
            out.extend(if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            })
        };
        put_u32(out, pts.len() as u32);
        for &(x, y) in pts {
            for v in [x, y] {
                out.extend(if little {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                });
            }
        }
    }

    fn square() -> Vec<(f64, f64)> {
        vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]
    }

    #[test]
    fn parses_polygon_both_byte_orders() {
        for little in [true, false] {
            let mut wkb = vec![u8::from(little)];
            wkb.extend(if little {
                3u32.to_le_bytes()
            } else {
                3u32.to_be_bytes()
            });
            wkb.extend(if little {
                1u32.to_le_bytes()
            } else {
                1u32.to_be_bytes()
            });
            ring_bytes(&mut wkb, &square(), little);
            let mp = wkb_to_multipolygon(&wkb).unwrap();
            assert_eq!(mp.0.len(), 1);
            assert_eq!(mp.0[0].exterior().0.len(), 5);
            assert_eq!(mp.0[0].exterior().0[2], Coord { x: 1.0, y: 1.0 });
        }
    }

    #[test]
    fn parses_multipolygon_and_skips_z() {
        let mut wkb = vec![1u8];
        wkb.extend(6u32.to_le_bytes());
        wkb.extend(2u32.to_le_bytes());
        for _ in 0..2 {
            wkb.push(1);
            wkb.extend(1003u32.to_le_bytes()); // ISO POLYGON Z
            wkb.extend(1u32.to_le_bytes());
            wkb.extend(4u32.to_le_bytes());
            for (x, y) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)] {
                for v in [x, y, 99.0f64] {
                    wkb.extend(v.to_le_bytes());
                }
            }
        }
        let mp = wkb_to_multipolygon(&wkb).unwrap();
        assert_eq!(mp.0.len(), 2);
        assert_eq!(mp.0[1].exterior().0[1], Coord { x: 1.0, y: 0.0 });
    }

    #[test]
    fn parses_ewkb_with_srid() {
        let mut wkb = vec![1u8];
        wkb.extend((3u32 | EWKB_SRID_FLAG).to_le_bytes());
        wkb.extend(4326u32.to_le_bytes());
        wkb.extend(1u32.to_le_bytes());
        ring_bytes(&mut wkb, &square(), true);
        assert_eq!(wkb_to_multipolygon(&wkb).unwrap().0.len(), 1);
    }

    #[test]
    fn empty_and_malformed() {
        let mut empty = vec![1u8];
        empty.extend(3u32.to_le_bytes());
        empty.extend(0u32.to_le_bytes());
        assert!(wkb_to_multipolygon(&empty).unwrap().0.is_empty());

        let mut point = vec![1u8];
        point.extend(1u32.to_le_bytes());
        point.extend(0f64.to_le_bytes());
        point.extend(0f64.to_le_bytes());
        assert!(wkb_to_multipolygon(&point).is_err());

        let mut truncated = vec![1u8];
        truncated.extend(3u32.to_le_bytes());
        truncated.extend(1u32.to_le_bytes());
        truncated.extend(u32::MAX.to_le_bytes());
        assert!(wkb_to_multipolygon(&truncated).is_err());

        let mut hole_without_exterior = vec![1u8];
        hole_without_exterior.extend(3u32.to_le_bytes());
        hole_without_exterior.extend(2u32.to_le_bytes());
        ring_bytes(&mut hole_without_exterior, &[], true);
        ring_bytes(&mut hole_without_exterior, &square(), true);
        assert!(wkb_to_multipolygon(&hole_without_exterior).is_err());

        let mut trailing = empty.clone();
        trailing.push(0);
        assert!(wkb_to_multipolygon(&trailing).is_err());
        assert!(wkb_to_multipolygon(&[]).is_err());
        assert!(wkb_to_multipolygon(&[7]).is_err());
    }
}
