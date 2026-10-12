//! Item exporters that serialise in Rust: JSON Lines, JSON, XML, CSV and
//! Parquet.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use parquet::basic::{Compression, LogicalType, Repetition, Type as Physical};
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::{Map, Value};

/// An item (dict, dataclass, list or scalar) as JSON. Dates and times
/// become ISO 8601 strings; floats that JSON can't hold (NaN, infinities)
/// become null.
fn to_json(obj: &Bound<'_, PyAny>, depth: usize) -> PyResult<Value> {
    if depth > 100 {
        return Err(PyValueError::new_err(
            "item nests deeper than 100 levels (a cycle?)",
        ));
    }
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if obj.is_instance_of::<PyInt>() {
        if let Ok(i) = obj.extract::<i64>() {
            return Ok(Value::from(i));
        }
        if let Ok(u) = obj.extract::<u64>() {
            return Ok(Value::from(u));
        }
        // Too big for 64 bits: keep every digit, as a string.
        return Ok(Value::String(obj.str()?.to_string()));
    }
    if let Ok(f) = obj.cast::<PyFloat>() {
        return Ok(serde_json::Number::from_f64(f.value()).map_or(Value::Null, Value::Number));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(Value::String(s.to_string()));
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        let mut map = Map::new();
        for (k, v) in d.iter() {
            let key = match k.cast::<PyString>() {
                Ok(s) => s.to_string(),
                Err(_) => k.str()?.to_string(),
            };
            map.insert(key, to_json(&v, depth + 1)?);
        }
        return Ok(Value::Object(map));
    }
    if obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>() {
        let mut out = Vec::new();
        for v in obj.try_iter()? {
            out.push(to_json(&v?, depth + 1)?);
        }
        return Ok(Value::Array(out));
    }
    let py = obj.py();
    if obj.hasattr("__dataclass_fields__")? {
        let as_dict = py.import("dataclasses")?.getattr("asdict")?.call1((obj,))?;
        return to_json(&as_dict, depth + 1);
    }
    if obj.hasattr("isoformat")? {
        return Ok(Value::String(obj.call_method0("isoformat")?.extract()?));
    }
    if obj.is_instance(&py.import("decimal")?.getattr("Decimal")?)? {
        return Ok(Value::String(obj.str()?.to_string()));
    }
    Err(PyTypeError::new_err(format!(
        "can't export a {} (use dicts, lists, str, numbers, bool, None, dates or dataclasses)",
        obj.get_type().name()?
    )))
}

fn open(path: &str) -> PyResult<BufWriter<File>> {
    open_for(path, false)
}

/// The file, emptied, or with `append` kept and added to.
fn open_for(path: &str, append: bool) -> PyResult<BufWriter<File>> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true);
    if append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    options
        .open(path)
        .map(BufWriter::new)
        .map_err(|e| PyValueError::new_err(format!("can't write {path}: {e}")))
}

fn io_error(e: std::io::Error) -> PyErr {
    pyo3::exceptions::PyOSError::new_err(e.to_string())
}

/// Writes one JSON object per line.
#[pyclass(frozen, module = "netweir")]
pub struct JsonlWriter {
    out: Mutex<Option<BufWriter<File>>>,
}

#[pymethods]
impl JsonlWriter {
    /// With `append`, items go after what the file already holds.
    #[new]
    #[pyo3(signature = (path, append=false))]
    fn new(path: &str, append: bool) -> PyResult<JsonlWriter> {
        Ok(JsonlWriter {
            out: Mutex::new(Some(open_for(path, append)?)),
        })
    }

    fn write(&self, item: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = to_json(item, 0)?;
        let mut guard = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let out = guard
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("the writer is closed"))?;
        serde_json::to_writer(&mut *out, &value).map_err(|e| io_error(e.into()))?;
        out.write_all(b"\n").map_err(io_error)
    }

    /// Pushes what's buffered to the file and asks the OS to keep it.
    fn flush(&self) -> PyResult<()> {
        if let Some(out) = self.out.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            out.flush().map_err(io_error)?;
            out.get_ref().sync_data().map_err(io_error)?;
        }
        Ok(())
    }

    fn close(&self) -> PyResult<()> {
        if let Some(mut out) = self.out.lock().unwrap_or_else(|e| e.into_inner()).take() {
            out.flush().map_err(io_error)?;
        }
        Ok(())
    }
}

/// Writes items as CSV rows under a header line.
#[pyclass(frozen, module = "netweir")]
pub struct CsvWriter {
    state: Mutex<CsvState>,
}

struct CsvState {
    out: Option<csv::Writer<BufWriter<File>>>,
    /// Column names; fixed by `fields` or by the first item's keys.
    fields: Option<Vec<String>>,
    header_written: bool,
}

fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        nested => nested.to_string(),
    }
}

#[pymethods]
impl CsvWriter {
    /// With `append`, rows go after what the file already holds, and a
    /// file that isn't empty is taken to have its header.
    #[new]
    #[pyo3(signature = (path, fields=None, append=false))]
    fn new(path: &str, fields: Option<Vec<String>>, append: bool) -> PyResult<CsvWriter> {
        let has_header = append && std::fs::metadata(path).is_ok_and(|m| m.len() > 0);
        let out = csv::Writer::from_writer(open_for(path, append)?);
        Ok(CsvWriter {
            state: Mutex::new(CsvState {
                out: Some(out),
                fields,
                header_written: has_header,
            }),
        })
    }

    /// Writes the item's row. Returns the keys it had that aren't columns,
    /// which are left out of the file.
    fn write(&self, item: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
        let Value::Object(map) = to_json(item, 0)? else {
            return Err(PyTypeError::new_err(
                "CSV rows need dict (or dataclass) items",
            ));
        };
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        let out = state
            .out
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("the writer is closed"))?;
        let csv_err = |e: csv::Error| io_error(std::io::Error::other(e));
        let fields = state
            .fields
            .get_or_insert_with(|| map.keys().cloned().collect());
        if !state.header_written {
            out.write_record(fields.iter()).map_err(csv_err)?;
            state.header_written = true;
        }
        out.write_record(
            fields
                .iter()
                .map(|f| map.get(f).map(cell).unwrap_or_default()),
        )
        .map_err(csv_err)?;
        Ok(map
            .keys()
            .filter(|k| !fields.contains(k))
            .cloned()
            .collect())
    }

    /// Pushes what's buffered to the file and asks the OS to keep it.
    fn flush(&self) -> PyResult<()> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(out) = guard.out.as_mut() {
            out.flush().map_err(io_error)?;
            out.get_ref().get_ref().sync_data().map_err(io_error)?;
        }
        Ok(())
    }

    fn close(&self) -> PyResult<()> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        if let Some(mut out) = state.out.take() {
            // No items arrived: still write the header if columns were given.
            if let (false, Some(fields)) = (state.header_written, &state.fields) {
                out.write_record(fields.iter())
                    .map_err(|e| io_error(std::io::Error::other(e)))?;
            }
            out.flush().map_err(io_error)?;
        }
        Ok(())
    }
}

/// Items read before the Parquet schema is fixed.
const SAMPLE: usize = 1_000;
/// Rows per Parquet row group.
const ROW_GROUP: usize = 10_000;

/// What a Parquet column holds.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Bool,
    Int,
    Double,
    Text,
}

impl Kind {
    /// The kind a value asks for; `None` for null, which fits anything.
    fn of(value: &Value) -> Option<Kind> {
        match value {
            Value::Null => None,
            Value::Bool(_) => Some(Kind::Bool),
            Value::Number(n) if n.is_i64() => Some(Kind::Int),
            // Above i64::MAX: no Parquet integer holds it exactly.
            Value::Number(n) if n.is_u64() => Some(Kind::Text),
            Value::Number(_) => Some(Kind::Double),
            _ => Some(Kind::Text),
        }
    }

    /// The narrowest kind holding both: integers and doubles make a
    /// double, any other mix makes text.
    fn merge(self, other: Kind) -> Kind {
        match (self, other) {
            (a, b) if a == b => a,
            (Kind::Int, Kind::Double) | (Kind::Double, Kind::Int) => Kind::Double,
            _ => Kind::Text,
        }
    }
}

/// One column's values for the row group being built, with a definition
/// level per row (0 for null).
enum Values {
    Bool(Vec<bool>),
    Int(Vec<i64>),
    Double(Vec<f64>),
    Text(Vec<ByteArray>),
}

struct Column {
    name: String,
    values: Values,
    defined: Vec<i16>,
}

impl Column {
    fn new(name: String, kind: Kind) -> Column {
        let values = match kind {
            Kind::Bool => Values::Bool(Vec::new()),
            Kind::Int => Values::Int(Vec::new()),
            Kind::Double => Values::Double(Vec::new()),
            Kind::Text => Values::Text(Vec::new()),
        };
        Column {
            name,
            values,
            defined: Vec::new(),
        }
    }

    fn schema(&self) -> PyResult<Arc<Type>> {
        let (physical, logical) = match self.values {
            Values::Bool(_) => (Physical::BOOLEAN, None),
            Values::Int(_) => (Physical::INT64, None),
            Values::Double(_) => (Physical::DOUBLE, None),
            Values::Text(_) => (Physical::BYTE_ARRAY, Some(LogicalType::String)),
        };
        Type::primitive_type_builder(&self.name, physical)
            .with_repetition(Repetition::OPTIONAL)
            .with_logical_type(logical)
            .build()
            .map(Arc::new)
            .map_err(parquet_error)
    }

    /// Adds the row's value. False if it doesn't fit the column, in which
    /// case the row gets a null.
    fn push(&mut self, value: Option<&Value>) -> bool {
        let fits = match (&mut self.values, value) {
            (_, None | Some(Value::Null)) => {
                self.defined.push(0);
                return true;
            }
            (Values::Bool(v), Some(Value::Bool(b))) => {
                v.push(*b);
                true
            }
            (Values::Int(v), Some(Value::Number(n))) => n.as_i64().map(|i| v.push(i)).is_some(),
            (Values::Double(v), Some(Value::Number(n))) => n.as_f64().map(|f| v.push(f)).is_some(),
            (Values::Text(v), Some(other)) => {
                v.push(ByteArray::from(cell(other).into_bytes()));
                true
            }
            _ => false,
        };
        self.defined.push(i16::from(fits));
        fits
    }

    fn clear(&mut self) {
        self.defined.clear();
        match &mut self.values {
            Values::Bool(v) => v.clear(),
            Values::Int(v) => v.clear(),
            Values::Double(v) => v.clear(),
            Values::Text(v) => v.clear(),
        }
    }
}

fn parquet_error(e: parquet::errors::ParquetError) -> PyErr {
    pyo3::exceptions::PyOSError::new_err(format!("parquet: {e}"))
}

/// Values a Parquet file couldn't hold, by key: (no column, wrong type).
type Problems = BTreeMap<String, (u64, u64)>;

struct ParquetState {
    file: Option<BufWriter<File>>,
    sample: Vec<Map<String, Value>>,
    columns: Vec<Column>,
    rows: usize,
    out: Option<SerializedFileWriter<BufWriter<File>>>,
    problems: Problems,
    closed: bool,
}

impl ParquetState {
    /// Fixes the schema from the sampled items, then writes them.
    fn start(&mut self) -> PyResult<()> {
        let mut kinds: Vec<(String, Option<Kind>)> = Vec::new();
        for item in &self.sample {
            for (key, value) in item {
                let kind = Kind::of(value);
                match kinds.iter_mut().find(|(k, _)| k == key) {
                    Some((_, seen)) => {
                        *seen = match (*seen, kind) {
                            (Some(a), Some(b)) => Some(a.merge(b)),
                            (a, b) => a.or(b),
                        }
                    }
                    None => kinds.push((key.clone(), kind)),
                }
            }
        }
        self.columns = kinds
            .into_iter()
            .map(|(name, kind)| Column::new(name, kind.unwrap_or(Kind::Text)))
            .collect();
        let fields = self
            .columns
            .iter()
            .map(Column::schema)
            .collect::<PyResult<Vec<_>>>()?;
        let schema = Type::group_type_builder("item")
            .with_fields(fields)
            .build()
            .map_err(parquet_error)?;
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        let file = self.file.take().expect("parquet writer started twice");
        self.out = Some(
            SerializedFileWriter::new(file, Arc::new(schema), Arc::new(props))
                .map_err(parquet_error)?,
        );
        for item in std::mem::take(&mut self.sample) {
            self.add(&item)?;
        }
        Ok(())
    }

    fn add(&mut self, item: &Map<String, Value>) -> PyResult<()> {
        for column in &mut self.columns {
            if !column.push(item.get(&column.name)) {
                self.problems.entry(column.name.clone()).or_default().1 += 1;
            }
        }
        for key in item.keys() {
            if !self.columns.iter().any(|c| &c.name == key) {
                self.problems.entry(key.clone()).or_default().0 += 1;
            }
        }
        self.rows += 1;
        if self.rows == ROW_GROUP {
            self.flush()?;
        }
        Ok(())
    }

    /// Writes the buffered rows as one row group.
    fn flush(&mut self) -> PyResult<()> {
        if self.rows == 0 {
            return Ok(());
        }
        let out = self.out.as_mut().expect("parquet rows before the schema");
        let mut group = out.next_row_group().map_err(parquet_error)?;
        for column in &mut self.columns {
            let mut writer = group
                .next_column()
                .map_err(parquet_error)?
                .expect("one column writer per schema field");
            let defined = Some(&column.defined[..]);
            match &column.values {
                Values::Bool(v) => writer.typed::<BoolType>().write_batch(v, defined, None),
                Values::Int(v) => writer.typed::<Int64Type>().write_batch(v, defined, None),
                Values::Double(v) => writer.typed::<DoubleType>().write_batch(v, defined, None),
                Values::Text(v) => writer
                    .typed::<ByteArrayType>()
                    .write_batch(v, defined, None),
            }
            .map_err(parquet_error)?;
            writer.close().map_err(parquet_error)?;
            column.clear();
        }
        group.close().map_err(parquet_error)?;
        self.rows = 0;
        Ok(())
    }
}

/// Writes items to a Parquet file. Column names and types come from the
/// first 1,000 items; see `write` for what happens to values that don't
/// fit them.
#[pyclass(frozen, module = "netweir")]
pub struct ParquetWriter {
    state: Mutex<ParquetState>,
}

#[pymethods]
impl ParquetWriter {
    #[new]
    fn new(path: &str) -> PyResult<ParquetWriter> {
        Ok(ParquetWriter {
            state: Mutex::new(ParquetState {
                file: Some(open(path)?),
                sample: Vec::new(),
                columns: Vec::new(),
                rows: 0,
                out: None,
                problems: Problems::new(),
                closed: false,
            }),
        })
    }

    /// Adds the item as a row. Once the schema is fixed, a key with no
    /// column is left out, an integer in a double column is widened, any
    /// value in a text column is stored as text, and any other mismatch is
    /// stored as null. `close` reports what was left out.
    fn write(&self, item: &Bound<'_, PyAny>) -> PyResult<()> {
        let Value::Object(map) = to_json(item, 0)? else {
            return Err(PyTypeError::new_err(
                "Parquet rows need dict (or dataclass) items",
            ));
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(PyValueError::new_err("the writer is closed"));
        }
        if state.out.is_none() {
            state.sample.push(map);
            if state.sample.len() == SAMPLE {
                state.start()?;
            }
            return Ok(());
        }
        state.add(&map)
    }

    /// Finishes the file. Returns, for each key some values of which
    /// couldn't be stored, how many had no column and how many had the
    /// wrong type.
    fn close(&self) -> PyResult<Vec<(String, u64, u64)>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Ok(Vec::new());
        }
        state.closed = true;
        if state.out.is_none() {
            state.start()?;
        }
        state.flush()?;
        if let Some(out) = state.out.take() {
            let mut file = out.into_inner().map_err(parquet_error)?;
            file.flush().map_err(io_error)?;
        }
        Ok(std::mem::take(&mut state.problems)
            .into_iter()
            .map(|(k, (missing, wrong))| (k, missing, wrong))
            .collect())
    }
}

/// A file of items between an opening and a closing: a JSON array or an
/// XML document. Each item is written with `separator` before it (the first
/// with `first`), and `foot` ends the file when it's closed.
struct Framed {
    out: Option<BufWriter<File>>,
    /// An item is in the file already: the next one needs the separator.
    any: bool,
    first: &'static [u8],
    separator: &'static [u8],
    foot: Vec<u8>,
}

impl Framed {
    /// Opens `path`, writing `head`; or with `append`, goes on from the last
    /// item of what's there. A resumed crawl cuts the file back to a length
    /// it recorded, which ends either after an item (recorded mid-crawl) or
    /// after `foot` (recorded once the file was closed): a `foot` is taken
    /// off, and whether the file has items is read from whether what's left
    /// ends with `head`. A file that doesn't start with `head` isn't one this
    /// writer made, and isn't added to. The foot always starts with a
    /// newline that no item has, so an item ending as the foot ends (a JSON
    /// list ends with `]`) isn't mistaken for it.
    fn open(
        path: &str,
        append: bool,
        head: &[u8],
        first: &'static [u8],
        separator: &'static [u8],
        foot: Vec<u8>,
    ) -> PyResult<Framed> {
        let cant = |e: std::io::Error| PyValueError::new_err(format!("can't write {path}: {e}"));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(!append)
            .open(path)
            .map_err(cant)?;
        let len = file.metadata().map_err(cant)?.len();
        let mut any = false;
        if append && len > 0 {
            let mut start = vec![0; head.len().min(len as usize)];
            file.read_exact(&mut start).map_err(cant)?;
            if start != head {
                return Err(PyValueError::new_err(format!(
                    "can't add to {path}: it doesn't start as this format's files do ({})",
                    String::from_utf8_lossy(head).replace('\n', " ")
                )));
            }
            // Only the end matters: the foot, whitespace, and enough before
            // to see the head.
            let tail_len = len.min((head.len() + foot.len() + 4096) as u64);
            file.seek(SeekFrom::Start(len - tail_len)).map_err(cant)?;
            let mut tail = Vec::with_capacity(tail_len as usize);
            file.read_to_end(&mut tail).map_err(cant)?;
            let mut end = trim_end(&tail);
            let foot = trim_end_slice(&foot);
            if tail[..end].ends_with(foot) {
                end = trim_end(&tail[..end - foot.len()]);
            }
            any = !tail[..end].ends_with(trim_end_slice(head));
            let keep = len - tail_len + end as u64;
            file.set_len(keep).map_err(cant)?;
            file.seek(SeekFrom::Start(keep)).map_err(cant)?;
        } else {
            file.write_all(head).map_err(cant)?;
        }
        Ok(Framed {
            out: Some(BufWriter::new(file)),
            any,
            first,
            separator,
            foot,
        })
    }

    fn write(&mut self, item: &[u8]) -> PyResult<()> {
        let out = self
            .out
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("the writer is closed"))?;
        out.write_all(if self.any { self.separator } else { self.first })
            .map_err(io_error)?;
        out.write_all(item).map_err(io_error)?;
        self.any = true;
        Ok(())
    }

    fn flush(&mut self) -> PyResult<()> {
        if let Some(out) = self.out.as_mut() {
            out.flush().map_err(io_error)?;
            out.get_ref().sync_data().map_err(io_error)?;
        }
        Ok(())
    }

    fn close(&mut self) -> PyResult<()> {
        if let Some(mut out) = self.out.take() {
            out.write_all(&self.foot).map_err(io_error)?;
            out.flush().map_err(io_error)?;
        }
        Ok(())
    }
}

/// Where `bytes` ends once trailing whitespace is left out.
fn trim_end(bytes: &[u8]) -> usize {
    bytes.len()
        - bytes
            .iter()
            .rev()
            .take_while(|b| b.is_ascii_whitespace())
            .count()
}

fn trim_end_slice(bytes: &[u8]) -> &[u8] {
    &bytes[..trim_end(bytes)]
}

/// Writes one JSON array, an item a line, as Scrapy's JSON exporter does.
#[pyclass(module = "netweir")]
pub struct JsonWriter {
    out: Mutex<Framed>,
}

#[pymethods]
impl JsonWriter {
    /// With `append`, items go after the last one the file already holds.
    #[new]
    #[pyo3(signature = (path, append=false))]
    fn new(path: &str, append: bool) -> PyResult<JsonWriter> {
        let framed = Framed::open(path, append, b"[", b"\n", b",\n", b"\n]\n".to_vec())?;
        Ok(JsonWriter {
            out: Mutex::new(framed),
        })
    }

    fn write(&self, item: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = to_json(item, 0)?;
        let text = serde_json::to_vec(&value).map_err(|e| io_error(e.into()))?;
        self.out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(&text)
    }

    fn flush(&self) -> PyResult<()> {
        self.out.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }

    fn close(&self) -> PyResult<()> {
        self.out.lock().unwrap_or_else(|e| e.into_inner()).close()
    }
}

/// Writes an XML document laid out as Scrapy's XML exporter lays it out:
/// one `item` element a line inside a `root` element.
#[pyclass(module = "netweir")]
pub struct XmlWriter {
    out: Mutex<Framed>,
    item: String,
}

#[pymethods]
impl XmlWriter {
    #[new]
    #[pyo3(signature = (path, root="items", item="item", append=false))]
    fn new(path: &str, root: &str, item: &str, append: bool) -> PyResult<XmlWriter> {
        let root = xml_name(root);
        if root == xml_name(item) {
            // The file's closing tag would be an item's.
            return Err(PyValueError::new_err(format!(
                "the root and item elements need different names, not both {root:?}"
            )));
        }
        let head = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<{root}>");
        let foot = format!("\n</{root}>\n").into_bytes();
        let framed = Framed::open(path, append, head.as_bytes(), b"\n", b"\n", foot)?;
        Ok(XmlWriter {
            out: Mutex::new(framed),
            item: xml_name(item),
        })
    }

    fn write(&self, item: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = to_json(item, 0)?;
        let mut text = Vec::new();
        xml_element(&mut text, &self.item, &value);
        self.out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(&text)
    }

    fn flush(&self) -> PyResult<()> {
        self.out.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }

    fn close(&self) -> PyResult<()> {
        self.out.lock().unwrap_or_else(|e| e.into_inner()).close()
    }
}

/// `<name>value</name>`. A dict's keys become elements, a list's values
/// `<value>` elements, as in Scrapy; null is an empty element.
fn xml_element(out: &mut Vec<u8>, name: &str, value: &Value) {
    out.push(b'<');
    out.extend_from_slice(name.as_bytes());
    out.push(b'>');
    match value {
        Value::Null => {}
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => xml_text(out, s),
        Value::Array(values) => {
            for v in values {
                xml_element(out, "value", v);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                xml_element(out, &xml_name(k), v);
            }
        }
    }
    out.extend_from_slice(b"</");
    out.extend_from_slice(name.as_bytes());
    out.push(b'>');
}

/// Text escaped for XML, without the characters XML 1.0 can't hold at all
/// (control characters other than tab, newline and carriage return).
fn xml_text(out: &mut Vec<u8>, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.extend_from_slice(b"&amp;"),
            '<' => out.extend_from_slice(b"&lt;"),
            '>' => out.extend_from_slice(b"&gt;"),
            '\t' | '\n' | '\r' => out.push(c as u8),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
}

/// `key` as an XML element name: each character a name can't hold becomes
/// `_`, and a name that would start with something a name can't start with
/// (a digit, a hyphen, a dot, or the reserved "xml") gets `_` in front.
/// Colons are left out too: they'd make a namespace prefix.
fn xml_name(key: &str) -> String {
    let mut name: String = key
        .chars()
        .map(|c| if c != ':' && name_char(c) { c } else { '_' })
        .collect();
    let starts_badly = name.chars().next().is_none_or(|c| !name_start_char(c));
    if starts_badly || name.to_ascii_lowercase().starts_with("xml") {
        name.insert(0, '_');
    }
    name
}

/// A character a name can start with, without ':'. Beyond ASCII these are
/// XML 1.0's fourth-edition letters (Appendix B), which every parser
/// accepts; the fifth edition allows more, but expat, which Python's
/// ElementTree uses, doesn't.
fn name_start_char(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || in_ranges(c, NAME_START)
}

/// A character a name can hold after its first, without ':'.
fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') || in_ranges(c, NAME_CHAR)
}

fn in_ranges(c: char, ranges: &[(u32, u32)]) -> bool {
    let c = c as u32;
    ranges
        .binary_search_by(|&(low, high)| {
            if high < c {
                std::cmp::Ordering::Less
            } else if low > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

// The fourth edition's name characters above ASCII, as inclusive code point
// ranges: read off expat by trying every character of the Basic
// Multilingual Plane as a name's first character, and after an "a".
#[rustfmt::skip]
const NAME_START: &[(u32, u32)] = &[
    (0x00C0, 0x00D6),
    (0x00D8, 0x00F6),
    (0x00F8, 0x0131),
    (0x0134, 0x013E),
    (0x0141, 0x0148),
    (0x014A, 0x017E),
    (0x0180, 0x01C3),
    (0x01CD, 0x01F0),
    (0x01F4, 0x01F5),
    (0x01FA, 0x0217),
    (0x0250, 0x02A8),
    (0x02BB, 0x02C1),
    (0x0386, 0x0386),
    (0x0388, 0x038A),
    (0x038C, 0x038C),
    (0x038E, 0x03A1),
    (0x03A3, 0x03CE),
    (0x03D0, 0x03D6),
    (0x03DA, 0x03DA),
    (0x03DC, 0x03DC),
    (0x03DE, 0x03DE),
    (0x03E0, 0x03E0),
    (0x03E2, 0x03F3),
    (0x0401, 0x040C),
    (0x040E, 0x044F),
    (0x0451, 0x045C),
    (0x045E, 0x0481),
    (0x0490, 0x04C4),
    (0x04C7, 0x04C8),
    (0x04CB, 0x04CC),
    (0x04D0, 0x04EB),
    (0x04EE, 0x04F5),
    (0x04F8, 0x04F9),
    (0x0531, 0x0556),
    (0x0559, 0x0559),
    (0x0561, 0x0586),
    (0x05D0, 0x05EA),
    (0x05F0, 0x05F2),
    (0x0621, 0x063A),
    (0x0641, 0x064A),
    (0x0671, 0x06B7),
    (0x06BA, 0x06BE),
    (0x06C0, 0x06CE),
    (0x06D0, 0x06D3),
    (0x06D5, 0x06D5),
    (0x06E5, 0x06E6),
    (0x0905, 0x0939),
    (0x093D, 0x093D),
    (0x0958, 0x0961),
    (0x0985, 0x098C),
    (0x098F, 0x0990),
    (0x0993, 0x09A8),
    (0x09AA, 0x09B0),
    (0x09B2, 0x09B2),
    (0x09B6, 0x09B9),
    (0x09DC, 0x09DD),
    (0x09DF, 0x09E1),
    (0x09F0, 0x09F1),
    (0x0A05, 0x0A0A),
    (0x0A0F, 0x0A10),
    (0x0A13, 0x0A28),
    (0x0A2A, 0x0A30),
    (0x0A32, 0x0A33),
    (0x0A35, 0x0A36),
    (0x0A38, 0x0A39),
    (0x0A59, 0x0A5C),
    (0x0A5E, 0x0A5E),
    (0x0A72, 0x0A74),
    (0x0A85, 0x0A8B),
    (0x0A8D, 0x0A8D),
    (0x0A8F, 0x0A91),
    (0x0A93, 0x0AA8),
    (0x0AAA, 0x0AB0),
    (0x0AB2, 0x0AB3),
    (0x0AB5, 0x0AB9),
    (0x0ABD, 0x0ABD),
    (0x0AE0, 0x0AE0),
    (0x0B05, 0x0B0C),
    (0x0B0F, 0x0B10),
    (0x0B13, 0x0B28),
    (0x0B2A, 0x0B30),
    (0x0B32, 0x0B33),
    (0x0B36, 0x0B39),
    (0x0B3D, 0x0B3D),
    (0x0B5C, 0x0B5D),
    (0x0B5F, 0x0B61),
    (0x0B85, 0x0B8A),
    (0x0B8E, 0x0B90),
    (0x0B92, 0x0B95),
    (0x0B99, 0x0B9A),
    (0x0B9C, 0x0B9C),
    (0x0B9E, 0x0B9F),
    (0x0BA3, 0x0BA4),
    (0x0BA8, 0x0BAA),
    (0x0BAE, 0x0BB5),
    (0x0BB7, 0x0BB9),
    (0x0C05, 0x0C0C),
    (0x0C0E, 0x0C10),
    (0x0C12, 0x0C28),
    (0x0C2A, 0x0C33),
    (0x0C35, 0x0C39),
    (0x0C60, 0x0C61),
    (0x0C85, 0x0C8C),
    (0x0C8E, 0x0C90),
    (0x0C92, 0x0CA8),
    (0x0CAA, 0x0CB3),
    (0x0CB5, 0x0CB9),
    (0x0CDE, 0x0CDE),
    (0x0CE0, 0x0CE1),
    (0x0D05, 0x0D0C),
    (0x0D0E, 0x0D10),
    (0x0D12, 0x0D28),
    (0x0D2A, 0x0D39),
    (0x0D60, 0x0D61),
    (0x0E01, 0x0E2E),
    (0x0E30, 0x0E30),
    (0x0E32, 0x0E33),
    (0x0E40, 0x0E45),
    (0x0E81, 0x0E82),
    (0x0E84, 0x0E84),
    (0x0E87, 0x0E88),
    (0x0E8A, 0x0E8A),
    (0x0E8D, 0x0E8D),
    (0x0E94, 0x0E97),
    (0x0E99, 0x0E9F),
    (0x0EA1, 0x0EA3),
    (0x0EA5, 0x0EA5),
    (0x0EA7, 0x0EA7),
    (0x0EAA, 0x0EAB),
    (0x0EAD, 0x0EAE),
    (0x0EB0, 0x0EB0),
    (0x0EB2, 0x0EB3),
    (0x0EBD, 0x0EBD),
    (0x0EC0, 0x0EC4),
    (0x0F40, 0x0F47),
    (0x0F49, 0x0F69),
    (0x10A0, 0x10C5),
    (0x10D0, 0x10F6),
    (0x1100, 0x1100),
    (0x1102, 0x1103),
    (0x1105, 0x1107),
    (0x1109, 0x1109),
    (0x110B, 0x110C),
    (0x110E, 0x1112),
    (0x113C, 0x113C),
    (0x113E, 0x113E),
    (0x1140, 0x1140),
    (0x114C, 0x114C),
    (0x114E, 0x114E),
    (0x1150, 0x1150),
    (0x1154, 0x1155),
    (0x1159, 0x1159),
    (0x115F, 0x1161),
    (0x1163, 0x1163),
    (0x1165, 0x1165),
    (0x1167, 0x1167),
    (0x1169, 0x1169),
    (0x116D, 0x116E),
    (0x1172, 0x1173),
    (0x1175, 0x1175),
    (0x119E, 0x119E),
    (0x11A8, 0x11A8),
    (0x11AB, 0x11AB),
    (0x11AE, 0x11AF),
    (0x11B7, 0x11B8),
    (0x11BA, 0x11BA),
    (0x11BC, 0x11C2),
    (0x11EB, 0x11EB),
    (0x11F0, 0x11F0),
    (0x11F9, 0x11F9),
    (0x1E00, 0x1E9B),
    (0x1EA0, 0x1EF9),
    (0x1F00, 0x1F15),
    (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45),
    (0x1F48, 0x1F4D),
    (0x1F50, 0x1F57),
    (0x1F59, 0x1F59),
    (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D),
    (0x1F5F, 0x1F7D),
    (0x1F80, 0x1FB4),
    (0x1FB6, 0x1FBC),
    (0x1FBE, 0x1FBE),
    (0x1FC2, 0x1FC4),
    (0x1FC6, 0x1FCC),
    (0x1FD0, 0x1FD3),
    (0x1FD6, 0x1FDB),
    (0x1FE0, 0x1FEC),
    (0x1FF2, 0x1FF4),
    (0x1FF6, 0x1FFC),
    (0x2126, 0x2126),
    (0x212A, 0x212B),
    (0x212E, 0x212E),
    (0x2180, 0x2182),
    (0x3007, 0x3007),
    (0x3021, 0x3029),
    (0x3041, 0x3094),
    (0x30A1, 0x30FA),
    (0x3105, 0x312C),
    (0x4E00, 0x9FA5),
    (0xAC00, 0xD7A3),
];

#[rustfmt::skip]
const NAME_CHAR: &[(u32, u32)] = &[
    (0x00B7, 0x00B7),
    (0x00C0, 0x00D6),
    (0x00D8, 0x00F6),
    (0x00F8, 0x0131),
    (0x0134, 0x013E),
    (0x0141, 0x0148),
    (0x014A, 0x017E),
    (0x0180, 0x01C3),
    (0x01CD, 0x01F0),
    (0x01F4, 0x01F5),
    (0x01FA, 0x0217),
    (0x0250, 0x02A8),
    (0x02BB, 0x02C1),
    (0x02D0, 0x02D1),
    (0x0300, 0x0345),
    (0x0360, 0x0361),
    (0x0386, 0x038A),
    (0x038C, 0x038C),
    (0x038E, 0x03A1),
    (0x03A3, 0x03CE),
    (0x03D0, 0x03D6),
    (0x03DA, 0x03DA),
    (0x03DC, 0x03DC),
    (0x03DE, 0x03DE),
    (0x03E0, 0x03E0),
    (0x03E2, 0x03F3),
    (0x0401, 0x040C),
    (0x040E, 0x044F),
    (0x0451, 0x045C),
    (0x045E, 0x0481),
    (0x0483, 0x0486),
    (0x0490, 0x04C4),
    (0x04C7, 0x04C8),
    (0x04CB, 0x04CC),
    (0x04D0, 0x04EB),
    (0x04EE, 0x04F5),
    (0x04F8, 0x04F9),
    (0x0531, 0x0556),
    (0x0559, 0x0559),
    (0x0561, 0x0586),
    (0x0591, 0x05A1),
    (0x05A3, 0x05B9),
    (0x05BB, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C4),
    (0x05D0, 0x05EA),
    (0x05F0, 0x05F2),
    (0x0621, 0x063A),
    (0x0640, 0x0652),
    (0x0660, 0x0669),
    (0x0670, 0x06B7),
    (0x06BA, 0x06BE),
    (0x06C0, 0x06CE),
    (0x06D0, 0x06D3),
    (0x06D5, 0x06E8),
    (0x06EA, 0x06ED),
    (0x06F0, 0x06F9),
    (0x0901, 0x0903),
    (0x0905, 0x0939),
    (0x093C, 0x094D),
    (0x0951, 0x0954),
    (0x0958, 0x0963),
    (0x0966, 0x096F),
    (0x0981, 0x0983),
    (0x0985, 0x098C),
    (0x098F, 0x0990),
    (0x0993, 0x09A8),
    (0x09AA, 0x09B0),
    (0x09B2, 0x09B2),
    (0x09B6, 0x09B9),
    (0x09BC, 0x09BC),
    (0x09BE, 0x09C4),
    (0x09C7, 0x09C8),
    (0x09CB, 0x09CD),
    (0x09D7, 0x09D7),
    (0x09DC, 0x09DD),
    (0x09DF, 0x09E3),
    (0x09E6, 0x09F1),
    (0x0A02, 0x0A02),
    (0x0A05, 0x0A0A),
    (0x0A0F, 0x0A10),
    (0x0A13, 0x0A28),
    (0x0A2A, 0x0A30),
    (0x0A32, 0x0A33),
    (0x0A35, 0x0A36),
    (0x0A38, 0x0A39),
    (0x0A3C, 0x0A3C),
    (0x0A3E, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4D),
    (0x0A59, 0x0A5C),
    (0x0A5E, 0x0A5E),
    (0x0A66, 0x0A74),
    (0x0A81, 0x0A83),
    (0x0A85, 0x0A8B),
    (0x0A8D, 0x0A8D),
    (0x0A8F, 0x0A91),
    (0x0A93, 0x0AA8),
    (0x0AAA, 0x0AB0),
    (0x0AB2, 0x0AB3),
    (0x0AB5, 0x0AB9),
    (0x0ABC, 0x0AC5),
    (0x0AC7, 0x0AC9),
    (0x0ACB, 0x0ACD),
    (0x0AE0, 0x0AE0),
    (0x0AE6, 0x0AEF),
    (0x0B01, 0x0B03),
    (0x0B05, 0x0B0C),
    (0x0B0F, 0x0B10),
    (0x0B13, 0x0B28),
    (0x0B2A, 0x0B30),
    (0x0B32, 0x0B33),
    (0x0B36, 0x0B39),
    (0x0B3C, 0x0B43),
    (0x0B47, 0x0B48),
    (0x0B4B, 0x0B4D),
    (0x0B56, 0x0B57),
    (0x0B5C, 0x0B5D),
    (0x0B5F, 0x0B61),
    (0x0B66, 0x0B6F),
    (0x0B82, 0x0B83),
    (0x0B85, 0x0B8A),
    (0x0B8E, 0x0B90),
    (0x0B92, 0x0B95),
    (0x0B99, 0x0B9A),
    (0x0B9C, 0x0B9C),
    (0x0B9E, 0x0B9F),
    (0x0BA3, 0x0BA4),
    (0x0BA8, 0x0BAA),
    (0x0BAE, 0x0BB5),
    (0x0BB7, 0x0BB9),
    (0x0BBE, 0x0BC2),
    (0x0BC6, 0x0BC8),
    (0x0BCA, 0x0BCD),
    (0x0BD7, 0x0BD7),
    (0x0BE7, 0x0BEF),
    (0x0C01, 0x0C03),
    (0x0C05, 0x0C0C),
    (0x0C0E, 0x0C10),
    (0x0C12, 0x0C28),
    (0x0C2A, 0x0C33),
    (0x0C35, 0x0C39),
    (0x0C3E, 0x0C44),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4D),
    (0x0C55, 0x0C56),
    (0x0C60, 0x0C61),
    (0x0C66, 0x0C6F),
    (0x0C82, 0x0C83),
    (0x0C85, 0x0C8C),
    (0x0C8E, 0x0C90),
    (0x0C92, 0x0CA8),
    (0x0CAA, 0x0CB3),
    (0x0CB5, 0x0CB9),
    (0x0CBE, 0x0CC4),
    (0x0CC6, 0x0CC8),
    (0x0CCA, 0x0CCD),
    (0x0CD5, 0x0CD6),
    (0x0CDE, 0x0CDE),
    (0x0CE0, 0x0CE1),
    (0x0CE6, 0x0CEF),
    (0x0D02, 0x0D03),
    (0x0D05, 0x0D0C),
    (0x0D0E, 0x0D10),
    (0x0D12, 0x0D28),
    (0x0D2A, 0x0D39),
    (0x0D3E, 0x0D43),
    (0x0D46, 0x0D48),
    (0x0D4A, 0x0D4D),
    (0x0D57, 0x0D57),
    (0x0D60, 0x0D61),
    (0x0D66, 0x0D6F),
    (0x0E01, 0x0E2E),
    (0x0E30, 0x0E3A),
    (0x0E40, 0x0E4E),
    (0x0E50, 0x0E59),
    (0x0E81, 0x0E82),
    (0x0E84, 0x0E84),
    (0x0E87, 0x0E88),
    (0x0E8A, 0x0E8A),
    (0x0E8D, 0x0E8D),
    (0x0E94, 0x0E97),
    (0x0E99, 0x0E9F),
    (0x0EA1, 0x0EA3),
    (0x0EA5, 0x0EA5),
    (0x0EA7, 0x0EA7),
    (0x0EAA, 0x0EAB),
    (0x0EAD, 0x0EAE),
    (0x0EB0, 0x0EB9),
    (0x0EBB, 0x0EBD),
    (0x0EC0, 0x0EC4),
    (0x0EC6, 0x0EC6),
    (0x0EC8, 0x0ECD),
    (0x0ED0, 0x0ED9),
    (0x0F18, 0x0F19),
    (0x0F20, 0x0F29),
    (0x0F35, 0x0F35),
    (0x0F37, 0x0F37),
    (0x0F39, 0x0F39),
    (0x0F3E, 0x0F47),
    (0x0F49, 0x0F69),
    (0x0F71, 0x0F84),
    (0x0F86, 0x0F8B),
    (0x0F90, 0x0F95),
    (0x0F97, 0x0F97),
    (0x0F99, 0x0FAD),
    (0x0FB1, 0x0FB7),
    (0x0FB9, 0x0FB9),
    (0x10A0, 0x10C5),
    (0x10D0, 0x10F6),
    (0x1100, 0x1100),
    (0x1102, 0x1103),
    (0x1105, 0x1107),
    (0x1109, 0x1109),
    (0x110B, 0x110C),
    (0x110E, 0x1112),
    (0x113C, 0x113C),
    (0x113E, 0x113E),
    (0x1140, 0x1140),
    (0x114C, 0x114C),
    (0x114E, 0x114E),
    (0x1150, 0x1150),
    (0x1154, 0x1155),
    (0x1159, 0x1159),
    (0x115F, 0x1161),
    (0x1163, 0x1163),
    (0x1165, 0x1165),
    (0x1167, 0x1167),
    (0x1169, 0x1169),
    (0x116D, 0x116E),
    (0x1172, 0x1173),
    (0x1175, 0x1175),
    (0x119E, 0x119E),
    (0x11A8, 0x11A8),
    (0x11AB, 0x11AB),
    (0x11AE, 0x11AF),
    (0x11B7, 0x11B8),
    (0x11BA, 0x11BA),
    (0x11BC, 0x11C2),
    (0x11EB, 0x11EB),
    (0x11F0, 0x11F0),
    (0x11F9, 0x11F9),
    (0x1E00, 0x1E9B),
    (0x1EA0, 0x1EF9),
    (0x1F00, 0x1F15),
    (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45),
    (0x1F48, 0x1F4D),
    (0x1F50, 0x1F57),
    (0x1F59, 0x1F59),
    (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D),
    (0x1F5F, 0x1F7D),
    (0x1F80, 0x1FB4),
    (0x1FB6, 0x1FBC),
    (0x1FBE, 0x1FBE),
    (0x1FC2, 0x1FC4),
    (0x1FC6, 0x1FCC),
    (0x1FD0, 0x1FD3),
    (0x1FD6, 0x1FDB),
    (0x1FE0, 0x1FEC),
    (0x1FF2, 0x1FF4),
    (0x1FF6, 0x1FFC),
    (0x20D0, 0x20DC),
    (0x20E1, 0x20E1),
    (0x2126, 0x2126),
    (0x212A, 0x212B),
    (0x212E, 0x212E),
    (0x2180, 0x2182),
    (0x3005, 0x3005),
    (0x3007, 0x3007),
    (0x3021, 0x302F),
    (0x3031, 0x3035),
    (0x3041, 0x3094),
    (0x3099, 0x309A),
    (0x309D, 0x309E),
    (0x30A1, 0x30FA),
    (0x30FC, 0x30FE),
    (0x3105, 0x312C),
    (0x4E00, 0x9FA5),
    (0xAC00, 0xD7A3),
];

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<JsonlWriter>()?;
    m.add_class::<JsonWriter>()?;
    m.add_class::<XmlWriter>()?;
    m.add_class::<CsvWriter>()?;
    m.add_class::<ParquetWriter>()?;
    Ok(())
}
