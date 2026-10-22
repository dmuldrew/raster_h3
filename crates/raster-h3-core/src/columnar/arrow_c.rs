//! Official Apache Arrow C Data Interface (`ArrowSchema`, `ArrowArray`) and
//! Apache Arrow C Stream Interface (`ArrowArrayStream`) implementation.
//!
//! Conforms to:
//! - <https://arrow.apache.org/docs/format/CDataInterface.html>
//! - <https://arrow.apache.org/docs/format/CStreamInterface.html>
//!
//! Enables zero-copy and ABI-stable columnar data exchange across Polars, DataFusion,
//! PyArrow (`__arrow_c_stream__` / `__arrow_c_schema__` PyCapsules), DuckDB, and C/C++ consumers.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::Arc;

use crate::columnar::{ArrowDataType, ColumnArray, ColumnField, ColumnSchema, RecordBatch};
use crate::error::{RasterH3Error, Result};

/// Arrow C Data Interface flag indicating the field is nullable (`ARROW_FLAG_NULLABLE = 2`).
pub const ARROW_FLAG_NULLABLE: i64 = 2;

/// Official Apache Arrow C Data Interface `ArrowSchema` struct (`#[repr(C)]`).
#[repr(C)]
pub struct ArrowSchema {
    pub format: *const c_char,
    pub name: *const c_char,
    pub metadata: *const c_char,
    pub flags: i64,
    pub n_children: i64,
    pub children: *mut *mut ArrowSchema,
    pub dictionary: *mut ArrowSchema,
    pub release: Option<unsafe extern "C" fn(schema: *mut ArrowSchema)>,
    pub private_data: *mut c_void,
}

// SAFETY: every pointer in an `ArrowSchema` we produce points into its own
// heap-allocated `SchemaPrivateData` (or boxed children), none of which is
// thread-affine, and the Arrow C Data Interface lets consumers move a schema to
// another thread and call `release` there.
unsafe impl Send for ArrowSchema {}

struct SchemaPrivateData {
    _format: CString,
    _name: CString,
    children_vec: Vec<*mut ArrowSchema>,
}

/// # Safety
/// `schema` must be null or point to a live `ArrowSchema` built by
/// [`ArrowSchema::from_field`] / [`ArrowSchema::from_column_schema`].
unsafe extern "C" fn release_arrow_schema(schema: *mut ArrowSchema) {
    if schema.is_null() {
        return;
    }
    // SAFETY: non-null and, per this callback's contract, a live `ArrowSchema`
    // that nobody else is accessing while it is being released.
    let s = unsafe { &mut *schema };
    if s.release.is_none() {
        return;
    }
    if !s.private_data.is_null() {
        // SAFETY: `release` is still set, so `private_data` is the pointer we
        // produced with `Box::into_raw(Box<SchemaPrivateData>)` and has not been
        // freed yet; it is nulled below so it can't be freed twice.
        let mut priv_data = unsafe { Box::from_raw(s.private_data as *mut SchemaPrivateData) };
        for child_ptr in priv_data.children_vec.drain(..) {
            if !child_ptr.is_null() {
                // SAFETY: each child came from `Box::into_raw(Box<ArrowSchema>)` in
                // `from_column_schema` and is owned solely by this parent, so it is
                // live here and is freed exactly once by the `Box::from_raw` below.
                unsafe {
                    if let Some(rel) = (*child_ptr).release {
                        rel(child_ptr);
                    }
                    drop(Box::from_raw(child_ptr));
                }
            }
        }
        s.private_data = ptr::null_mut();
    }
    s.release = None;
}

impl ArrowSchema {
    /// Create an empty/released `ArrowSchema`.
    pub const fn empty() -> Self {
        Self {
            format: ptr::null(),
            name: ptr::null(),
            metadata: ptr::null(),
            flags: 0,
            n_children: 0,
            children: ptr::null_mut(),
            dictionary: ptr::null_mut(),
            release: None,
            private_data: ptr::null_mut(),
        }
    }

    /// Construct a leaf `ArrowSchema` for a single [`ColumnField`].
    pub fn from_field(field: &ColumnField) -> Self {
        let format_c = CString::new(field.data_type.arrow_format_str()).unwrap_or_default();
        let name_c = CString::new(field.name.as_str()).unwrap_or_default();
        let flags = if field.nullable {
            ARROW_FLAG_NULLABLE
        } else {
            0
        };
        let priv_box = Box::new(SchemaPrivateData {
            _format: format_c,
            _name: name_c,
            children_vec: Vec::new(),
        });
        let format_ptr = priv_box._format.as_ptr();
        let name_ptr = priv_box._name.as_ptr();
        Self {
            format: format_ptr,
            name: name_ptr,
            metadata: ptr::null(),
            flags,
            n_children: 0,
            children: ptr::null_mut(),
            dictionary: ptr::null_mut(),
            release: Some(release_arrow_schema),
            private_data: Box::into_raw(priv_box) as *mut c_void,
        }
    }

    /// Construct a top-level struct `ArrowSchema` (`"+s"`) representing a [`ColumnSchema`].
    pub fn from_column_schema(schema: &ColumnSchema) -> Self {
        let format_c = CString::new("+s").unwrap();
        let name_c = CString::new("").unwrap();
        let mut children_vec: Vec<*mut ArrowSchema> = schema
            .fields
            .iter()
            .map(|f| Box::into_raw(Box::new(Self::from_field(f))))
            .collect();
        let n_children = children_vec.len() as i64;
        let children_ptr = if children_vec.is_empty() {
            ptr::null_mut()
        } else {
            children_vec.as_mut_ptr()
        };
        let priv_box = Box::new(SchemaPrivateData {
            _format: format_c,
            _name: name_c,
            children_vec,
        });
        let format_ptr = priv_box._format.as_ptr();
        let name_ptr = priv_box._name.as_ptr();
        Self {
            format: format_ptr,
            name: name_ptr,
            metadata: ptr::null(),
            flags: 0,
            n_children,
            children: children_ptr,
            dictionary: ptr::null_mut(),
            release: Some(release_arrow_schema),
            private_data: Box::into_raw(priv_box) as *mut c_void,
        }
    }

    /// Parse a top-level struct `ArrowSchema` back into a [`ColumnSchema`].
    ///
    /// # Safety
    /// `self` must be a valid, unreleased `ArrowSchema` struct.
    pub unsafe fn to_column_schema(&self) -> Result<ColumnSchema> {
        if self.release.is_none() || self.format.is_null() {
            return Err(RasterH3Error::InvalidParameter(
                "ArrowSchema is released or null".into(),
            ));
        }
        // SAFETY: `format` was checked non-null above and, for a valid unreleased
        // schema (caller contract), is a NUL-terminated string owned by the schema.
        let fmt = unsafe { CStr::from_ptr(self.format) }.to_string_lossy();
        if fmt != "+s" {
            return Err(RasterH3Error::InvalidParameter(format!(
                "Expected top-level struct format '+s', got '{}'",
                fmt
            )));
        }
        let mut fields = Vec::with_capacity(self.n_children.max(0) as usize);
        if self.n_children > 0 && self.children.is_null() {
            return Err(RasterH3Error::InvalidParameter(
                "ArrowSchema has children but a null children array".into(),
            ));
        }
        for i in 0..(self.n_children.max(0) as usize) {
            // SAFETY: `children` is non-null (checked above) and, for a valid
            // schema, points to `n_children` child pointers; `i < n_children`.
            let child_ptr = unsafe { *self.children.add(i) };
            if child_ptr.is_null() {
                return Err(RasterH3Error::InvalidParameter(
                    "Null child pointer in ArrowSchema".into(),
                ));
            }
            // SAFETY: non-null child of a valid schema, owned by (and outliving the
            // borrow of) `self`.
            let child = unsafe { &*child_ptr };
            let name = if child.name.is_null() {
                String::new()
            } else {
                // SAFETY: non-null `name` of a valid schema is NUL-terminated.
                unsafe { CStr::from_ptr(child.name) }
                    .to_string_lossy()
                    .into_owned()
            };
            if child.format.is_null() {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Null format string for ArrowSchema child '{name}'"
                )));
            }
            // SAFETY: checked non-null just above; a valid schema's format is a
            // NUL-terminated string.
            let c_fmt = unsafe { CStr::from_ptr(child.format) }.to_string_lossy();
            let dt = match c_fmt.as_ref() {
                "L" => ArrowDataType::UInt64,
                "l" => ArrowDataType::Int64,
                "C" => ArrowDataType::UInt8,
                "g" => ArrowDataType::Float64,
                "b" => ArrowDataType::Boolean,
                "u" => ArrowDataType::Utf8,
                "z" => ArrowDataType::Binary,
                other => {
                    return Err(RasterH3Error::InvalidParameter(format!(
                        "Unsupported Arrow format '{}' for column '{}'",
                        other, name
                    )))
                }
            };
            let nullable = (child.flags & ARROW_FLAG_NULLABLE) != 0;
            fields.push(ColumnField::new(name, dt, nullable));
        }
        Ok(ColumnSchema::new(fields))
    }
}

impl Drop for ArrowSchema {
    fn drop(&mut self) {
        if let Some(rel) = self.release {
            // SAFETY: a set `release` means `self` is a live, unreleased schema and
            // `rel` is the producer's callback for it; we own it, so this is the
            // single release the C Data Interface requires.
            unsafe { rel(self) };
        }
    }
}

/// Official Apache Arrow C Data Interface `ArrowArray` struct (`#[repr(C)]`).
#[repr(C)]
pub struct ArrowArray {
    pub length: i64,
    pub null_count: i64,
    pub offset: i64,
    pub n_buffers: i64,
    pub n_children: i64,
    pub buffers: *mut *const c_void,
    pub children: *mut *mut ArrowArray,
    pub dictionary: *mut ArrowArray,
    pub release: Option<unsafe extern "C" fn(array: *mut ArrowArray)>,
    pub private_data: *mut c_void,
}

// SAFETY: an `ArrowArray` we produce owns its buffers through
// `ArrayPrivateData` (plain `Vec`s and boxed children, none thread-affine), and
// the C Data Interface allows moving an array across threads and releasing it
// from any of them. Imported arrays are only read, never mutated, through it.
unsafe impl Send for ArrowArray {}

#[allow(dead_code)]
enum OwnedArrowBuffer {
    U8(Vec<u8>),
    U64(Vec<u64>),
    I64(Vec<i64>),
    F64(Vec<f64>),
    I32(Vec<i32>),
}

struct ArrayPrivateData {
    _owned_buffers: Vec<OwnedArrowBuffer>,
    buffer_ptrs: Vec<*const c_void>,
    children_vec: Vec<*mut ArrowArray>,
}

/// # Safety
/// `array` must be null or point to a live `ArrowArray` built by
/// [`ArrowArray::from_column_array`] / [`ArrowArray::from_record_batch`].
unsafe extern "C" fn release_arrow_array(array: *mut ArrowArray) {
    if array.is_null() {
        return;
    }
    // SAFETY: non-null and, per this callback's contract, a live `ArrowArray`
    // that nobody else is accessing while it is being released.
    let a = unsafe { &mut *array };
    if a.release.is_none() {
        return;
    }
    if !a.private_data.is_null() {
        // SAFETY: `release` is still set, so `private_data` is the pointer we
        // produced with `Box::into_raw(Box<ArrayPrivateData>)` and has not been
        // freed yet; it is nulled below so it can't be freed twice.
        let mut priv_data = unsafe { Box::from_raw(a.private_data as *mut ArrayPrivateData) };
        for child_ptr in priv_data.children_vec.drain(..) {
            if !child_ptr.is_null() {
                // SAFETY: each child came from `Box::into_raw(Box<ArrowArray>)` in
                // `from_record_batch` and is owned solely by this parent, so it is
                // live here and is freed exactly once by the `Box::from_raw` below.
                unsafe {
                    if let Some(rel) = (*child_ptr).release {
                        rel(child_ptr);
                    }
                    drop(Box::from_raw(child_ptr));
                }
            }
        }
        a.private_data = ptr::null_mut();
    }
    a.release = None;
}

/// Pack a boolean slice into an LSB-first Arrow bitmap.
fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let n_bytes = bits.len().div_ceil(8);
    let mut out = vec![0u8; n_bytes];
    for (i, &b) in bits.iter().enumerate() {
        if b {
            out[i / 8] |= 1u8 << (i % 8);
        }
    }
    out
}

/// Pack variable-width values into Arrow `i32` offsets and a contiguous data buffer,
/// dropping each value once copied.
fn pack_var_width(
    values: impl Iterator<Item = Vec<u8>>,
    len: usize,
    total_bytes: usize,
) -> (Vec<i32>, Vec<u8>) {
    let mut offsets = Vec::with_capacity(len + 1);
    let mut data = Vec::with_capacity(total_bytes.max(1));
    offsets.push(0i32);
    for v in values {
        data.extend_from_slice(&v);
        offsets.push(data.len() as i32);
    }
    (offsets, data)
}

/// Unpack an LSB-first Arrow bitmap of length `len`.
///
/// # Safety
/// `ptr` must be null or valid for reads of `(offset + len).div_ceil(8)` bytes.
unsafe fn unpack_bits(ptr: *const u8, offset: usize, len: usize) -> Vec<bool> {
    if ptr.is_null() {
        return vec![true; len];
    }
    let n_bytes = (offset + len).div_ceil(8);
    // SAFETY: the caller guarantees `ptr` is readable for `n_bytes`; `u8` has no
    // alignment requirement and the bytes are not mutated while borrowed.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, n_bytes) };
    let mut out = Vec::with_capacity(len);
    for i in offset..offset + len {
        let bit = (bytes[i / 8] >> (i % 8)) & 1 != 0;
        out.push(bit);
    }
    out
}

/// Copy `len` fixed-width values starting at logical index `start`.
///
/// # Safety
/// `ptr` must be null or aligned for `T` and valid for reads of `start + len`
/// elements.
unsafe fn read_values<T: Copy>(ptr: *const T, start: usize, len: usize) -> Vec<T> {
    if len == 0 || ptr.is_null() {
        return Vec::new();
    }
    // SAFETY: the caller guarantees `ptr` is `T`-aligned (the C Data Interface
    // requires buffers to be aligned for their element type) and readable for
    // `start + len` elements, so `[start, start + len)` is in bounds.
    unsafe { std::slice::from_raw_parts(ptr.add(start), len).to_vec() }
}

/// Borrow `len` variable-length values (Utf8/Binary layout) starting at logical index `start`.
/// Offsets are absolute positions into the data buffer, so slicing only shifts the offsets window.
///
/// # Safety
/// `off_ptr` must be null or `i32`-aligned and valid for reads of `start + len + 1`
/// offsets; `data_ptr` must be valid for reads up to the largest offset read. Both
/// buffers must outlive `'a` and stay unmodified for it.
unsafe fn read_var_len<'a>(
    off_ptr: *const i32,
    data_ptr: *const u8,
    start: usize,
    len: usize,
) -> Vec<&'a [u8]> {
    if len == 0 || off_ptr.is_null() {
        return Vec::new();
    }
    // SAFETY: per the caller contract the offsets buffer is aligned and holds at
    // least `start + len + 1` entries (Arrow stores `length + 1` offsets).
    let offsets = unsafe { std::slice::from_raw_parts(off_ptr.add(start), len + 1) };
    let total_bytes = offsets[len].max(0) as usize;
    let bytes = if total_bytes == 0 || data_ptr.is_null() {
        &[][..]
    } else {
        // SAFETY: `offsets[len]` is the end of the last visible value, which the
        // caller guarantees is within the data buffer; it lives for `'a`.
        unsafe { std::slice::from_raw_parts(data_ptr, total_bytes) }
    };
    (0..len)
        .map(|r| &bytes[offsets[r].max(0) as usize..offsets[r + 1].max(0) as usize])
        .collect()
}

impl ArrowArray {
    /// Create an empty/released `ArrowArray` (used to signal end-of-stream in `ArrowArrayStream`).
    pub const fn empty() -> Self {
        Self {
            length: 0,
            null_count: 0,
            offset: 0,
            n_buffers: 0,
            n_children: 0,
            buffers: ptr::null_mut(),
            children: ptr::null_mut(),
            dictionary: ptr::null_mut(),
            release: None,
            private_data: ptr::null_mut(),
        }
    }

    /// Construct a leaf `ArrowArray` from a [`ColumnArray`], taking ownership of its buffers.
    /// Fixed-width columns are handed to the consumer without copying; variable-width columns
    /// are packed into offsets + data, freeing each source value as it is copied.
    pub fn from_column_array(col: ColumnArray) -> Self {
        let len = col.len();
        let null_count = col.null_count();
        let mut owned_buffers = Vec::with_capacity(3);
        let mut buffer_ptrs = Vec::with_capacity(3);

        // Buffer 0: Validity bitmap (NULL if null_count == 0 per Arrow C Data Interface spec)
        match col.validity() {
            Some(val) if null_count > 0 => {
                let bitmap = pack_bits(val);
                buffer_ptrs.push(bitmap.as_ptr() as *const c_void);
                owned_buffers.push(OwnedArrowBuffer::U8(bitmap));
            }
            _ => buffer_ptrs.push(ptr::null()),
        }

        // Remaining buffers depend on physical layout. Moving a Vec does not move its heap
        // allocation, so pointers taken before the move stay valid.
        match col {
            ColumnArray::UInt64 { values, .. } => {
                buffer_ptrs.push(values.as_ptr() as *const c_void);
                owned_buffers.push(OwnedArrowBuffer::U64(values));
            }
            ColumnArray::Int64 { values, .. } => {
                buffer_ptrs.push(values.as_ptr() as *const c_void);
                owned_buffers.push(OwnedArrowBuffer::I64(values));
            }
            ColumnArray::UInt8 { values, .. } => {
                buffer_ptrs.push(values.as_ptr() as *const c_void);
                owned_buffers.push(OwnedArrowBuffer::U8(values));
            }
            ColumnArray::Float64 { values, .. } => {
                buffer_ptrs.push(values.as_ptr() as *const c_void);
                owned_buffers.push(OwnedArrowBuffer::F64(values));
            }
            ColumnArray::Boolean { values, .. } => {
                let packed = pack_bits(&values);
                buffer_ptrs.push(packed.as_ptr() as *const c_void);
                owned_buffers.push(OwnedArrowBuffer::U8(packed));
            }
            ColumnArray::Utf8 { values, .. } => {
                let total_bytes: usize = values.iter().map(|s| s.len()).sum();
                let (offsets, data) =
                    pack_var_width(values.into_iter().map(String::into_bytes), len, total_bytes);
                buffer_ptrs.push(offsets.as_ptr() as *const c_void);
                buffer_ptrs.push(data.as_ptr() as *const c_void);
                owned_buffers.push(OwnedArrowBuffer::I32(offsets));
                owned_buffers.push(OwnedArrowBuffer::U8(data));
            }
            ColumnArray::Binary { values, .. } => {
                let total_bytes: usize = values.iter().map(|b| b.len()).sum();
                let (offsets, data) = pack_var_width(values.into_iter(), len, total_bytes);
                buffer_ptrs.push(offsets.as_ptr() as *const c_void);
                buffer_ptrs.push(data.as_ptr() as *const c_void);
                owned_buffers.push(OwnedArrowBuffer::I32(offsets));
                owned_buffers.push(OwnedArrowBuffer::U8(data));
            }
        }

        let n_buffers = buffer_ptrs.len() as i64;
        let mut priv_box = Box::new(ArrayPrivateData {
            _owned_buffers: owned_buffers,
            buffer_ptrs,
            children_vec: Vec::new(),
        });
        let buffers_ptr = priv_box.buffer_ptrs.as_mut_ptr();

        Self {
            length: len as i64,
            null_count: null_count as i64,
            offset: 0,
            n_buffers,
            n_children: 0,
            buffers: buffers_ptr,
            children: ptr::null_mut(),
            dictionary: ptr::null_mut(),
            release: Some(release_arrow_array),
            private_data: Box::into_raw(priv_box) as *mut c_void,
        }
    }

    /// Construct a top-level struct `ArrowArray` from a [`RecordBatch`], taking ownership
    /// of its column buffers.
    pub fn from_record_batch(batch: RecordBatch) -> Self {
        let num_rows = batch.num_rows;
        let mut children_vec: Vec<*mut ArrowArray> = batch
            .columns
            .into_iter()
            .map(|c| Box::into_raw(Box::new(Self::from_column_array(c))))
            .collect();
        let n_children = children_vec.len() as i64;
        let children_ptr = if children_vec.is_empty() {
            ptr::null_mut()
        } else {
            children_vec.as_mut_ptr()
        };

        // Struct arrays have 1 buffer: the null bitmap (NULL since top-level batch has 0 struct nulls)
        let buffer_ptrs = vec![ptr::null()];
        let mut priv_box = Box::new(ArrayPrivateData {
            _owned_buffers: Vec::new(),
            buffer_ptrs,
            children_vec,
        });
        let buffers_ptr = priv_box.buffer_ptrs.as_mut_ptr();

        Self {
            length: num_rows as i64,
            null_count: 0,
            offset: 0,
            n_buffers: 1,
            n_children,
            buffers: buffers_ptr,
            children: children_ptr,
            dictionary: ptr::null_mut(),
            release: Some(release_arrow_array),
            private_data: Box::into_raw(priv_box) as *mut c_void,
        }
    }

    /// Import an `ArrowArray` into a [`RecordBatch`] given its [`ColumnSchema`].
    ///
    /// # Safety
    /// `self` must be a valid, unreleased `ArrowArray` matching `schema`.
    pub unsafe fn to_record_batch(&self, schema: Arc<ColumnSchema>) -> Result<RecordBatch> {
        if self.release.is_none() {
            return Err(RasterH3Error::InvalidParameter(
                "ArrowArray is already released".into(),
            ));
        }
        if self.n_children as usize != schema.fields.len() {
            return Err(RasterH3Error::InvalidParameter(format!(
                "ArrowArray n_children ({}) != schema field count ({})",
                self.n_children,
                schema.fields.len()
            )));
        }
        let num_rows = self.length.max(0) as usize;
        let parent_offset = self.offset.max(0) as usize;
        let mut columns = Vec::with_capacity(schema.fields.len());

        if self.n_children > 0 && self.children.is_null() {
            return Err(RasterH3Error::InvalidParameter(
                "ArrowArray has children but a null children array".into(),
            ));
        }
        for (i, field) in schema.fields.iter().enumerate() {
            // SAFETY: `children` is non-null and holds `n_children` entries (valid
            // array, caller contract); `i < n_children` was checked above.
            let child_ptr = unsafe { *self.children.add(i) };
            if child_ptr.is_null() {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Null child pointer {i} in ArrowArray"
                )));
            }
            // SAFETY: non-null child of a valid array, owned by `self` and not
            // released while `self` is borrowed.
            let child = unsafe { &*child_ptr };
            let expected_buffers = match field.data_type {
                ArrowDataType::Utf8 | ArrowDataType::Binary => 3,
                _ => 2,
            };
            if child.n_buffers < expected_buffers || child.buffers.is_null() {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "ArrowArray child {i} has {} buffers, expected {expected_buffers}",
                    child.n_buffers
                )));
            }
            // A struct's offset applies to its children on top of their own offsets, and the
            // struct's length bounds how many child rows are visible.
            let start = child.offset.max(0) as usize + parent_offset;
            let len = num_rows;
            if (child.length.max(0) as usize) < parent_offset + len {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "ArrowArray child {i} length ({}) is shorter than parent offset + length ({})",
                    child.length,
                    parent_offset + len
                )));
            }
            // `null_count == -1` means the count was not computed, so the bitmap must be read.
            let validity = if child.null_count != 0 {
                // SAFETY: `buffers` is non-null with at least 2 entries (checked
                // above); entry 0 is the validity bitmap.
                let bmp_ptr = unsafe { *child.buffers } as *const u8;
                if bmp_ptr.is_null() {
                    None
                } else {
                    // SAFETY: a non-null validity bitmap covers the child's
                    // `offset + length` slots, and `start + len` is within that
                    // (length check above).
                    Some(unsafe { unpack_bits(bmp_ptr, start, len) })
                }
            } else {
                None
            };

            // SAFETY: for every buffer read below, `buffers` holds at least
            // `expected_buffers` entries (checked above) laid out per the Arrow
            // C Data Interface for `field.data_type`, i.e. buffer 1 holds values
            // (or offsets) aligned for their type and buffer 2 the var-width data,
            // each covering the child's `offset + length` slots, which bounds
            // `start + len`. The caller guarantees the array matches `schema` and
            // stays alive and unmodified for the duration of this call; values
            // are copied out before returning.
            let col = unsafe {
                match field.data_type {
                    ArrowDataType::UInt64 => ColumnArray::UInt64 {
                        values: read_values(*child.buffers.add(1) as *const u64, start, len),
                        validity,
                    },
                    ArrowDataType::Int64 => ColumnArray::Int64 {
                        values: read_values(*child.buffers.add(1) as *const i64, start, len),
                        validity,
                    },
                    ArrowDataType::UInt8 => ColumnArray::UInt8 {
                        values: read_values(*child.buffers.add(1) as *const u8, start, len),
                        validity,
                    },
                    ArrowDataType::Float64 => ColumnArray::Float64 {
                        values: read_values(*child.buffers.add(1) as *const f64, start, len),
                        validity,
                    },
                    ArrowDataType::Boolean => {
                        let data_ptr = *child.buffers.add(1) as *const u8;
                        let values = unpack_bits(data_ptr, start, len);
                        ColumnArray::Boolean { values, validity }
                    }
                    ArrowDataType::Utf8 => {
                        let values = read_var_len(
                            *child.buffers.add(1) as *const i32,
                            *child.buffers.add(2) as *const u8,
                            start,
                            len,
                        )
                        .into_iter()
                        .map(|b| std::str::from_utf8(b).unwrap_or_default().to_owned())
                        .collect();
                        ColumnArray::Utf8 { values, validity }
                    }
                    ArrowDataType::Binary => {
                        let values = read_var_len(
                            *child.buffers.add(1) as *const i32,
                            *child.buffers.add(2) as *const u8,
                            start,
                            len,
                        )
                        .into_iter()
                        .map(<[u8]>::to_vec)
                        .collect();
                        ColumnArray::Binary { values, validity }
                    }
                }
            };
            columns.push(col);
        }

        Ok(RecordBatch {
            schema,
            columns,
            num_rows,
        })
    }
}

impl Drop for ArrowArray {
    fn drop(&mut self) {
        if let Some(rel) = self.release {
            // SAFETY: a set `release` means `self` is a live, unreleased array and
            // `rel` is the producer's callback for it; we own it, so this is the
            // single release the C Data Interface requires.
            unsafe { rel(self) };
        }
    }
}

/// Official Apache Arrow C Stream Interface `ArrowArrayStream` struct (`#[repr(C)]`).
#[repr(C)]
pub struct ArrowArrayStream {
    pub get_schema:
        Option<unsafe extern "C" fn(stream: *mut ArrowArrayStream, out: *mut ArrowSchema) -> c_int>,
    pub get_next:
        Option<unsafe extern "C" fn(stream: *mut ArrowArrayStream, out: *mut ArrowArray) -> c_int>,
    pub get_last_error:
        Option<unsafe extern "C" fn(stream: *mut ArrowArrayStream) -> *const c_char>,
    pub release: Option<unsafe extern "C" fn(stream: *mut ArrowArrayStream)>,
    pub private_data: *mut c_void,
}

// SAFETY: `StreamPrivateData` holds an `Arc<ColumnSchema>`, a `Send` iterator
// and a `CString`, so the stream may be driven and released from another thread
// as long as calls are not concurrent, which the C Stream Interface already
// forbids.
unsafe impl Send for ArrowArrayStream {}

struct StreamPrivateData {
    schema: Arc<ColumnSchema>,
    iter: Box<dyn Iterator<Item = Result<RecordBatch>> + Send>,
    last_error: Option<CString>,
}

unsafe extern "C" fn stream_get_schema(
    stream: *mut ArrowArrayStream,
    out: *mut ArrowSchema,
) -> c_int {
    if stream.is_null() || out.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: non-null, and the C Stream Interface passes the stream this
    // callback was installed on, with no concurrent calls.
    let s = unsafe { &mut *stream };
    if s.private_data.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: a non-null `private_data` on a stream we built is the live
    // `Box<StreamPrivateData>` from `from_iterator` (freed only by release).
    let priv_data = unsafe { &*(s.private_data as *const StreamPrivateData) };
    // SAFETY: `out` is non-null consumer storage for one `ArrowSchema`; it is
    // treated as uninitialized, so `write` avoids dropping whatever was there.
    unsafe { ptr::write(out, ArrowSchema::from_column_schema(&priv_data.schema)) };
    0
}

unsafe extern "C" fn stream_get_next(stream: *mut ArrowArrayStream, out: *mut ArrowArray) -> c_int {
    if stream.is_null() || out.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: non-null, and the C Stream Interface passes the stream this
    // callback was installed on, with no concurrent calls.
    let s = unsafe { &mut *stream };
    if s.private_data.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: a non-null `private_data` on a stream we built is the live
    // `Box<StreamPrivateData>` from `from_iterator`; calls are not concurrent,
    // so this is the only reference to it.
    let priv_data = unsafe { &mut *(s.private_data as *mut StreamPrivateData) };
    let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| priv_data.iter.next()));
    match step {
        Ok(Some(Ok(batch))) => {
            // SAFETY: `out` is non-null consumer storage for one `ArrowArray`,
            // treated as uninitialized, so `write` avoids dropping it.
            unsafe { ptr::write(out, ArrowArray::from_record_batch(batch)) };
            0
        }
        Ok(None) => {
            // SAFETY: as above; a released (empty) array signals end of stream.
            unsafe { ptr::write(out, ArrowArray::empty()) };
            0
        }
        Ok(Some(Err(err))) => {
            priv_data.last_error = Some(CString::new(err.to_string()).unwrap_or_default());
            libc::EIO
        }
        Err(payload) => {
            let msg = crate::error::safe_panic_payload_to_string(payload);
            priv_data.last_error =
                Some(CString::new(format!("Arrow stream panicked: {msg}")).unwrap_or_default());
            libc::EIO
        }
    }
}

unsafe extern "C" fn stream_get_last_error(stream: *mut ArrowArrayStream) -> *const c_char {
    if stream.is_null() {
        return ptr::null();
    }
    // SAFETY: non-null, and the C Stream Interface passes the stream this
    // callback was installed on.
    let s = unsafe { &*stream };
    if s.private_data.is_null() {
        return ptr::null();
    }
    // SAFETY: a non-null `private_data` is our live `Box<StreamPrivateData>`. The
    // returned pointer borrows `last_error`, which the C Stream Interface only
    // requires to stay valid until the next call on the stream or its release.
    let priv_data = unsafe { &*(s.private_data as *const StreamPrivateData) };
    match &priv_data.last_error {
        Some(c) => c.as_ptr(),
        None => ptr::null(),
    }
}

unsafe extern "C" fn release_arrow_stream(stream: *mut ArrowArrayStream) {
    if stream.is_null() {
        return;
    }
    // SAFETY: non-null and the stream this callback was installed on; release is
    // the last call the consumer makes, so nothing else is accessing it.
    let s = unsafe { &mut *stream };
    if s.release.is_none() {
        return;
    }
    if !s.private_data.is_null() {
        // Dropping the iterator may join worker threads; never unwind into the consumer.
        // SAFETY: `release` is still set, so `private_data` is the unfreed
        // `Box<StreamPrivateData>` from `from_iterator`; it is nulled below.
        let priv_data = unsafe { Box::from_raw(s.private_data as *mut StreamPrivateData) };
        crate::error::catch_unwind_or((), || drop(priv_data));
        s.private_data = ptr::null_mut();
    }
    s.get_schema = None;
    s.get_next = None;
    s.get_last_error = None;
    s.release = None;
}

impl ArrowArrayStream {
    /// Wrap any `Iterator<Item = Result<RecordBatch>> + Send + 'static` into an `ArrowArrayStream`.
    pub fn from_iterator<I>(schema: Arc<ColumnSchema>, iter: I) -> Self
    where
        I: Iterator<Item = Result<RecordBatch>> + Send + 'static,
    {
        let priv_box = Box::new(StreamPrivateData {
            schema,
            iter: Box::new(iter),
            last_error: None,
        });
        Self {
            get_schema: Some(stream_get_schema),
            get_next: Some(stream_get_next),
            get_last_error: Some(stream_get_last_error),
            release: Some(release_arrow_stream),
            private_data: Box::into_raw(priv_box) as *mut c_void,
        }
    }

    /// Consume all batches from an `ArrowArrayStream` into a `(Arc<ColumnSchema>, Vec<RecordBatch>)`.
    ///
    /// # Safety
    /// `self` must be a valid, unreleased `ArrowArrayStream`.
    pub unsafe fn collect_batches(&mut self) -> Result<(Arc<ColumnSchema>, Vec<RecordBatch>)> {
        let get_schema_fn = self.get_schema.ok_or_else(|| {
            RasterH3Error::InvalidParameter("ArrowArrayStream is released".into())
        })?;
        let get_next_fn = self.get_next.ok_or_else(|| {
            RasterH3Error::InvalidParameter("ArrowArrayStream missing get_next".into())
        })?;

        let mut raw_schema = ArrowSchema::empty();
        // SAFETY: `self` is a valid unreleased stream (caller contract) and
        // `get_schema_fn` is its own callback; `raw_schema` is writable storage
        // whose (empty) contents need no release if overwritten.
        let rc = unsafe { get_schema_fn(self, &mut raw_schema) };
        if rc != 0 {
            return Err(RasterH3Error::InvalidParameter(format!(
                "ArrowArrayStream.get_schema failed with code {}",
                rc
            )));
        }
        // SAFETY: `get_schema` returned 0, so `raw_schema` is now a valid,
        // unreleased schema owned by us.
        let schema = Arc::new(unsafe { raw_schema.to_column_schema() }?);

        let mut batches = Vec::new();
        loop {
            let mut raw_array = ArrowArray::empty();
            // SAFETY: same stream/callback pairing as `get_schema` above;
            // `raw_array` is writable storage for one array.
            let rc = unsafe { get_next_fn(self, &mut raw_array) };
            if rc != 0 {
                let msg = if let Some(err_fn) = self.get_last_error {
                    // SAFETY: the stream's own callback, called on the stream.
                    let ptr = unsafe { err_fn(self) };
                    if !ptr.is_null() {
                        // SAFETY: a non-null last-error is a NUL-terminated string
                        // valid until the next call on the stream; we copy it now.
                        unsafe { CStr::from_ptr(ptr) }
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        format!("ArrowArrayStream error code {}", rc)
                    }
                } else {
                    format!("ArrowArrayStream error code {}", rc)
                };
                return Err(RasterH3Error::StreamFailed(msg));
            }
            if raw_array.release.is_none() {
                break;
            }
            // SAFETY: `get_next` succeeded with a non-released array, which the
            // producer guarantees matches the schema it reported.
            batches.push(unsafe { raw_array.to_record_batch(Arc::clone(&schema)) }?);
        }

        Ok((schema, batches))
    }
}

impl Drop for ArrowArrayStream {
    fn drop(&mut self) {
        if let Some(rel) = self.release {
            // SAFETY: a set `release` means `self` is a live, unreleased stream and
            // `rel` is the producer's callback for it; we own it, so this is the
            // single release the C Stream Interface requires.
            unsafe { rel(self) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arrow_c_data_and_stream_roundtrip() {
        let schema = Arc::new(ColumnSchema::new(vec![
            ColumnField::new("h3_index", ArrowDataType::UInt64, false),
            ColumnField::new("h3_hex", ArrowDataType::Utf8, false),
            ColumnField::new("stddev", ArrowDataType::Float64, true),
            ColumnField::new("resolution", ArrowDataType::UInt8, false),
            ColumnField::new("is_valid", ArrowDataType::Boolean, false),
            ColumnField::new("wkb", ArrowDataType::Binary, false),
        ]));

        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                ColumnArray::UInt64 {
                    values: vec![0x8828308281fffff, 0x8828308283fffff],
                    validity: None,
                },
                ColumnArray::Utf8 {
                    values: vec!["8828308281fffff".to_string(), "8828308283fffff".to_string()],
                    validity: None,
                },
                ColumnArray::Float64 {
                    values: vec![0.0, 4.25],
                    validity: Some(vec![false, true]),
                },
                ColumnArray::UInt8 {
                    values: vec![8, 8],
                    validity: None,
                },
                ColumnArray::Boolean {
                    values: vec![true, false],
                    validity: None,
                },
                ColumnArray::Binary {
                    values: vec![vec![1, 2, 3], vec![4, 5, 6, 7]],
                    validity: None,
                },
            ],
        )
        .unwrap();

        let mut stream = ArrowArrayStream::from_iterator(
            Arc::clone(&schema),
            vec![Ok(batch.clone())].into_iter(),
        );
        // SAFETY: freshly built by `from_iterator`, so valid and unreleased.
        let (out_schema, out_batches) = unsafe { stream.collect_batches().unwrap() };
        assert_eq!(*out_schema, *schema);
        assert_eq!(out_batches.len(), 1);
        assert_eq!(out_batches[0], batch);
    }

    fn sliceable_batch() -> (Arc<ColumnSchema>, RecordBatch) {
        let schema = Arc::new(ColumnSchema::new(vec![
            ColumnField::new("v", ArrowDataType::Int64, true),
            ColumnField::new("s", ArrowDataType::Utf8, false),
            ColumnField::new("b", ArrowDataType::Boolean, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                ColumnArray::Int64 {
                    values: vec![10, 20, 30],
                    validity: Some(vec![true, false, true]),
                },
                ColumnArray::Utf8 {
                    values: vec!["a".into(), "bb".into(), "ccc".into()],
                    validity: None,
                },
                ColumnArray::Boolean {
                    values: vec![true, false, true],
                    validity: None,
                },
            ],
        )
        .unwrap();
        (schema, batch)
    }

    #[test]
    fn release_callbacks_are_null_safe_and_idempotent() {
        let (schema, batch) = sliceable_batch();
        let mut raw_schema = ArrowSchema::from_column_schema(&schema);
        let mut array = ArrowArray::from_record_batch(batch.clone());
        let mut stream = ArrowArrayStream::from_iterator(schema, vec![Ok(batch)].into_iter());
        // SAFETY: null is supported; each non-null argument is owned and produced
        // by the matching constructor, and remains live until after both releases.
        unsafe {
            release_arrow_schema(ptr::null_mut());
            release_arrow_array(ptr::null_mut());
            release_arrow_stream(ptr::null_mut());
            for _ in 0..2 {
                release_arrow_schema(&mut raw_schema);
                release_arrow_array(&mut array);
                release_arrow_stream(&mut stream);
            }
        }
        assert!(raw_schema.release.is_none() && raw_schema.private_data.is_null());
        assert!(array.release.is_none() && array.private_data.is_null());
        assert!(stream.release.is_none() && stream.private_data.is_null());
    }

    #[test]
    fn owned_exports_can_be_released_on_another_thread() {
        let (schema, batch) = sliceable_batch();
        let raw_schema = ArrowSchema::from_column_schema(&schema);
        let array = ArrowArray::from_record_batch(batch.clone());
        let stream = ArrowArrayStream::from_iterator(schema, vec![Ok(batch)].into_iter());
        std::thread::spawn(move || {
            drop(raw_schema);
            drop(array);
            drop(stream);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn test_import_respects_parent_and_child_offsets() {
        let (schema, batch) = sliceable_batch();
        let expected = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                ColumnArray::Int64 {
                    values: vec![20, 30],
                    validity: Some(vec![false, true]),
                },
                ColumnArray::Utf8 {
                    values: vec!["bb".into(), "ccc".into()],
                    validity: None,
                },
                ColumnArray::Boolean {
                    values: vec![false, true],
                    validity: None,
                },
            ],
        )
        .unwrap();

        // Slice via the struct's offset.
        let mut arr = ArrowArray::from_record_batch(batch.clone());
        arr.offset = 1;
        arr.length = 2;
        // SAFETY: built from `batch` with `schema`; offset 1 + length 2 stays
        // within the 3-row children.
        let got = unsafe { arr.to_record_batch(Arc::clone(&schema)).unwrap() };
        assert_eq!(got, expected);

        // Slice via each child's offset.
        let mut arr = ArrowArray::from_record_batch(batch);
        arr.length = 2;
        for i in 0..arr.n_children as usize {
            // SAFETY: `i < n_children` and each child is a live box owned by `arr`.
            let child = unsafe { &mut **arr.children.add(i) };
            child.offset = 1;
            child.length = 2;
        }
        // SAFETY: each child's offset 1 + length 2 stays within its 3 rows.
        let got = unsafe { arr.to_record_batch(Arc::clone(&schema)).unwrap() };
        assert_eq!(got, expected);
    }

    #[test]
    fn test_import_reads_validity_when_null_count_unknown() {
        let (schema, batch) = sliceable_batch();
        let arr = ArrowArray::from_record_batch(batch.clone());
        // SAFETY: the batch has 3 columns, so child 0 exists and is owned by `arr`.
        unsafe { (**arr.children).null_count = -1 };
        // SAFETY: `arr` was built from `batch` with this `schema`.
        let got = unsafe { arr.to_record_batch(Arc::clone(&schema)).unwrap() };
        assert_eq!(got, batch);
    }
}
