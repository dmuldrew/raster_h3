use geo_types::{Coord, LineString, MultiPolygon, Polygon};
use h3o::geom::{
    ContainmentMode, MultiPolygon as H3MultiPolygon, PolyfillConfig, Polygon as H3Polygon, ToCells,
};
use h3o::Resolution;
use raster_h3_core::scalar_ops::h3_polygon_to_cells_wkb;

enum TestGeometry {
    Polygon(Polygon<f64>),
    MultiPolygon(MultiPolygon<f64>),
}

impl TestGeometry {
    fn wkb(&self) -> Vec<u8> {
        match self {
            Self::Polygon(polygon) => polygon_wkb(polygon),
            Self::MultiPolygon(multipolygon) => {
                let mut wkb = vec![1];
                wkb.extend(6u32.to_le_bytes());
                wkb.extend((multipolygon.0.len() as u32).to_le_bytes());
                for polygon in &multipolygon.0 {
                    wkb.extend(polygon_wkb(polygon));
                }
                wkb
            }
        }
    }

    fn reference_cells(&self, resolution: Resolution, containment: ContainmentMode) -> Vec<u64> {
        let config = PolyfillConfig::new(resolution).containment_mode(containment);
        let cells = match self {
            Self::Polygon(polygon) => H3Polygon::from_degrees(polygon.clone())
                .unwrap()
                .to_cells(config)
                .map(u64::from)
                .collect(),
            Self::MultiPolygon(multipolygon) => H3MultiPolygon::from_degrees(multipolygon.clone())
                .unwrap()
                .to_cells(config)
                .map(u64::from)
                .collect(),
        };
        sorted_unique(cells)
    }
}

fn sorted_unique(mut cells: Vec<u64>) -> Vec<u64> {
    cells.sort_unstable();
    cells.dedup();
    cells
}

fn line_string(points: &[(f64, f64)]) -> LineString<f64> {
    LineString::new(points.iter().map(|&(x, y)| Coord { x, y }).collect())
}

fn rectangle(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Polygon<f64> {
    Polygon::new(
        line_string(&[
            (min_x, min_y),
            (max_x, min_y),
            (max_x, max_y),
            (min_x, max_y),
            (min_x, min_y),
        ]),
        Vec::new(),
    )
}

fn polygon_wkb(polygon: &Polygon<f64>) -> Vec<u8> {
    let mut wkb = vec![1];
    wkb.extend(3u32.to_le_bytes());
    wkb.extend((1_u32 + polygon.interiors().len() as u32).to_le_bytes());
    for ring in std::iter::once(polygon.exterior()).chain(polygon.interiors()) {
        wkb.extend((ring.0.len() as u32).to_le_bytes());
        for point in &ring.0 {
            wkb.extend(point.x.to_le_bytes());
            wkb.extend(point.y.to_le_bytes());
        }
    }
    wkb
}

#[test]
fn polygon_to_cells_matches_h3o_for_containment_modes_and_overlaps() {
    let exterior_with_hole = Polygon::new(
        line_string(&[
            (-122.49, 37.71),
            (-122.35, 37.71),
            (-122.35, 37.83),
            (-122.49, 37.83),
            (-122.49, 37.71),
        ]),
        vec![line_string(&[
            (-122.45, 37.75),
            (-122.40, 37.75),
            (-122.40, 37.79),
            (-122.45, 37.79),
            (-122.45, 37.75),
        ])],
    );
    let geometries = [
        (
            "simple polygon",
            TestGeometry::Polygon(rectangle(-122.47, 37.73, -122.37, 37.81)),
        ),
        (
            "polygon with a hole",
            TestGeometry::Polygon(exterior_with_hole),
        ),
        (
            "overlapping multipolygon",
            TestGeometry::MultiPolygon(MultiPolygon::new(vec![
                rectangle(-122.48, 37.73, -122.40, 37.81),
                rectangle(-122.43, 37.73, -122.35, 37.81),
            ])),
        ),
    ];
    let modes = [
        ContainmentMode::ContainsCentroid,
        ContainmentMode::ContainsBoundary,
        ContainmentMode::IntersectsBoundary,
        ContainmentMode::Covers,
    ];

    for (name, geometry) in &geometries {
        let wkb = geometry.wkb();
        for resolution in [6_u8, 7] {
            for mode in modes {
                let expected =
                    geometry.reference_cells(Resolution::try_from(resolution).unwrap(), mode);
                let actual = h3_polygon_to_cells_wkb(&wkb, i64::from(resolution), mode).unwrap();
                assert_eq!(
                    actual, expected,
                    "{name}, resolution {resolution}, containment {mode:?}"
                );
                assert!(
                    actual.windows(2).all(|pair| pair[0] < pair[1]),
                    "{name}, resolution {resolution}, containment {mode:?}: \
                     output must be sorted and deduplicated"
                );
            }
        }
    }
}
