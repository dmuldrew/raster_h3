//! PROJ-string tokenizing and the parameter checks shared by the fast paths.

/// Tokenize a PROJ string into key-value pairs.
pub(crate) fn tokenize_proj_string(src: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for token in src.split_whitespace() {
        let token = token.trim().trim_start_matches('+');
        if token.is_empty() {
            continue;
        }
        if let Some((k, v)) = token.split_once('=') {
            map.insert(k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
        } else {
            map.insert(token.to_ascii_lowercase(), String::new());
        }
    }
    map
}

/// Keys every fast path tolerates because they carry no geometric meaning.
const NEUTRAL_KEYS: &[&str] = &["proj", "no_defs", "wktext", "type"];

/// True when every token in `tokens` is understood by a fast path that
/// accepts `keys` in addition to [`NEUTRAL_KEYS`]. Linear units must be
/// metres, any prime meridian Greenwich and any datum shift zero; everything
/// else (`+axis`, `+pm`, `+geoc`, `+lon_wrap`, `+over`, foot units, Helmert
/// shifts, ...) routes to proj4rs, which honours those parameters, rather
/// than being silently dropped.
pub(super) fn fast_path_accepts(
    tokens: &std::collections::HashMap<String, String>,
    keys: &[&str],
) -> bool {
    tokens.iter().all(|(k, v)| match k.as_str() {
        "towgs84" => is_all_zero_towgs84(v),
        "nadgrids" => v == "@null" || v.is_empty(),
        "units" => v == "m",
        "to_meter" => v.parse::<f64>().is_ok_and(|m| m == 1.0),
        "pm" => v == "greenwich" || v.parse::<f64>().is_ok_and(|d| d == 0.0),
        k => NEUTRAL_KEYS.contains(&k) || keys.contains(&k),
    })
}

/// Parse an optional numeric token; returns `Some(default)` when absent and
/// `None` when present but not a finite `f64`, so malformed numbers never
/// silently fall back to defaults on a fast path.
pub(super) fn parse_opt_f64(
    tokens: &std::collections::HashMap<String, String>,
    key: &str,
    default: f64,
) -> Option<f64> {
    match tokens.get(key) {
        Some(v) => v.parse::<f64>().ok().filter(|x| x.is_finite()),
        None => Some(default),
    }
}

/// Helper to check if a +towgs84 parameter list is effectively all-zero (e.g., "0,0,0" or "0,0,0,0,0,0,0").
pub(super) fn is_all_zero_towgs84(val: &str) -> bool {
    let mut count = 0;
    for part in val.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.parse::<f64>() {
            Ok(num) => {
                if num != 0.0 {
                    return false;
                }
                count += 1;
            }
            Err(_) => return false,
        }
    }
    count > 0
}
