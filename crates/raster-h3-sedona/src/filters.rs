//! Translation of DataFusion filter `Expr`s into `raster_h3` [`FilterPredicate`]s.
//!
//! The core aggregator's thresholds are inclusive (`count >= min_count`, `min_mean <= mean
//! <= max_mean`), so inclusive comparisons push down `Exact`. Strict comparisons push the
//! inclusive bound down as `Inexact` (a superset) and DataFusion re-applies the predicate.
//! Spatial predicates on `wkb` push a widened `Bbox` (see the `spatial` module).

use datafusion::arrow::datatypes::DataType;
use datafusion::common::ScalarValue;
use datafusion::logical_expr::{Between, BinaryExpr, Expr, Operator, TableProviderFilterPushDown};
use raster_h3_datafusion::FilterPredicate;

use crate::spatial::translate_spatial;
use crate::table::TableKind;

/// Predicates to push into the scan for one filter, and how exactly they implement it.
pub(crate) struct Translated {
    pub predicates: Vec<FilterPredicate>,
    pub pushdown: TableProviderFilterPushDown,
}

/// `bbox_margin_km` is `None` when the table's cells have no bounded size (compacted
/// output), which disables spatial pushdown.
pub(crate) fn translate(
    expr: &Expr,
    kind: TableKind,
    bbox_margin_km: Option<f64>,
) -> Option<Translated> {
    match expr {
        Expr::ScalarFunction(_) => {
            let bbox = translate_spatial(expr, bbox_margin_km?)?;
            single(
                FilterPredicate::Bbox(bbox),
                TableProviderFilterPushDown::Inexact,
            )
        }
        Expr::BinaryExpr(BinaryExpr { left, op, right }) => {
            if let (Some(col), Some(lit)) = (column_name(left), literal(right)) {
                comparison(&col, *op, lit, kind)
            } else if let (Some(lit), Some(col)) = (literal(left), column_name(right)) {
                comparison(&col, op.swap()?, lit, kind)
            } else {
                None
            }
        }
        Expr::Between(Between {
            expr,
            negated: false,
            low,
            high,
        }) => {
            let col = column_name(expr)?;
            let lower = literal(low).and_then(|l| comparison(&col, Operator::GtEq, l, kind));
            let upper = literal(high).and_then(|h| comparison(&col, Operator::LtEq, h, kind));
            let exact = matches!(
                (&lower, &upper),
                (Some(l), Some(u))
                    if l.pushdown == TableProviderFilterPushDown::Exact
                        && u.pushdown == TableProviderFilterPushDown::Exact
            );
            let predicates: Vec<_> = lower
                .into_iter()
                .chain(upper)
                .flat_map(|t| t.predicates)
                .collect();
            (!predicates.is_empty()).then_some(Translated {
                predicates,
                pushdown: if exact {
                    TableProviderFilterPushDown::Exact
                } else {
                    TableProviderFilterPushDown::Inexact
                },
            })
        }
        _ => None,
    }
}

fn comparison(col: &str, op: Operator, lit: &ScalarValue, kind: TableKind) -> Option<Translated> {
    use TableProviderFilterPushDown::{Exact, Inexact};

    if col == "resolution" {
        // Only an integer literal: `resolution = 8.5` must not truncate to 8.
        if op != Operator::Eq || !lit.data_type().is_integer() {
            return None;
        }
        let r = u8::try_from(as_i64(lit)?).ok()?;
        return single(FilterPredicate::Resolution(r), Exact);
    }

    let v = as_f64(lit)?;
    if v.is_nan() {
        return None;
    }
    let lower = |make: fn(f64) -> FilterPredicate| match op {
        Operator::GtEq => single(make(v), Exact),
        Operator::Gt => single(make(v), Inexact),
        _ => None,
    };
    match (kind, col) {
        (TableKind::Continuous, "count") | (TableKind::Categorical, "total_count") => {
            lower(FilterPredicate::MinCount)
        }
        (TableKind::Categorical, "majority_fraction") => {
            lower(FilterPredicate::MinMajorityFraction)
        }
        (TableKind::Continuous, "mean") => match op {
            Operator::LtEq => single(FilterPredicate::MaxMean(v), Exact),
            Operator::Lt => single(FilterPredicate::MaxMean(v), Inexact),
            _ => lower(FilterPredicate::MinMean),
        },
        _ => None,
    }
}

fn single(predicate: FilterPredicate, pushdown: TableProviderFilterPushDown) -> Option<Translated> {
    Some(Translated {
        predicates: vec![predicate],
        pushdown,
    })
}

/// The column a comparison constrains. A lossless widening cast is unwrapped only for the
/// integer `resolution` column; casts of float columns may truncate and change the predicate.
fn column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Column(c) => Some(c.name.to_ascii_lowercase()),
        Expr::Cast(cast) => match cast.expr.as_ref() {
            Expr::Column(c)
                if c.name.eq_ignore_ascii_case("resolution")
                    && (cast.field.data_type().is_integer()
                        || cast.field.data_type().is_floating()) =>
            {
                Some("resolution".to_string())
            }
            _ => None,
        },
        _ => None,
    }
}

fn literal(expr: &Expr) -> Option<&ScalarValue> {
    match expr {
        Expr::Literal(v, _) if !v.is_null() => Some(v),
        _ => None,
    }
}

fn as_f64(v: &ScalarValue) -> Option<f64> {
    match v.cast_to(&DataType::Float64).ok()? {
        ScalarValue::Float64(Some(f)) => Some(f),
        _ => None,
    }
}

fn as_i64(v: &ScalarValue) -> Option<i64> {
    match v.cast_to(&DataType::Int64).ok()? {
        ScalarValue::Int64(Some(i)) => Some(i),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::prelude::{col, lit};
    use TableProviderFilterPushDown::{Exact, Inexact};

    fn check(
        expr: Expr,
        kind: TableKind,
    ) -> Option<(Vec<FilterPredicate>, TableProviderFilterPushDown)> {
        translate(&expr, kind, None).map(|t| (t.predicates, t.pushdown))
    }

    #[test]
    fn continuous_comparisons() {
        let k = TableKind::Continuous;
        assert_eq!(
            check(col("count").gt_eq(lit(4.0)), k),
            Some((vec![FilterPredicate::MinCount(4.0)], Exact))
        );
        assert_eq!(
            check(col("count").gt(lit(4)), k),
            Some((vec![FilterPredicate::MinCount(4.0)], Inexact))
        );
        assert_eq!(
            check(lit(10.0).gt_eq(col("mean")), k),
            Some((vec![FilterPredicate::MaxMean(10.0)], Exact))
        );
        assert_eq!(
            check(col("mean").between(lit(1.0), lit(2.0)), k),
            Some((
                vec![FilterPredicate::MinMean(1.0), FilterPredicate::MaxMean(2.0)],
                Exact
            ))
        );
        assert_eq!(
            check(col("resolution").eq(lit(8i64)), k),
            Some((vec![FilterPredicate::Resolution(8)], Exact))
        );
        assert!(check(col("resolution").eq(lit(8.5)), k).is_none());
        assert!(check(col("count").lt(lit(4.0)), k).is_none());
        assert!(check(col("stddev").gt_eq(lit(1.0)), k).is_none());
        // `majority_fraction` belongs to categorical tables only.
        assert!(check(col("majority_fraction").gt_eq(lit(0.5)), k).is_none());
    }

    #[test]
    fn categorical_comparisons() {
        let k = TableKind::Categorical;
        assert_eq!(
            check(col("total_count").gt_eq(lit(3.0)), k),
            Some((vec![FilterPredicate::MinCount(3.0)], Exact))
        );
        assert_eq!(
            check(col("majority_fraction").gt(lit(0.5)), k),
            Some((vec![FilterPredicate::MinMajorityFraction(0.5)], Inexact))
        );
        // A count upper bound has no core counterpart, so BETWEEN pushes only the lower bound.
        assert_eq!(
            check(col("total_count").between(lit(1.0), lit(9.0)), k),
            Some((vec![FilterPredicate::MinCount(1.0)], Inexact))
        );
        assert!(check(col("mean").gt_eq(lit(1.0)), k).is_none());
    }
}
