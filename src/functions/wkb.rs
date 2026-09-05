use h3o::CellIndex;

/// Convert an H3 cell into an OGC standard 2D Polygon Well-Known Binary (WKB) representation
/// directly into a stack-allocated buffer (zero heap allocations, ~10-15ns).
///
/// Returns the number of bytes written to `buf` (125 bytes for hexagons, 109 bytes for pentagons).
#[inline]
pub fn cell_to_wkb(cell: CellIndex, buf: &mut [u8; 128]) -> usize {
    // 1. Byte order: 1 = Little-Endian (NDR)
    buf[0] = 1;

    // 2. Geometry Type: 3 = wkbPolygon (2D)
    buf[1..5].copy_from_slice(&3u32.to_le_bytes());

    // 3. Number of Rings: 1 (exterior ring)
    buf[5..9].copy_from_slice(&1u32.to_le_bytes());

    // Points start at offset 13 (reserving bytes 9..13 for num_points)
    let mut offset = 13;
    let mut count = 0u32;
    let mut first_lng = 0.0f64;
    let mut first_lat = 0.0f64;

    for v in cell.boundary().iter() {
        let lng = v.lng();
        let lat = v.lat();
        if count == 0 {
            first_lng = lng;
            first_lat = lat;
        }
        buf[offset..offset + 8].copy_from_slice(&lng.to_le_bytes());
        buf[offset + 8..offset + 16].copy_from_slice(&lat.to_le_bytes());
        offset += 16;
        count += 1;
    }

    // Close the linear ring by repeating the first vertex
    buf[offset..offset + 8].copy_from_slice(&first_lng.to_le_bytes());
    buf[offset + 8..offset + 16].copy_from_slice(&first_lat.to_le_bytes());
    offset += 16;
    let total_points = count + 1;

    // 4. Number of Points in Ring
    buf[9..13].copy_from_slice(&total_points.to_le_bytes());

    offset
}

/// Convert a raw 64-bit H3 index to standard 2D WKB polygon bytes.
/// Returns `None` if the index is not a valid H3 cell.
#[inline]
pub fn h3_index_to_wkb(h3_index: u64, buf: &mut [u8; 128]) -> Option<usize> {
    let cell = CellIndex::try_from(h3_index).ok()?;
    Some(cell_to_wkb(cell, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cell_to_wkb_hexagon() {
        // San Francisco cell at res 8
        let coord = h3o::LatLng::new(37.7749, -122.4194).expect("valid latlng");
        let cell = coord.to_cell(h3o::Resolution::Eight);
        assert!(!cell.is_pentagon());

        let mut buf = [0u8; 128];
        let len = cell_to_wkb(cell, &mut buf);

        // Hexagon: 1 + 4 + 4 + 4 + 7*16 = 125 bytes
        assert_eq!(len, 125);

        // Byte order: Little Endian
        assert_eq!(buf[0], 1);

        // Geom type: 3 (Polygon)
        let geom_type = u32::from_le_bytes(buf[1..5].try_into().unwrap());
        assert_eq!(geom_type, 3);

        // Num rings: 1
        let num_rings = u32::from_le_bytes(buf[5..9].try_into().unwrap());
        assert_eq!(num_rings, 1);

        // Num points: 7 (6 vertices + 1 closing)
        let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(num_points, 7);

        // Verify first and last points match (closed ring)
        let first_x = f64::from_le_bytes(buf[13..21].try_into().unwrap());
        let first_y = f64::from_le_bytes(buf[21..29].try_into().unwrap());
        let last_x = f64::from_le_bytes(buf[109..117].try_into().unwrap());
        let last_y = f64::from_le_bytes(buf[117..125].try_into().unwrap());

        assert_eq!(first_x, last_x);
        assert_eq!(first_y, last_y);

        // Verify coordinates are reasonable for San Francisco
        assert!(first_x < -120.0 && first_x > -125.0);
        assert!(first_y > 35.0 && first_y < 40.0);
    }

    #[test]
    fn test_cell_to_wkb_pentagon() {
        // Find a pentagon base cell
        let pentagon = CellIndex::base_cells()
            .find(|c| c.is_pentagon())
            .expect("pentagon base cell");
        assert!(pentagon.is_pentagon());

        let mut buf = [0u8; 128];
        let len = cell_to_wkb(pentagon, &mut buf);

        // Pentagon: 1 + 4 + 4 + 4 + 6*16 = 109 bytes
        assert_eq!(len, 109);

        let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(num_points, 6);

        // Closed ring
        let first_x = f64::from_le_bytes(buf[13..21].try_into().unwrap());
        let first_y = f64::from_le_bytes(buf[21..29].try_into().unwrap());
        let last_x = f64::from_le_bytes(buf[93..101].try_into().unwrap());
        let last_y = f64::from_le_bytes(buf[101..109].try_into().unwrap());

        assert_eq!(first_x, last_x);
        assert_eq!(first_y, last_y);
    }

    #[test]
    fn test_h3_index_to_wkb_invalid() {
        let mut buf = [0u8; 128];
        assert!(h3_index_to_wkb(0, &mut buf).is_none());
        assert!(h3_index_to_wkb(0xffffffffffffffffu64, &mut buf).is_none());
    }
}
