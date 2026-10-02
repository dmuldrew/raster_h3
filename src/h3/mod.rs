//! The in-tree H3 geospatial indexing system.
//!
//! H3 is a geospatial indexing system using a hexagonal grid that can be
//! (approximately) subdivided into finer and finer hexagonal grids, combining
//! the benefits of a hexagonal grid with S2's hierarchical subdivisions.
//!
//! ## Crate features
//!
//! * **std** -
//! When enabled, this will cause `h3o` to use the standard library. In terms of
//! APIs, `std` causes error types to implement the `std::error::Error` trait.
//! Enabling `std` will also result in performance optimizations.
//!
//! * **geo** -
//! When enabled, you'll be able to convert lists of H3 cell indexes from and
//! into geometric shapes. Also enables the `GeoJSON` support. Requires `std`.
//!
//! * **serde** -
//! When enabled, H3 index types (cell, vertex and edge) derive serde traits.
//!
//! ## H3 to H3O mapping
//!
//! For people used to the H3 API, here is the mapping to H3O.
//!
//! ### Indexing functions
//!
//! | H3               | H3O                        |
//! | :--------------- | :------------------------- |
//! | `latLngToCell`   | [`LatLng::to_cell`]        |
//! | `cellToLatLng`   | [`LatLng::from`](./struct.LatLng.html#impl-From<CellIndex>-for-LatLng) |
//! | `cellToBoundary` | [`CellIndex::boundary`]    |
//!
//! ### Index inspection functions
//!
//! | H3                    | H3O                              |
//! | :-------------------- | :------------------------------- |
//! | `getResolution`       | [`CellIndex::resolution`]        |
//! | `getBaseCellNumber`   | [`CellIndex::base_cell`]         |
//! | `stringToH3`          | [`str::parse`]                   |
//! | `h3ToString`          | [`ToString::to_string`]          |
//! | `isValidCell`         | [`CellIndex::try_from`](./struct.CellIndex.html#impl-TryFrom<u64>-for-CellIndex) |
//! | `isResClassIII`       | [`Resolution::is_class3`]        |
//! | `isPentagon`          | [`CellIndex::is_pentagon`]       |
//! | `getIcosahedronFaces` | [`CellIndex::icosahedron_faces`] |
//! | `maxFaceCount`        | [`CellIndex::max_face_count`]    |
//!
//! ### Grid traversal functions
//!
//! | H3                        | H3O                                     |
//! | :------------------------ | :-------------------------------------- |
//! | `gridDisk`                | [`CellIndex::grid_disk`]                |
//! | `maxGridDiskSize`         | [`max_grid_disk_size`]                  |
//! | `gridDiskDistances`       | [`CellIndex::grid_disk_distances`]      |
//! | `gridDiskUnsafe`          | [`CellIndex::grid_disk_fast`]           |
//! | `gridDiskDistancesUnsafe` | [`CellIndex::grid_disk_distances_fast`] |
//! | `gridDiskDistancesSafe`   | [`CellIndex::grid_disk_distances_safe`] |
//! | `gridDisksUnsafe`         | [`CellIndex::grid_disks_fast`]          |
//! | `gridRingUnsafe`          | [`CellIndex::grid_ring_fast`]           |
//! | `gridPathCells`           | [`CellIndex::grid_path_cells`]          |
//! | `gridPathCellsSize`       | [`CellIndex::grid_path_cells_size`]     |
//! | `gridDistance`            | [`CellIndex::grid_distance`]            |
//! | `cellToLocalIj`           | [`CellIndex::to_local_ij`]              |
//! | `localIjToCell`           | [`CellIndex::try_from`](./struct.CellIndex.html#impl-TryFrom<LocalIJ>-for-CellIndex) |
//!
//! ### Hierarchical grid functions
//!
//! | H3                      | H3O                           |
//! | :---------------------- | :---------------------------- |
//! | `cellToParent`          | [`CellIndex::parent`]         |
//! | `cellToChildren`        | [`CellIndex::children`]       |
//! | `cellToChildrenSize`    | [`CellIndex::children_count`] |
//! | `cellToCenterChild`     | [`CellIndex::center_child`]   |
//! | `cellToChildPos`        | [`CellIndex::child_position`] |
//! | `childPosToCell`        | [`CellIndex::child_at`]       |
//! | `compactCells`          | [`CellIndex::compact`]        |
//! | `uncompactCells`        | [`CellIndex::uncompact`]      |
//! | `uncompactCellsSize`    | [`CellIndex::uncompact_size`] |
//!
//! ### Region functions
//!
//! | H3                      | H3O                                |
//! | :---------------------- | :--------------------------------- |
//! | `polygonToCells`        | [`geom::ToCells::to_cells`]        |
//! | `maxPolygonToCellsSize` | [`geom::ToCells::max_cells_count`] |
//! | `h3SetToLinkedGeo`      | [`geom::ToGeo::to_geom`]           |
//! | `destroyLinkedPolygon`  | N/A                                |
//!
//! ### Directed edge functions
//!
//! | H3                           | H3O                                |
//! | :--------------------------- | :--------------------------------- |
//! | `areNeighborCells`           | [`CellIndex::is_neighbor_with`]    |
//! | `cellsToDirectedEdge`        | [`CellIndex::edge`]                |
//! | `isValidDirectedEdge`        | [`DirectedEdgeIndex::try_from`](./struct.DirectedEdgeIndex.html#impl-TryFrom<u64>-for-DirectedEdgeIndex) |
//! | `getDirectedEdgeOrigin`      | [`DirectedEdgeIndex::origin`]      |
//! | `getDirectedEdgeDestination` | [`DirectedEdgeIndex::destination`] |
//! | `directedEdgeToCells`        | [`DirectedEdgeIndex::cells`]       |
//! | `originToDirectedEdges`      | [`CellIndex::edges`]               |
//! | `directedEdgeToBoundary`     | [`DirectedEdgeIndex::boundary`]    |
//!
//! ### Vertex functions
//!
//! | H3               | H3O                       |
//! | :--------------- | :------------------------ |
//! | `cellToVertex`   | [`CellIndex::vertex`]     |
//! | `cellToVertexes` | [`CellIndex::vertexes`]   |
//! | `vertexToLatLng` | [`LatLng::from`](./struct.LatLng.html#impl-From<VertexIndex>-for-LatLng) |
//! | `isValidVertex`  | [`VertexIndex::try_from`](./struct.VertexIndex.html#impl-TryFrom<u64>-for-VertexIndex) |
//!
//! ### Miscellaneous H3 functions
//!
//! | H3                          | H3O                                |
//! | :-------------------------- | :--------------------------------- |
//! | `degsToRads`                | [`f64::to_radians`]                |
//! | `radsToDegs`                | [`f64::to_degrees`]                |
//! | `getHexagonAreaAvgKm2`      | [`Resolution::area_km2`]           |
//! | `getHexagonAreaAvgM2`       | [`Resolution::area_m2`]            |
//! | `cellAreaKm2`               | [`CellIndex::area_km2`]            |
//! | `cellAreaM2`                | [`CellIndex::area_m2`]             |
//! | `cellAreaRads2`             | [`CellIndex::area_rads2`]          |
//! | `getHexagonEdgeLengthAvgKm` | [`Resolution::edge_length_km`]     |
//! | `getHexagonEdgeLengthAvgM`  | [`Resolution::edge_length_m`]      |
//! | `edgeLengthKm`              | [`DirectedEdgeIndex::length_km`]   |
//! | `edgeLengthM`               | [`DirectedEdgeIndex::length_m`]    |
//! | `edgeLengthRads`            | [`DirectedEdgeIndex::length_rads`] |
//! | `getNumCells`               | [`Resolution::cell_count`]         |
//! | `getRes0Cells`              | [`CellIndex::base_cells`]          |
//! | `res0CellCount`             | [`BaseCell::count`]                |
//! | `getPentagons`              | [`Resolution::pentagons`]          |
//! | `pentagonCount`             | [`Resolution::pentagon_count`]     |
//! | `greatCircleDistanceKm`     | [`LatLng::distance_km`]            |
//! | `greatCircleDistanceM`      | [`LatLng::distance_m`]             |
//! | `greatCircleDistanceRads`   | [`LatLng::distance_rads`]          |

#![allow(warnings)]
#![allow(clippy::all)]

extern crate alloc;

mod base_cell;
mod boundary;
mod coord;
pub use coord::certificate;
mod direction;
pub mod error;
mod face;
mod grid;
mod index;
mod resolution;

#[path = "math-std.rs"]
mod math;

pub use base_cell::BaseCell;
pub use boundary::Boundary;
pub use coord::{CoordIJ, LatLng, LocalIJ};
pub use direction::Direction;
pub use face::{Face, FaceSet};
pub use index::{
    CellIndex, DirectedEdgeIndex, Edge, IndexMode, Vertex, VertexIndex,
};
pub use resolution::Resolution;

use resolution::ExtendedResolution;

// -----------------------------------------------------------------------------

/// An icosahedron has 20 faces.
const NUM_ICOSA_FACES: usize = 20;
// The number of vertices in a hexagon.
const NUM_HEX_VERTS: u8 = 6;
// The number of vertices in a pentagon.
const NUM_PENT_VERTS: u8 = 5;

/// Direction: counterclockwise
const CCW: bool = true;
/// Direction: clockwise
const CW: bool = false;

/// Earth radius in kilometers using WGS84 authalic radius.
pub const EARTH_RADIUS_KM: f64 = 6371.007180918475_f64;

/// Number of pentagon per resolution.
const NUM_PENTAGONS: u8 = 12;

/// Default cell index (resolution 0, base cell 0).
const DEFAULT_CELL_INDEX: u64 = 0x0800_1fff_ffff_ffff;

// 2π
const TWO_PI: f64 = 2. * core::f64::consts::PI;

// -----------------------------------------------------------------------------

/// Maximum number of indices produced by the grid disk algorithm with the given
/// `k`.
///
/// # Example
///
/// ```
/// let count = h3o::max_grid_disk_size(3);
/// ```
#[must_use]
pub const fn max_grid_disk_size(k: u32) -> u64 {
    // k value which will encompass all cells at resolution 15.
    // This is the largest possible k in the H3 grid system.
    const K_MAX: u32 = 13_780_510;

    if k >= K_MAX {
        return Resolution::Fifteen.cell_count();
    }

    let k = k as u64;
    // Formula source and proof: https://oeis.org/A003215
    3 * k * (k + 1) + 1
}
