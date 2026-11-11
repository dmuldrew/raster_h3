//! Private, lossless accumulator runs. Two-way merges bound open readers and memory;
//! binary-carry levels avoid rewriting the entire history on every spill.
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::Path;

use super::sharded_map::AccumulatorMerge;
use crate::aggregator::{
    accumulator::H3Accumulator, categorical::CategoricalAccumulator, quantiles::QuantileSketch,
};

pub trait SpillAccumulator: AccumulatorMerge {
    fn write_state(&self, out: &mut impl Write) -> io::Result<()>;
    fn read_state(input: &mut impl Read, limit: usize) -> io::Result<Self>;
    fn memory_bytes(&self) -> usize {
        size_of::<Self>() + self.heap_bytes()
    }
}

pub(crate) fn table_bytes<K, V>(capacity: usize) -> usize {
    // Conservative allowance for bucket slack, control bytes and alignment.
    if capacity == 0 {
        0
    } else {
        (capacity + 1).saturating_mul(2 * (size_of::<(K, V)>() + 1)) + 64
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn write_u64(out: &mut impl Write, v: u64) -> io::Result<()> {
    out.write_all(&v.to_le_bytes())
}
fn read_u64(input: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
fn write_f64(out: &mut impl Write, v: f64) -> io::Result<()> {
    write_u64(out, v.to_bits())
}
fn read_f64(input: &mut impl Read) -> io::Result<f64> {
    Ok(f64::from_bits(read_u64(input)?))
}
/// A total or count: never NaN, negative or infinite (sums of finite positive weights).
fn read_total(input: &mut impl Read, what: &str) -> io::Result<f64> {
    let v = read_f64(input)?;
    if v.is_finite() && v >= 0.0 {
        Ok(v)
    } else {
        Err(invalid(&format!("invalid {what} {v} in accumulator state")))
    }
}
/// One bucket's or class's weight: always finite and positive (empty entries are not kept).
fn read_weight(input: &mut impl Read, what: &str) -> io::Result<f64> {
    let v = read_f64(input)?;
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(invalid(&format!(
            "invalid {what} weight {v} in accumulator state"
        )))
    }
}
fn count(input: &mut impl Read, limit: usize) -> io::Result<usize> {
    let n = usize::try_from(read_u64(input)?).map_err(|_| invalid("spill length overflow"))?;
    if n > limit / 64 {
        return Err(invalid("spill accumulator exceeds aggregation budget"));
    }
    Ok(n)
}

impl SpillAccumulator for H3Accumulator {
    fn write_state(&self, out: &mut impl Write) -> io::Result<()> {
        for v in [
            self.sum,
            self.count,
            self.sum_w2,
            self.min,
            self.max,
            self.m2,
        ] {
            write_f64(out, v)?;
        }
        write_u64(out, u64::from(self.quantiles.is_some()))?;
        if let Some(q) = &self.quantiles {
            write_f64(out, q.zero_count)?;
            write_f64(out, q.total_count)?;
            for bins in [&q.pos_bins, &q.neg_bins] {
                write_u64(out, bins.len() as u64)?;
                for (&k, &v) in bins {
                    out.write_all(&k.to_le_bytes())?;
                    write_f64(out, v)?;
                }
            }
        }
        Ok(())
    }
    /// Rejects states the accumulator cannot reach: a negative, NaN or infinite count or
    /// `sum_w2`, a negative `m2`, or `min > max` (or a NaN bound) once anything is counted.
    /// `sum`, a NaN `m2` and infinite bounds stay allowed: extreme inputs can overflow them.
    fn read_state(input: &mut impl Read, limit: usize) -> io::Result<Self> {
        let sum = read_f64(input)?;
        let count = read_total(input, "count")?;
        let sum_w2 = read_total(input, "sum of squared weights")?;
        let (min, max, m2) = (read_f64(input)?, read_f64(input)?, read_f64(input)?);
        if m2 < 0.0 {
            return Err(invalid("negative m2 in accumulator state"));
        }
        if count > 0.0 && !(min <= max) {
            return Err(invalid("accumulator state has min > max"));
        }
        let mut acc = Self::from_weighted_stats(sum, count, sum_w2, min, max, m2);
        match read_u64(input)? {
            0 => {}
            1 => {
                let mut q = QuantileSketch::new();
                q.zero_count = read_total(input, "quantile zero count")?;
                q.total_count = read_total(input, "quantile total count")?;
                for bins in [&mut q.pos_bins, &mut q.neg_bins] {
                    let n = self::count(input, limit)?;
                    for _ in 0..n {
                        let mut k = [0; 4];
                        input.read_exact(&mut k)?;
                        let key = i32::from_le_bytes(k);
                        if bins
                            .insert(key, read_weight(input, "quantile bucket")?)
                            .is_some()
                        {
                            return Err(invalid("duplicate quantile bucket in accumulator state"));
                        }
                    }
                }
                acc.quantiles = Some(Box::new(q));
            }
            _ => return Err(invalid("invalid quantile marker in spill")),
        }
        if acc.memory_bytes() > limit {
            return Err(invalid("quantile accumulator exceeds aggregation budget"));
        }
        Ok(acc)
    }
}

impl SpillAccumulator for CategoricalAccumulator {
    fn write_state(&self, out: &mut impl Write) -> io::Result<()> {
        write_f64(out, self.total_count)?;
        let n = self
            .heap_counts
            .as_ref()
            .map_or(self.inline_len as usize, |h| h.len());
        write_u64(out, n as u64)?;
        // Do not round-trip through JSON: it cannot preserve arbitrary f64 states.
        let mut result = Ok(());
        self.for_each_class(|k, v| {
            if result.is_ok() {
                result = out
                    .write_all(&k.to_le_bytes())
                    .and_then(|_| write_f64(out, v));
            }
        });
        result
    }
    /// Rejects a NaN, negative or infinite total, a class weight that is not finite and
    /// positive, and a class listed twice.
    fn read_state(input: &mut impl Read, limit: usize) -> io::Result<Self> {
        let total = read_total(input, "total count")?;
        let n = count(input, limit)?;
        let mut acc = Self::new();
        for _ in 0..n {
            let mut k = [0; 8];
            input.read_exact(&mut k)?;
            let class = i64::from_le_bytes(k);
            let weight = read_weight(input, "class")?;
            if acc.get_class_count(class) != 0.0 {
                return Err(invalid("duplicate class in accumulator state"));
            }
            acc.update_weighted(class, weight);
        }
        acc.total_count = total;
        if acc.memory_bytes() > limit {
            return Err(invalid(
                "categorical accumulator exceeds aggregation budget",
            ));
        }
        Ok(acc)
    }
}

struct Run {
    file: File,
    len: u64,
}

pub struct RunReader<A> {
    input: BufReader<File>,
    remaining: u64,
    limit: usize,
    _acc: PhantomData<A>,
}
impl<A: SpillAccumulator> RunReader<A> {
    fn new(mut run: Run, limit: usize) -> io::Result<Self> {
        run.file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            input: BufReader::with_capacity(4096, run.file),
            remaining: run.len,
            limit,
            _acc: PhantomData,
        })
    }
    pub fn next_record(&mut self) -> io::Result<Option<(u64, A)>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        // Truncation is an error, never successful EOF.
        let key = read_u64(&mut self.input)?;
        let value = A::read_state(&mut self.input, self.limit)?;
        self.remaining -= 1;
        Ok(Some((key, value)))
    }
}

pub struct SpillRuns<A> {
    levels: Vec<Option<Run>>,
    limit: usize,
    directory: Option<std::path::PathBuf>,
    pub runs_written: u64,
    pub bytes_written: u64,
    _acc: PhantomData<A>,
}
impl<A: SpillAccumulator> SpillRuns<A> {
    pub fn new(limit: usize, directory: Option<&Path>) -> Self {
        Self {
            levels: Vec::new(),
            limit,
            directory: directory.map(Path::to_owned),
            runs_written: 0,
            bytes_written: 0,
            _acc: PhantomData,
        }
    }
    pub fn has_spilled(&self) -> bool {
        self.runs_written != 0
    }
    fn file(&self) -> io::Result<File> {
        match &self.directory {
            Some(dir) => tempfile::tempfile_in(dir),
            None => tempfile::tempfile(),
        }
    }
    pub fn push_sorted(&mut self, records: impl IntoIterator<Item = (u64, A)>) -> io::Result<()> {
        let mut out = BufWriter::with_capacity(4096, self.file()?);
        let mut len = 0;
        let mut previous = None;
        for (key, acc) in records {
            if previous.is_some_and(|p| p >= key) {
                return Err(invalid("spill input is not strictly sorted"));
            }
            if acc.memory_bytes() > self.limit {
                return Err(invalid("single accumulator exceeds aggregation budget"));
            }
            write_u64(&mut out, key)?;
            acc.write_state(&mut out)?;
            previous = Some(key);
            len += 1;
        }
        out.flush()?;
        if len == 0 {
            return Ok(());
        }
        let file = out.into_inner().map_err(|e| e.into_error())?;
        self.bytes_written += file.metadata()?.len();
        let mut run = Run { file, len };
        self.runs_written += 1;
        let mut level = 0;
        loop {
            if level == 64 {
                return Err(invalid("spill run counter exhausted"));
            }
            if level == self.levels.len() {
                self.levels.push(None);
            }
            if let Some(other) = self.levels[level].take() {
                run = self.merge(other, run)?;
                level += 1;
            } else {
                self.levels[level] = Some(run);
                break;
            }
        }
        Ok(())
    }
    fn merge(&mut self, left: Run, right: Run) -> io::Result<Run> {
        let mut left = RunReader::<A>::new(left, self.limit)?;
        let mut right = RunReader::<A>::new(right, self.limit)?;
        let mut a = left.next_record()?;
        let mut b = right.next_record()?;
        let mut out = BufWriter::with_capacity(4096, self.file()?);
        let mut len = 0;
        while a.is_some() || b.is_some() {
            let (key, acc) = if b.is_none()
                || a.as_ref().zip(b.as_ref()).is_some_and(|(a, b)| a.0 < b.0)
            {
                let v = a.take().unwrap();
                a = left.next_record()?;
                v
            } else if a.is_none() || a.as_ref().zip(b.as_ref()).is_some_and(|(a, b)| a.0 > b.0) {
                let v = b.take().unwrap();
                b = right.next_record()?;
                v
            } else {
                let (key, mut acc) = a.take().unwrap();
                acc.merge(&b.take().unwrap().1);
                if acc.memory_bytes() > self.limit {
                    return Err(invalid("merged accumulator exceeds aggregation budget"));
                }
                a = left.next_record()?;
                b = right.next_record()?;
                (key, acc)
            };
            write_u64(&mut out, key)?;
            acc.write_state(&mut out)?;
            len += 1;
        }
        out.flush()?;
        let file = out.into_inner().map_err(|e| e.into_error())?;
        self.bytes_written += file.metadata()?.len();
        Ok(Run { file, len })
    }
    pub fn finish(&mut self) -> io::Result<Option<RunReader<A>>> {
        let mut combined = None;
        for i in 0..self.levels.len() {
            if let Some(run) = self.levels[i].take() {
                combined = Some(match combined {
                    Some(other) => self.merge(run, other)?,
                    None => run,
                });
            }
        }
        combined.map(|r| RunReader::new(r, self.limit)).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn binary_codec_preserves_quantile_state_and_nonfinite_statistics() {
        let mut a = H3Accumulator::with_quantiles();
        for value in [-100.0, -1.0, 0.0, 0.1, 12.0, 1000.0] {
            a.update_weighted(value, 0.25);
        }
        let mut bytes = Vec::new();
        a.write_state(&mut bytes).unwrap();
        let b = H3Accumulator::read_state(&mut Cursor::new(bytes), 65536).unwrap();
        assert_eq!(a, b);
        let a = H3Accumulator::default();
        let mut bytes = Vec::new();
        a.write_state(&mut bytes).unwrap();
        assert_eq!(
            a,
            H3Accumulator::read_state(&mut Cursor::new(bytes), 65536).unwrap()
        );
    }

    /// A continuous state as `write_state` lays it out.
    fn continuous_bytes(stats: [f64; 6], sketch: Option<(f64, f64, &[(i32, f64)])>) -> Vec<u8> {
        let mut out = Vec::new();
        for v in stats {
            write_f64(&mut out, v).unwrap();
        }
        write_u64(&mut out, u64::from(sketch.is_some())).unwrap();
        if let Some((zero, total, bins)) = sketch {
            write_f64(&mut out, zero).unwrap();
            write_f64(&mut out, total).unwrap();
            write_u64(&mut out, bins.len() as u64).unwrap();
            for &(k, w) in bins {
                out.extend_from_slice(&k.to_le_bytes());
                write_f64(&mut out, w).unwrap();
            }
            write_u64(&mut out, 0).unwrap();
        }
        out
    }

    /// A categorical state as `write_state` lays it out.
    fn categorical_bytes(total: f64, classes: &[(i64, f64)]) -> Vec<u8> {
        let mut out = Vec::new();
        write_f64(&mut out, total).unwrap();
        write_u64(&mut out, classes.len() as u64).unwrap();
        for &(k, w) in classes {
            out.extend_from_slice(&k.to_le_bytes());
            write_f64(&mut out, w).unwrap();
        }
        out
    }

    #[test]
    fn decoder_accepts_states_the_accumulator_can_reach() {
        let read = |b: Vec<u8>| H3Accumulator::read_state(&mut Cursor::new(b), 65536);
        let (nan, inf) = (f64::NAN, f64::INFINITY);
        // [sum, count, sum_w2, min, max, m2]
        // Empty: min and max keep their infinite sentinels.
        read(continuous_bytes([0.0, 0.0, 0.0, inf, -inf, 0.0], None)).unwrap();
        // Extreme inputs can overflow the sum, and then turn m2 into NaN.
        read(continuous_bytes([inf, 2.0, 2.0, 1e308, 1e308, nan], None)).unwrap();
        read(continuous_bytes(
            [3.0, 2.0, 2.0, 1.0, 2.0, 0.5],
            Some((0.0, 2.0, &[(5, 2.0)])),
        ))
        .unwrap();
        CategoricalAccumulator::read_state(
            &mut Cursor::new(categorical_bytes(3.0, &[(1, 2.0), (-7, 1.0)])),
            65536,
        )
        .unwrap();
    }

    #[test]
    fn decoder_rejects_states_the_accumulator_cannot_reach() {
        let (nan, inf) = (f64::NAN, f64::INFINITY);
        let ok = [3.0, 2.0, 2.0, 1.0, 2.0, 0.5];
        let with = |i: usize, v: f64| {
            let mut stats = ok;
            stats[i] = v;
            stats
        };
        let continuous: Vec<(&str, Vec<u8>)> = vec![
            ("NaN count", continuous_bytes(with(1, nan), None)),
            ("negative count", continuous_bytes(with(1, -1.0), None)),
            ("infinite count", continuous_bytes(with(1, inf), None)),
            ("NaN sum_w2", continuous_bytes(with(2, nan), None)),
            ("min > max", continuous_bytes(with(3, 5.0), None)),
            ("NaN min", continuous_bytes(with(3, nan), None)),
            ("negative m2", continuous_bytes(with(5, -1e-9), None)),
            (
                "NaN zero count",
                continuous_bytes(ok, Some((nan, 2.0, &[]))),
            ),
            (
                "negative total",
                continuous_bytes(ok, Some((0.0, -2.0, &[]))),
            ),
            (
                "zero bucket",
                continuous_bytes(ok, Some((0.0, 2.0, &[(5, 0.0)]))),
            ),
            (
                "NaN bucket",
                continuous_bytes(ok, Some((0.0, 2.0, &[(5, nan)]))),
            ),
            (
                "infinite bucket",
                continuous_bytes(ok, Some((0.0, 2.0, &[(5, inf)]))),
            ),
            (
                "duplicate bucket",
                continuous_bytes(ok, Some((0.0, 2.0, &[(5, 1.0), (5, 1.0)]))),
            ),
        ];
        for (case, bytes) in continuous {
            let err = H3Accumulator::read_state(&mut Cursor::new(bytes), 65536).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{case}: {err}");
        }

        let categorical = [
            ("NaN total", categorical_bytes(nan, &[(1, 1.0)])),
            ("infinite total", categorical_bytes(inf, &[(1, 1.0)])),
            ("NaN class weight", categorical_bytes(1.0, &[(1, nan)])),
            ("zero class weight", categorical_bytes(1.0, &[(1, 0.0)])),
            ("infinite class weight", categorical_bytes(1.0, &[(1, inf)])),
            (
                "duplicate class",
                categorical_bytes(2.0, &[(1, 1.0), (1, 1.0)]),
            ),
        ];
        for (case, bytes) in categorical {
            let err =
                CategoricalAccumulator::read_state(&mut Cursor::new(bytes), 65536).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{case}: {err}");
        }
    }

    #[test]
    fn corrupt_partial_states_are_rejected_on_decode() {
        use super::super::partial::{decode_partial_state, encode_partial_state};
        let mut acc = H3Accumulator::default();
        acc.update(4.0);
        let mut bytes = encode_partial_state(&acc);
        decode_partial_state::<H3Accumulator>(&bytes, 65536).unwrap();
        // The count follows the 4-byte magic, the version, the kind and the 8-byte sum.
        bytes[14..22].copy_from_slice(&f64::NAN.to_bits().to_le_bytes());
        let err = decode_partial_state::<H3Accumulator>(&bytes, 65536).unwrap_err();
        assert!(err.to_string().contains("invalid count"), "{err}");
    }

    #[test]
    fn binary_codec_preserves_categorical_heap() {
        let mut a = CategoricalAccumulator::new();
        for k in -30..30 {
            a.update_weighted(k, 0.25);
        }
        let mut bytes = Vec::new();
        a.write_state(&mut bytes).unwrap();
        let b = CategoricalAccumulator::read_state(&mut Cursor::new(bytes), 65536).unwrap();
        assert_eq!(a.total_count, b.total_count);
        a.for_each_class(|k, v| assert_eq!(v, b.get_class_count(k)));
    }

    #[test]
    fn repeated_runs_merge_late_contributions_once_and_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut runs = SpillRuns::<H3Accumulator>::new(65536, Some(dir.path()));
            for n in 1..=37 {
                runs.push_sorted((0..100).map(|k| {
                    let mut acc = H3Accumulator::with_quantiles();
                    acc.update(n as f64);
                    (k, acc)
                }))
                .unwrap();
            }
            assert_eq!(runs.runs_written, 37);
            assert!(runs.levels.len() <= 6);
            let mut reader = runs.finish().unwrap().unwrap();
            for k in 0..100 {
                let (key, acc) = reader.next_record().unwrap().unwrap();
                assert_eq!(key, k);
                assert_eq!(acc.count, 37.0);
                assert_eq!(acc.sum, 703.0);
                assert_eq!(acc.quantiles.unwrap().count(), 37.0);
            }
            assert!(reader.next_record().unwrap().is_none());
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn truncated_runs_are_errors_not_eof() {
        let mut runs = SpillRuns::<H3Accumulator>::new(65536, None);
        runs.push_sorted([(1, H3Accumulator::new(2.0))]).unwrap();
        runs.levels[0].as_ref().unwrap().file.set_len(9).unwrap();
        let mut reader = runs.finish().unwrap().unwrap();
        assert!(reader.next_record().is_err());
    }

    #[test]
    fn oversized_merged_state_fails_without_discarding_categories() {
        let mut runs = SpillRuns::<CategoricalAccumulator>::new(2048, None);
        for offset in [0, 100] {
            let mut acc = CategoricalAccumulator::new();
            for k in offset..offset + 16 {
                acc.update(k);
            }
            let result = runs.push_sorted([(1, acc)]);
            if offset == 0 {
                result.unwrap();
            } else {
                assert!(result.is_err());
            }
        }
    }
}
