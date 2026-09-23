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
        std::mem::size_of::<Self>() + self.heap_bytes()
    }
}

pub(crate) fn table_bytes<K, V>(capacity: usize) -> usize {
    // Conservative allowance for bucket slack, control bytes and alignment.
    if capacity == 0 {
        0
    } else {
        (capacity + 1).saturating_mul(2 * (std::mem::size_of::<(K, V)>() + 1)) + 64
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
fn count(input: &mut impl Read, limit: usize) -> io::Result<usize> {
    let n = usize::try_from(read_u64(input)?).map_err(|_| invalid("spill length overflow"))?;
    if n > limit / 64 {
        return Err(invalid("spill accumulator exceeds aggregation budget"));
    }
    Ok(n)
}

impl SpillAccumulator for H3Accumulator {
    fn write_state(&self, out: &mut impl Write) -> io::Result<()> {
        for v in [self.sum, self.count, self.min, self.max, self.m2] {
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
    fn read_state(input: &mut impl Read, limit: usize) -> io::Result<Self> {
        let mut acc = Self::from_stats(
            read_f64(input)?,
            read_f64(input)?,
            read_f64(input)?,
            read_f64(input)?,
            read_f64(input)?,
        );
        match read_u64(input)? {
            0 => {}
            1 => {
                let mut q = QuantileSketch::new();
                q.zero_count = read_f64(input)?;
                q.total_count = read_f64(input)?;
                for bins in [&mut q.pos_bins, &mut q.neg_bins] {
                    let n = count(input, limit)?;
                    for _ in 0..n {
                        let mut k = [0; 4];
                        input.read_exact(&mut k)?;
                        bins.insert(i32::from_le_bytes(k), read_f64(input)?);
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
    fn read_state(input: &mut impl Read, limit: usize) -> io::Result<Self> {
        let total = read_f64(input)?;
        let n = count(input, limit)?;
        let mut acc = Self::new();
        for _ in 0..n {
            let mut k = [0; 8];
            input.read_exact(&mut k)?;
            acc.update_weighted(i64::from_le_bytes(k), read_f64(input)?);
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
    _acc: PhantomData<A>,
}
impl<A: SpillAccumulator> SpillRuns<A> {
    pub fn new(limit: usize, directory: Option<&Path>) -> Self {
        Self {
            levels: Vec::new(),
            limit,
            directory: directory.map(Path::to_owned),
            runs_written: 0,
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
    fn merge(&self, left: Run, right: Run) -> io::Result<Run> {
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
        Ok(Run {
            file: out.into_inner().map_err(|e| e.into_error())?,
            len,
        })
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
