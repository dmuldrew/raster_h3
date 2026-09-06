use std::sync::Arc;
use fxhash::FxHashMap;
use crate::error::{RasterH3Error, Result};

/// Action to take for categories not explicitly matched by any remapping rule
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmappedAction {
    /// Pass through the original category value unchanged (default)
    PassThrough,
    /// Drop unmapped categories (treated as nodata/null, excluded from aggregation)
    Drop,
    /// Map unmapped categories to a default category ID
    MapTo(i64),
}

/// A single remapping rule matching an exact value or inclusive range
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemapRule {
    /// Match an exact category value
    Exact(i64, Option<i64>),
    /// Match an inclusive range [start, end]
    Range(i64, i64, Option<i64>),
}

/// High-performance categorical class remapper with L1-cache direct array lookup table
#[derive(Debug, Clone)]
pub struct CategoryRemapper {
    /// Dense array lookup table for non-negative categories where max <= 65535.
    /// Index = category value, Value = Some(target) or None (dropped).
    lut: Option<Vec<Option<i64>>>,
    /// Exact value lookup map for sparse/negative/large values
    direct_map: FxHashMap<i64, Option<i64>>,
    /// Range rules for sparse/negative/large values
    range_rules: Vec<(i64, i64, Option<i64>)>,
    /// Fallback action when a category is not matched
    unmapped_action: UnmappedAction,
}

impl CategoryRemapper {
    /// Create a new CategoryRemapper with the given rules and fallback action
    pub fn new(rules: Vec<RemapRule>, unmapped_action: UnmappedAction) -> Self {
        let mut direct_map = FxHashMap::default();
        let mut range_rules = Vec::new();

        let mut min_val: i64 = 0;
        let mut max_val: i64 = 0;
        let mut has_values = false;

        for rule in &rules {
            match *rule {
                RemapRule::Exact(src, target) => {
                    direct_map.insert(src, target);
                    if !has_values {
                        min_val = src;
                        max_val = src;
                        has_values = true;
                    } else {
                        min_val = min_val.min(src);
                        max_val = max_val.max(src);
                    }
                }
                RemapRule::Range(start, end, target) => {
                    let (s, e) = if start <= end { (start, end) } else { (end, start) };
                    range_rules.push((s, e, target));
                    if !has_values {
                        min_val = s;
                        max_val = e;
                        has_values = true;
                    } else {
                        min_val = min_val.min(s);
                        max_val = max_val.max(e);
                    }
                }
            }
        }

        // Check if we can build a dense LUT (non-negative categories, max <= 65535)
        let lut = if has_values && min_val >= 0 && max_val <= 65535 {
            let size = (max_val as usize) + 1;
            let mut table = Vec::with_capacity(size);

            for i in 0..size {
                let cat = i as i64;
                let mapped = if let Some(&target) = direct_map.get(&cat) {
                    target
                } else {
                    let mut found = None;
                    for &(s, e, target) in &range_rules {
                        if cat >= s && cat <= e {
                            found = Some(target);
                            break;
                        }
                    }
                    if let Some(target) = found {
                        target
                    } else {
                        match unmapped_action {
                            UnmappedAction::PassThrough => Some(cat),
                            UnmappedAction::Drop => None,
                            UnmappedAction::MapTo(v) => Some(v),
                        }
                    }
                };
                table.push(mapped);
            }
            Some(table)
        } else {
            None
        };

        Self {
            lut,
            direct_map,
            range_rules,
            unmapped_action,
        }
    }

    /// Remap a category value to its target category or None (dropped/nodata).
    /// Executed on the inner pixel loop of worker threads.
    #[inline(always)]
    pub fn remap(&self, cat: i64) -> Option<i64> {
        if let Some(ref lut) = self.lut {
            if cat >= 0 && (cat as usize) < lut.len() {
                // Direct L1-cache flat array lookup (1 cycle, ~0.3 ns)
                return lut[cat as usize];
            }
        }
        self.remap_fallback(cat)
    }

    #[inline(never)]
    fn remap_fallback(&self, cat: i64) -> Option<i64> {
        if let Some(&target) = self.direct_map.get(&cat) {
            return target;
        }
        for &(s, e, target) in &self.range_rules {
            if cat >= s && cat <= e {
                return target;
            }
        }
        match self.unmapped_action {
            UnmappedAction::PassThrough => Some(cat),
            UnmappedAction::Drop => None,
            UnmappedAction::MapTo(v) => Some(v),
        }
    }

    /// Parse a remapping specification string.
    ///
    /// Supported formats:
    /// - Range mapping: `'101..109: 1'` or `'101..=109: 1'` or `'101-109: 1'`
    /// - Individual values: `'42: 1, 43: 2'`
    /// - Value lists: `'[101, 102, 103]: 1'` or `'101, 102: 1'`
    /// - Drop / Nodata: `'99: null'` or `'99: nodata'`
    /// - Fallback: `'else: null'` or `'default: 0'`
    /// - Enclosing braces: `'{101..109: 1, 121..124: 2}'`
    /// - JSON dicts: `'{"101": 1, "102": 1}'`
    pub fn parse(spec: &str) -> Result<Self> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err(RasterH3Error::InvalidParameter(
                "Remap specification cannot be empty".to_string(),
            ));
        }

        // Strip outer enclosing braces { ... } or brackets [ ... ] if present
        let mut clean = trimmed;
        if (clean.starts_with('{') && clean.ends_with('}'))
            || (clean.starts_with('[') && clean.ends_with(']'))
        {
            clean = &clean[1..clean.len() - 1].trim();
        }

        let mut rules = Vec::new();
        let mut unmapped_action = UnmappedAction::PassThrough;

        // Split by comma or semicolon, respecting potential brackets [1, 2]
        let entries = split_entries(clean);

        for entry in entries {
            let item = entry.trim();
            if item.is_empty() {
                continue;
            }

            // Split by ':' or '=>' or '->'
            let (src_part, target_part) = if let Some(pos) = item.find("=>") {
                (&item[..pos], &item[pos + 2..])
            } else if let Some(pos) = item.find("->") {
                (&item[..pos], &item[pos + 2..])
            } else if let Some(pos) = item.find(':') {
                (&item[..pos], &item[pos + 1..])
            } else {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Invalid remap entry '{}': missing ':' separator (e.g. '101..109: 1')",
                    item
                )));
            };

            let src_str = clean_token(src_part);
            let target_str = clean_token(target_part);

            // Parse target
            let target_opt = parse_target(&target_str)?;

            // Check if left side is fallback ('else', 'default', '*', '_')
            if is_fallback_token(&src_str) {
                unmapped_action = match target_opt {
                    None => UnmappedAction::Drop,
                    Some(v) => UnmappedAction::MapTo(v),
                };
                continue;
            }

            // Parse source(s): could be range, list, or single value
            parse_source_rules(&src_str, target_opt, &mut rules)?;
        }

        Ok(Self::new(rules, unmapped_action))
    }

    /// Helper to wrap CategoryRemapper in Arc for multi-threaded sharing
    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

fn is_fallback_token(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "else" | "default" | "*" | "_"
    )
}

fn clean_token(s: &str) -> String {
    let t = s.trim();
    // Strip surrounding single or double quotes
    if (t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')) {
        if t.len() >= 2 {
            return t[1..t.len() - 1].trim().to_string();
        }
    }
    t.to_string()
}

fn parse_target(s: &str) -> Result<Option<i64>> {
    let lower = s.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "null" | "none" | "nodata" | "drop" | "skip"
    ) {
        return Ok(None);
    }
    s.parse::<i64>().map(Some).map_err(|_| {
        RasterH3Error::InvalidParameter(format!(
            "Invalid target category '{}': must be an integer or 'null'",
            s
        ))
    })
}

fn parse_source_rules(src: &str, target: Option<i64>, rules: &mut Vec<RemapRule>) -> Result<()> {
    // 1. Check for list in brackets: [101, 102, 103]
    let trimmed = src.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        for part in inner.split(',') {
            parse_source_rules(part.trim(), target, rules)?;
        }
        return Ok(());
    }

    // 2. Check for range: '101..=109', '101..109', or '101-109'
    if let Some(pos) = trimmed.find("..=") {
        let s = trimmed[..pos].trim().parse::<i64>().map_err(|_| {
            RasterH3Error::InvalidParameter(format!("Invalid range start in '{}'", trimmed))
        })?;
        let e = trimmed[pos + 3..].trim().parse::<i64>().map_err(|_| {
            RasterH3Error::InvalidParameter(format!("Invalid range end in '{}'", trimmed))
        })?;
        rules.push(RemapRule::Range(s, e, target));
        return Ok(());
    }

    if let Some(pos) = trimmed.find("..") {
        let s = trimmed[..pos].trim().parse::<i64>().map_err(|_| {
            RasterH3Error::InvalidParameter(format!("Invalid range start in '{}'", trimmed))
        })?;
        let e = trimmed[pos + 2..].trim().parse::<i64>().map_err(|_| {
            RasterH3Error::InvalidParameter(format!("Invalid range end in '{}'", trimmed))
        })?;
        rules.push(RemapRule::Range(s, e, target));
        return Ok(());
    }

    // Check hyphen range: e.g. "101-109" (ensure hyphen is not leading negative sign)
    if let Some(dash_pos) = trimmed[1..].find('-') {
        let actual_pos = dash_pos + 1;
        let left = trimmed[..actual_pos].trim();
        let right = trimmed[actual_pos + 1..].trim();
        if let (Ok(s), Ok(e)) = (left.parse::<i64>(), right.parse::<i64>()) {
            rules.push(RemapRule::Range(s, e, target));
            return Ok(());
        }
    }

    // 3. Single category integer
    let val = trimmed.parse::<i64>().map_err(|_| {
        RasterH3Error::InvalidParameter(format!(
            "Invalid category specification '{}': must be an integer, range (e.g. 101..109), or 'else'",
            trimmed
        ))
    })?;
    rules.push(RemapRule::Exact(val, target));
    Ok(())
}

fn split_entries(s: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut in_bracket = false;

    for ch in s.chars() {
        match ch {
            '[' => {
                in_bracket = true;
                current.push(ch);
            }
            ']' => {
                in_bracket = false;
                current.push(ch);
            }
            ',' | ';' if !in_bracket => {
                if !current.trim().is_empty() {
                    entries.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        entries.push(current);
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fuel_models_example() {
        let spec = "{101..109: 1, 121..124: 2, 141..149: 3, 161..165: 4, 181..189: 5}";
        let remapper = CategoryRemapper::parse(spec).expect("valid fuel model spec");

        // Grass (101..109 -> 1)
        assert_eq!(remapper.remap(101), Some(1));
        assert_eq!(remapper.remap(105), Some(1));
        assert_eq!(remapper.remap(109), Some(1));

        // Grass-Shrub (121..124 -> 2)
        assert_eq!(remapper.remap(121), Some(2));
        assert_eq!(remapper.remap(124), Some(2));

        // Timber-Litter (181..189 -> 5)
        assert_eq!(remapper.remap(185), Some(5));

        // Unmapped passes through by default
        assert_eq!(remapper.remap(91), Some(91));
        assert_eq!(remapper.remap(200), Some(200));
    }

    #[test]
    fn test_parse_with_null_and_else() {
        let spec = "10..15: 1, 20: 2, 99: null, else: null";
        let remapper = CategoryRemapper::parse(spec).expect("valid spec with else null");

        assert_eq!(remapper.remap(10), Some(1));
        assert_eq!(remapper.remap(15), Some(1));
        assert_eq!(remapper.remap(20), Some(2));
        assert_eq!(remapper.remap(99), None); // Explicitly dropped
        assert_eq!(remapper.remap(30), None); // Dropped by else: null
    }

    #[test]
    fn test_parse_bracket_list() {
        let spec = "[10, 20, 30]: 1, 40-50: 2";
        let remapper = CategoryRemapper::parse(spec).expect("valid list spec");

        assert_eq!(remapper.remap(10), Some(1));
        assert_eq!(remapper.remap(20), Some(1));
        assert_eq!(remapper.remap(30), Some(1));
        assert_eq!(remapper.remap(45), Some(2));
        assert_eq!(remapper.remap(100), Some(100)); // Passthrough
    }

    #[test]
    fn test_dense_lut_vs_sparse_fallback_parity() {
        let spec = "1..5: 10, 6..10: 20, else: 0";
        let remapper = CategoryRemapper::parse(spec).unwrap();
        assert!(remapper.lut.is_some());

        for val in 0..20 {
            let res = remapper.remap(val);
            let fallback_res = remapper.remap_fallback(val);
            assert_eq!(res, fallback_res, "mismatch at val {}", val);
        }
    }
}
