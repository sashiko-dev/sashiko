use flate2::Compression;
use flate2::write::{GzDecoder, GzEncoder};
use libsql::Value;
use std::io::Write;

const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];
/// Strings strictly longer than this threshold are gzip-compressed on write.
pub const COMPRESSION_THRESHOLD_BYTES: usize = 1024;
/// Payloads at or above this threshold are compressed on the blocking thread
/// pool so large LLM transcripts, diffs, or logs do not stall Tokio worker
/// threads.
const BLOCKING_COMPRESSION_THRESHOLD_BYTES: usize = 64 * 1024;

pub fn compress_string_if_needed(input: &str) -> Value {
    if input.len() > COMPRESSION_THRESHOLD_BYTES {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        if encoder.write_all(input.as_bytes()).is_ok()
            && let Ok(compressed) = encoder.finish()
        {
            return Value::Blob(compressed);
        }
    }
    Value::Text(input.to_string())
}

/// Compresses strings exceeding [`COMPRESSION_THRESHOLD_BYTES`] into a gzip
/// `Value::Blob`, offloading payloads `>= 64 KiB` to `tokio::task::spawn_blocking`.
pub async fn compress_string_async(input: &str) -> Value {
    if input.len() >= BLOCKING_COMPRESSION_THRESHOLD_BYTES {
        let owned = input.to_owned();
        if let Ok(val) =
            tokio::task::spawn_blocking(move || compress_string_if_needed(&owned)).await
        {
            return val;
        }
    }
    compress_string_if_needed(input)
}

/// Optional counterpart to [`compress_string_async`], returning `Value::Null`
/// when `input` is `None`.
pub async fn compress_opt_string_async(input: Option<&str>) -> Value {
    match input {
        Some(s) => compress_string_async(s).await,
        None => Value::Null,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compress_string_async_handles_small_medium_and_large_payloads() {
        let small = "a".repeat(COMPRESSION_THRESHOLD_BYTES);
        assert_eq!(
            compress_string_async(&small).await,
            Value::Text(small.clone())
        );

        let medium = "b".repeat(COMPRESSION_THRESHOLD_BYTES + 1);
        let medium_val = compress_string_async(&medium).await;
        assert!(matches!(medium_val, Value::Blob(_)));
        assert_eq!(decompress_string_value(&medium_val).unwrap(), medium);

        let large = "c".repeat(BLOCKING_COMPRESSION_THRESHOLD_BYTES + 128);
        let large_val = compress_string_async(&large).await;
        assert!(matches!(large_val, Value::Blob(_)));
        assert_eq!(decompress_string_value(&large_val).unwrap(), large);

        assert_eq!(compress_opt_string_async(None).await, Value::Null);
        let opt_val = compress_opt_string_async(Some(&medium)).await;
        assert!(matches!(opt_val, Value::Blob(_)));
        assert_eq!(decompress_string_value(&opt_val).unwrap(), medium);
    }
}
