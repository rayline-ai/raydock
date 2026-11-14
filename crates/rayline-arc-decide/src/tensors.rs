//! The head tensors of a release's `model.safetensors`.
//!
//! Only tensors under the wanted prefixes are read; the encoder's weights
//! (most of the file) are skipped. The format is the safetensors one: an
//! 8-byte little-endian header length, a JSON header naming each tensor's
//! dtype, shape and byte range, then the data.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// The largest header this reader accepts. A release's is about 50 KB.
const MAX_HEADER_BYTES: u64 = 64 << 20;
/// The largest single head tensor this reader accepts.
const MAX_TENSOR_BYTES: u64 = 1 << 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorError(pub String);

impl std::fmt::Display for TensorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TensorError {}

fn error<T>(message: impl Into<String>) -> Result<T, TensorError> {
    Err(TensorError(message.into()))
}

#[derive(Debug, Clone)]
enum Data {
    F64(Vec<f64>),
    F32(Vec<f32>),
    I64(Vec<i64>),
    U8(Vec<u8>),
}

/// One tensor, row-major.
#[derive(Debug, Clone)]
pub struct Tensor {
    pub shape: Vec<usize>,
    data: Data,
}

/// The tensors read from one file, keyed by name without the prefix they
/// were selected by.
#[derive(Debug, Default)]
pub struct Tensors {
    by_name: HashMap<String, Tensor>,
}

impl Tensors {
    /// Reads every tensor whose name starts with `prefix`.
    pub fn read(path: &Path, prefix: &str) -> Result<Self, TensorError> {
        let mut file =
            File::open(path).map_err(|e| TensorError(format!("open {}: {e}", path.display())))?;
        let io = |e: std::io::Error| TensorError(format!("read {}: {e}", path.display()));
        let mut length = [0u8; 8];
        file.read_exact(&mut length).map_err(io)?;
        let length = u64::from_le_bytes(length);
        if length > MAX_HEADER_BYTES {
            return error("the weights header is implausibly large");
        }
        let mut raw = vec![0u8; length as usize];
        file.read_exact(&mut raw).map_err(io)?;
        let header: Value = serde_json::from_slice(&raw)
            .map_err(|e| TensorError(format!("the weights header is not JSON: {e}")))?;
        let Some(entries) = header.as_object() else {
            return error("the weights header is not an object");
        };
        let base = 8 + length;
        let size = file.metadata().map_err(io)?.len();
        let mut by_name = HashMap::new();
        for (name, entry) in entries {
            let Some(short) = name.strip_prefix(prefix) else {
                continue;
            };
            let dtype = entry.get("dtype").and_then(Value::as_str).unwrap_or("");
            let shape: Option<Vec<usize>> =
                entry
                    .get("shape")
                    .and_then(Value::as_array)
                    .and_then(|dims| {
                        dims.iter()
                            .map(|d| d.as_u64().map(|d| d as usize))
                            .collect()
                    });
            let range = entry
                .get("data_offsets")
                .and_then(Value::as_array)
                .filter(|r| r.len() == 2)
                .and_then(|r| Some((r[0].as_u64()?, r[1].as_u64()?)));
            let (Some(shape), Some((start, end))) = (shape, range) else {
                return error(format!("tensor {name} has a malformed header entry"));
            };
            let width: u64 = match dtype {
                "F64" | "I64" => 8,
                "F32" => 4,
                "U8" | "BOOL" => 1,
                other => return error(format!("tensor {name} has unsupported dtype {other}")),
            };
            let count = shape.iter().try_fold(1u64, |n, d| n.checked_mul(*d as u64));
            let bytes = end.checked_sub(start);
            if count.and_then(|c| c.checked_mul(width)) != bytes
                || bytes.is_none_or(|b| b > MAX_TENSOR_BYTES)
                || base.checked_add(end).is_none_or(|stop| stop > size)
            {
                return error(format!("tensor {name} does not fit its byte range"));
            }
            let mut buffer = vec![0u8; (end - start) as usize];
            file.seek(SeekFrom::Start(base + start)).map_err(io)?;
            file.read_exact(&mut buffer).map_err(io)?;
            let data = match dtype {
                "F64" => Data::F64(
                    buffer
                        .chunks_exact(8)
                        .map(|c| {
                            f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                        })
                        .collect(),
                ),
                "I64" => Data::I64(
                    buffer
                        .chunks_exact(8)
                        .map(|c| {
                            i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                        })
                        .collect(),
                ),
                "F32" => Data::F32(
                    buffer
                        .chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect(),
                ),
                _ => Data::U8(buffer),
            };
            by_name.insert(short.to_owned(), Tensor { shape, data });
        }
        Ok(Self { by_name })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    fn get(&self, name: &str) -> Result<&Tensor, TensorError> {
        self.by_name
            .get(name)
            .ok_or_else(|| TensorError(format!("the weights carry no tensor {name}")))
    }

    fn shaped<'a>(&'a self, name: &str, shape: &[usize]) -> Result<&'a Tensor, TensorError> {
        let tensor = self.get(name)?;
        if tensor.shape != shape {
            return error(format!(
                "tensor {name} has shape {:?}, expected {shape:?}",
                tensor.shape
            ));
        }
        Ok(tensor)
    }

    pub fn shape(&self, name: &str) -> Result<&[usize], TensorError> {
        Ok(&self.get(name)?.shape)
    }

    pub fn f64(&self, name: &str, shape: &[usize]) -> Result<Vec<f64>, TensorError> {
        match &self.shaped(name, shape)?.data {
            Data::F64(values) => Ok(values.clone()),
            _ => error(format!("tensor {name} is not float64")),
        }
    }

    pub fn f32(&self, name: &str, shape: &[usize]) -> Result<Vec<f32>, TensorError> {
        match &self.shaped(name, shape)?.data {
            Data::F32(values) => Ok(values.clone()),
            _ => error(format!("tensor {name} is not float32")),
        }
    }

    pub fn i64(&self, name: &str, shape: &[usize]) -> Result<Vec<i64>, TensorError> {
        match &self.shaped(name, shape)?.data {
            Data::I64(values) => Ok(values.clone()),
            _ => error(format!("tensor {name} is not int64")),
        }
    }

    /// A flag tensor: nonzero is true.
    pub fn flags(&self, name: &str, shape: &[usize]) -> Result<Vec<bool>, TensorError> {
        match &self.shaped(name, shape)?.data {
            Data::U8(values) => Ok(values.iter().map(|v| *v != 0).collect()),
            _ => error(format!("tensor {name} is not a flag tensor")),
        }
    }
}
