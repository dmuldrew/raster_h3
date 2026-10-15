//! Helmert datum shifts (`+towgs84`) against PROJ (`cs2cs` 9.8.1).
//!
//! Each reference point was produced by projecting a WGS84 location into the
//! CRS with `cs2cs`, rounding to whole metres, and inverting with `cs2cs` using
//! the identical PROJ definition (same `+towgs84`). That checks the
//! implementation to 0.01 m. A second set of references is PROJ's default
//! `EPSG:<code> -> EPSG:4326` transformation (national grids where PROJ has
//! them), which checks absolute accuracy against the EPSG-published accuracy
//! of the chosen Helmert transformation.

use raster_h3::crs::transformer::CrsTransformer;
use raster_h3::error::RasterH3Error;

/// Ground distance in metres (equirectangular; fine at these separations).
fn ground_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let r = 6_378_137.0_f64;
    let lat = ((a.1 + b.1) / 2.0).to_radians();
    let dx = (a.0 - b.0).to_radians() * lat.cos() * r;
    let dy = (a.1 - b.1).to_radians() * r;
    dx.hypot(dy)
}

struct Reference {
    epsg: u32,
    xy: (f64, f64),
    /// `cs2cs` with the same `+towgs84` as `from_epsg_code`.
    same_helmert: (f64, f64),
    /// `cs2cs EPSG:<code> EPSG:4326` (PROJ's preferred transformation).
    proj_default: (f64, f64),
    /// EPSG-published accuracy of the Helmert transformation used, metres.
    accuracy_m: f64,
}

#[rustfmt::skip]
const REFERENCES: &[Reference] = &[
    // British National Grid, OSGB36 to WGS 84 (6), EPSG:1314, 2 m. PROJ default uses EPSG:7710 (OSTN15-derived, 1 m).
    Reference { epsg: 27700, xy: (530043.0, 180358.0), same_helmert: (-0.1276028722, 51.5071981654), proj_default: (-0.1275777380, 51.5071995681), accuracy_m: 2.0 },
    Reference { epsg: 27700, xy: (325897.0, 674001.0), same_helmert: (-3.1883034244, 55.9532981548), proj_default: (-3.1883049296, 55.9532932832), accuracy_m: 2.0 },
    // Amersfoort / RD New, Amersfoort to WGS 84 (4), EPSG:4833, 1 m.
    Reference { epsg: 28992, xy: (122097.0, 486745.0), same_helmert: (4.9041027880, 52.3676012649), proj_default: (4.9041027880, 52.3676012649), accuracy_m: 1.0 },
    Reference { epsg: 28992, xy: (194826.0, 469394.0), same_helmert: (5.9699004336, 52.2112020906), proj_default: (5.9699004336, 52.2112020906), accuracy_m: 1.0 },
    // DHDN / 3-degree Gauss-Kruger, DHDN to WGS 84 (2), EPSG:1777, 3 m.
    Reference { epsg: 31467, xy: (3513519.0, 5404271.0), same_helmert: (9.1829031418, 48.7757996070), proj_default: (9.1829076740, 48.7757961980), accuracy_m: 3.0 },
    Reference { epsg: 31467, xy: (3477335.0, 5552791.0), same_helmert: (8.6821026263, 50.1109022319), proj_default: (8.6820953825, 50.1109011458), accuracy_m: 3.0 },
    Reference { epsg: 31469, xy: (5465038.0, 5651760.0), same_helmert: (14.5000046884, 51.0000028842), proj_default: (14.4999911492, 51.0000057135), accuracy_m: 3.0 },
    // ED50 / UTM, ED50 to WGS 84 (1), EPSG:1133, 10 m.
    Reference { epsg: 23030, xy: (440392.0, 4474464.0), same_helmert: (-3.7038019394, 40.4168014921), proj_default: (-3.7038926249, 40.4167917519), accuracy_m: 10.0 },
    Reference { epsg: 23032, xy: (514937.0, 5034735.0), same_helmert: (9.1900031092, 45.4641982837), proj_default: (9.1900096051, 45.4642035076), accuracy_m: 10.0 },
    // ETRS89 treated as WGS84 (no shift), as PROJ does by default.
    Reference { epsg: 2154, xy: (652469.0, 6862035.0), same_helmert: (2.3521997195, 48.8565976654), proj_default: (2.3521997195, 48.8565976654), accuracy_m: 1.0 },
    Reference { epsg: 3035, xy: (4552036.0, 3273268.0), same_helmert: (13.4049931851, 52.5199977339), proj_default: (13.4049931851, 52.5199977339), accuracy_m: 1.0 },
    Reference { epsg: 25832, xy: (692095.0, 5334541.0), same_helmert: (11.5819982145, 48.1351033158), proj_default: (11.5819982145, 48.1351033158), accuracy_m: 1.0 },
];

#[test]
fn helmert_shifts_match_proj_with_identical_parameters() {
    for r in REFERENCES {
        let tf = CrsTransformer::from_epsg_code(r.epsg).unwrap();
        let got = tf.transform_point(r.xy.0, r.xy.1).unwrap();
        let err = ground_m(got, r.same_helmert);
        assert!(
            err <= 0.01,
            "EPSG:{} {:?}: {got:?} is {err:.4} m from PROJ",
            r.epsg,
            r.xy
        );
    }
}

#[test]
fn helmert_shifts_are_within_published_accuracy_of_proj_default() {
    for r in REFERENCES {
        let tf = CrsTransformer::from_epsg_code(r.epsg).unwrap();
        let got = tf.transform_point(r.xy.0, r.xy.1).unwrap();
        let err = ground_m(got, r.proj_default);
        assert!(
            err <= r.accuracy_m,
            "EPSG:{}: {err:.2} m from PROJ's default exceeds the {} m accuracy",
            r.epsg,
            r.accuracy_m
        );
    }
}

#[test]
fn helmert_inverse_round_trips() {
    for r in REFERENCES {
        let tf = CrsTransformer::from_epsg_code(r.epsg).unwrap();
        let (lon, lat) = tf.transform_point(r.xy.0, r.xy.1).unwrap();
        let (x, y) = tf.wgs84_to_source(lon, lat).unwrap();
        let err = (x - r.xy.0).hypot(y - r.xy.1);
        assert!(err <= 0.01, "EPSG:{}: round trip off by {err:.4} m", r.epsg);
    }
}

const OSGB36_TOWGS84: &str = "+towgs84=446.448,-125.157,542.06,0.15,0.247,0.842,-20.489";

#[test]
fn explicit_towgs84_is_accepted_and_matches_named_datum() {
    let explicit =
        CrsTransformer::from_proj_string(&format!("+proj=longlat +ellps=airy {OSGB36_TOWGS84}"))
            .unwrap();
    // cs2cs: (-1.0015262258, 52.0004395745)
    let got = explicit.transform_point(-1.0, 52.0).unwrap();
    assert!(
        ground_m(got, (-1.0015262258, 52.0004395745)) <= 0.01,
        "{got:?}"
    );

    // proj4rs' built-in OSGB36 parameters differ only in rounding (mm level).
    let named = CrsTransformer::from_proj_string("+proj=longlat +datum=OSGB36").unwrap();
    let got_named = named.transform_point(-1.0, 52.0).unwrap();
    assert!(ground_m(got, got_named) <= 0.02);
}

#[test]
fn shift_composes_with_prime_meridian_and_axis_order() {
    // cs2cs: +pm is applied before the datum shift.
    let pm = CrsTransformer::from_proj_string(
        "+proj=longlat +ellps=intl +pm=paris +towgs84=-87,-98,-121",
    )
    .unwrap();
    let got = pm.transform_point(-1.0, 52.0).unwrap();
    assert!(
        ground_m(got, (1.3358322051, 51.9991652488)) <= 0.01,
        "{got:?}"
    );
    let back = pm.wgs84_to_source(got.0, got.1).unwrap();
    assert!(ground_m(back, (-1.0, 52.0)) <= 0.01, "{back:?}");

    let neu = CrsTransformer::from_proj_string(&format!(
        "+proj=longlat +ellps=airy +axis=neu {OSGB36_TOWGS84}"
    ))
    .unwrap();
    let got = neu.transform_point(52.0, -1.0).unwrap();
    assert!(
        ground_m(got, (-1.0015262258, 52.0004395745)) <= 0.01,
        "{got:?}"
    );
}

#[test]
fn nonzero_shift_never_takes_a_fast_path() {
    for def in [
        "+proj=longlat +ellps=WGS84 +towgs84=100,0,0",
        "+proj=merc +a=6378137 +b=6378137 +towgs84=0,0,10",
        "+proj=aea +lat_1=29.5 +lat_2=45.5 +lon_0=-96 +ellps=GRS80 +towgs84=1,2,3",
    ] {
        let tf = CrsTransformer::from_proj_string(def).unwrap();
        assert!(matches!(tf, CrsTransformer::Proj4 { .. }), "{def}");
    }
    // A zero shift keeps the analytical path.
    assert!(matches!(
        CrsTransformer::from_proj_string("+proj=longlat +ellps=WGS84 +towgs84=0,0,0").unwrap(),
        CrsTransformer::Wgs84Identity
    ));
}

#[test]
fn grid_shift_datums_are_rejected_clearly() {
    for def in [
        "+proj=longlat +datum=NAD27",
        "+proj=longlat +ellps=clrk66 +nadgrids=conus",
    ] {
        match CrsTransformer::from_proj_string(def) {
            Err(RasterH3Error::CrsError(msg)) => assert!(msg.contains("grid"), "{def}: {msg}"),
            other => panic!("{def}: expected CrsError, got {:?}", other.map(|_| ())),
        }
    }
}
