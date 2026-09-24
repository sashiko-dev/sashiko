use flate2::Compression;
use flate2::write::{GzDecoder, GzEncoder};
use libsql::Value;
use std::io::Write;

const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];

pub fn compress_string_if_needed(input: &str) -> Value {
    if input.len() > 1024 {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        if encoder.write_all(input.as_bytes()).is_ok()
            && let Ok(compressed) = encoder.finish()
        {
            return Value::Blob(compressed);
        }
    }
    Value::Text(input.to_string())
}

pub fn get_compressed_string(row: &libsql::Row, index: i32) -> Result<String, libsql::Error> {
    let val: Value = row.get(index)?;
    match val {
        Value::Text(s) => Ok(s),
        Value::Blob(b) => decompress_gzip(&b)
            .map(Ok)
            .unwrap_or_else(|| String::from_utf8(b).map_err(|_| libsql::Error::InvalidColumnType)),
        Value::Null => Ok(String::new()),
        _ => Err(libsql::Error::InvalidColumnType),
    }
}

pub fn decompress_string_value(value: &Value) -> Result<String, libsql::Error> {
    match value {
        Value::Text(s) => Ok(s.clone()),
        Value::Blob(b) => decompress_gzip(b).map(Ok).unwrap_or_else(|| {
            String::from_utf8(b.clone()).map_err(|_| libsql::Error::InvalidColumnType)
        }),
        Value::Null => Ok(String::new()),
        _ => Err(libsql::Error::InvalidColumnType),
    }
}

fn decompress_gzip(bytes: &[u8]) -> Option<String> {
    if !bytes.starts_with(&GZIP_MAGIC) {
        return None;
    }
    let mut decoder = GzDecoder::new(Vec::new());
    decoder.write_all(bytes).ok()?;
    String::from_utf8(decoder.finish().ok()?).ok()
}

pub fn get_compressed_string_opt(
    row: &libsql::Row,
    index: i32,
) -> Result<Option<String>, libsql::Error> {
    let val: Value = row.get(index)?;
    match val {
        Value::Null => Ok(None),
        Value::Text(s) => Ok(Some(s)),
        Value::Blob(b) => {
            if b.starts_with(&GZIP_MAGIC) {
                let mut decoder = GzDecoder::new(Vec::new());
                if decoder.write_all(&b).is_ok()
                    && let Ok(decompressed) = decoder.finish()
                    && let Ok(s) = String::from_utf8(decompressed)
                {
                    return Ok(Some(s));
                }
            }
            String::from_utf8(b)
                .map(Some)
                .map_err(|_| libsql::Error::InvalidColumnType)
        }
        _ => Err(libsql::Error::InvalidColumnType),
    }
}
