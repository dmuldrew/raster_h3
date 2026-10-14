use std::hash::{BuildHasher, Hasher};

/// SplitMix64 finalizer over a raw H3 index.
///
/// H3 fills unused child digits with ones, so at resolution R the low
/// `3 * (15 - R)` bits of every index are identical. A multiplicative hash
/// such as FxHash leaves those low bits nearly constant, and hashbrown picks
/// buckets from the low bits, so tables degrade toward linear probing. This
/// finalizer folds the varying base-cell and child digits into every output
/// bit. Arithmetic stays in u64 on both 32- and 64-bit hosts.
#[inline(always)]
pub fn mix_h3(cell_u64: u64) -> u64 {
    let mut h = cell_u64;
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// `BuildHasher` for maps and sets keyed by raw `u64` H3 indices.
#[derive(Clone, Copy, Debug, Default)]
pub struct H3BuildHasher;

impl BuildHasher for H3BuildHasher {
    type Hasher = H3Hasher;

    #[inline(always)]
    fn build_hasher(&self) -> H3Hasher {
        H3Hasher(0)
    }
}

/// Hasher produced by [`H3BuildHasher`]. A single `u64` key hashes to
/// `mix_h3(key)`; other writes are folded through the same finalizer.
#[derive(Clone, Copy, Debug, Default)]
pub struct H3Hasher(u64);

impl Hasher for H3Hasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline(always)]
    fn write_u64(&mut self, n: u64) {
        self.0 = mix_h3(self.0.rotate_left(5) ^ n);
    }

    /// Fallback for non-`u64` keys: fold 8-byte little-endian words, with
    /// the length mixed in so trailing zero bytes still change the hash.
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.write_u64(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        let mut tail = [0u8; 8];
        tail[..rest.len()].copy_from_slice(rest);
        self.write_u64(u64::from_le_bytes(tail) ^ ((bytes.len() as u64) << 56));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// All 823,543 res-10 children of one res-3 cell, in index order. With
    /// FxBuildHasher, inserting and removing ~600k such cells took seconds.
    fn res10_cells() -> Vec<u64> {
        h3o::CellIndex::try_from(0x8a2a1072b59ffffu64)
            .unwrap()
            .parent(h3o::Resolution::Three)
            .unwrap()
            .children(h3o::Resolution::Ten)
            .map(u64::from)
            .collect()
    }

    #[test]
    fn single_u64_hashes_to_finalizer() {
        let h = H3BuildHasher.hash_one(0x8a2a1072b59ffffu64);
        assert_eq!(h, mix_h3(0x8a2a1072b59ffff));
    }

    #[test]
    fn res10_insert_remove_is_fast() {
        let cells = res10_cells();
        assert!(cells.len() > 500_000, "{}", cells.len());
        let start = Instant::now();
        let mut map: HashMap<u64, u64, H3BuildHasher> = HashMap::default();
        for &c in &cells {
            map.insert(c, c);
        }
        for &c in &cells {
            assert!(map.remove(&c).is_some());
        }
        assert!(map.is_empty());
        let elapsed = start.elapsed();
        // ~25ms in release; FxBuildHasher took seconds. The bound is loose
        // enough for debug builds and loaded CI hosts.
        assert!(
            elapsed < Duration::from_secs(if cfg!(debug_assertions) { 10 } else { 1 }),
            "{} res-10 cells took {elapsed:?}",
            cells.len()
        );
    }
}
