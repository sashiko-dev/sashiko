// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Database execution boundary shared by the daemon's storage backends.

use bytes::BytesMut;
use libsql::Value;
use libsql::params::{IntoParams, Params};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type};

const WRITE_LOCK: i32 = 0x53415348;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Libsql(#[from] libsql::Error),
    #[error("PostgreSQL operation failed: {0}")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("could not reconnect PostgreSQL session: {0}")]
    Reconnect(#[source] anyhow::Error),
    #[error("PostgreSQL session was canceled")]
    Canceled,
    #[error("invalid database value: {0}")]
    Value(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct PgConfig {
    pub url: String,
    pub schema: String,
    pub tls: native_tls::TlsConnector,
}

struct PgConnection {
    client: Mutex<tokio_postgres::Client>,
    driver: std::sync::Mutex<PgDriver>,
    config: PgConfig,
    session: PgSession,
    active: AtomicBool,
    broken: AtomicBool,
}

struct PgDriver(tokio::task::JoinHandle<()>);

impl Drop for PgDriver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct PgCall<'a> {
    connection: &'a PgConnection,
    complete: bool,
}

impl<'a> PgCall<'a> {
    fn new(connection: &'a PgConnection) -> Self {
        Self {
            connection,
            complete: false,
        }
    }
}

impl Drop for PgCall<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.connection.abort();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PgSession {
    Regular,
    Transaction,
    DedupLock,
}

impl PgConnection {
    fn driver(&self) -> std::sync::MutexGuard<'_, PgDriver> {
        self.driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn abort(&self) {
        self.broken.store(true, Ordering::Release);
        self.active.store(false, Ordering::Release);
        self.driver().0.abort();
    }

    async fn client(&self) -> Result<tokio::sync::MutexGuard<'_, tokio_postgres::Client>> {
        let mut client = self.client.lock().await;
        let broken = self.broken.load(Ordering::Acquire);
        if self.session != PgSession::Regular && broken {
            return Err(Error::Canceled);
        }
        if self.session == PgSession::Regular && (broken || client.is_closed()) {
            let replacement = Connection::connect_postgres(self.config.clone(), PgSession::Regular)
                .await
                .map_err(Error::Reconnect)?;
            if let Backend::Postgres(replacement) = replacement.0 {
                let mut replacement_client = replacement.client.lock().await;
                let mut driver = self.driver();
                let mut replacement_driver = replacement.driver();
                std::mem::swap(&mut *client, &mut *replacement_client);
                std::mem::swap(&mut *driver, &mut *replacement_driver);
                self.broken.store(false, Ordering::Release);
            }
        }
        Ok(client)
    }
}

#[derive(Clone)]
pub struct Connection(Backend);

#[derive(Clone)]
enum Backend {
    Libsql(libsql::Connection),
    Postgres(Arc<PgConnection>),
}

impl From<libsql::Connection> for Connection {
    fn from(conn: libsql::Connection) -> Self {
        Self(Backend::Libsql(conn))
    }
}

impl Connection {
    pub fn is_postgres(&self) -> bool {
        matches!(self.0, Backend::Postgres(_))
    }

    pub fn sql<'a>(&self, libsql: &'a str, postgres: &'a str) -> &'a str {
        if self.is_postgres() { postgres } else { libsql }
    }

    pub async fn postgres(config: PgConfig) -> anyhow::Result<Self> {
        Self::connect_postgres(config, PgSession::Regular).await
    }

    /// Hold a PostgreSQL session lock while canonical bug decisions are made.
    /// Closing the dedicated session releases the lock even on cancellation.
    pub async fn dedup_lock(&self) -> anyhow::Result<Option<Self>> {
        let Backend::Postgres(connection) = &self.0 else {
            return Ok(None);
        };
        let conn = Self::connect_postgres(connection.config.clone(), PgSession::DedupLock).await?;
        if let Backend::Postgres(connection) = &conn.0 {
            let client = connection.client.lock().await;
            let mut call = PgCall::new(connection);
            client
                .query_one(
                    "SELECT pg_catalog.pg_advisory_lock(pg_catalog.hashtext($1),$2)",
                    &[&connection.config.schema, &(WRITE_LOCK + 1)],
                )
                .await?;
            call.complete = true;
        }
        Ok(Some(conn))
    }

    async fn connect_postgres(config: PgConfig, session: PgSession) -> anyhow::Result<Self> {
        let transaction = session == PgSession::Transaction;
        anyhow::ensure!(
            !config.schema.is_empty()
                && config.schema.len() <= 63
                && !config.schema.starts_with("pg_")
                && config.schema != "information_schema"
                && config
                    .schema
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "PostgreSQL schema must be a non-system name of 1 to 63 letters, digits, or underscores"
        );
        let mut options: tokio_postgres::Config = config.url.parse()?;
        options.connect_timeout(std::time::Duration::from_secs(10));
        options.ssl_mode(tokio_postgres::config::SslMode::Require);
        let (mut client, driver) = options
            .connect(postgres_native_tls::MakeTlsConnector::new(
                config.tls.clone(),
            ))
            .await?;
        let driver = PgDriver(tokio::spawn(async move {
            if let Err(error) = driver.await {
                tracing::warn!("PostgreSQL connection ended: {error}");
            }
        }));
        let schema = format!("\"{}\"", config.schema);
        if !transaction {
            let tx = client.transaction().await?;
            pg_write_lock(&tx, &config.schema).await?;
            let exists: bool = tx
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname=$1)",
                    &[&config.schema],
                )
                .await?
                .get(0);
            if !exists {
                tx.batch_execute(&format!("CREATE SCHEMA {schema}")).await?;
            }
            tx.commit().await?;
        }
        client
            .batch_execute(&format!("SET search_path TO {schema}, pg_catalog"))
            .await?;
        if transaction {
            client.batch_execute("BEGIN").await?;
            pg_write_lock(&client, &config.schema).await?;
        }
        Ok(Self(Backend::Postgres(Arc::new(PgConnection {
            client: Mutex::new(client),
            driver: std::sync::Mutex::new(driver),
            config,
            session,
            active: AtomicBool::new(transaction),
            broken: AtomicBool::new(false),
        }))))
    }

    pub async fn query(&self, sql: &str, params: impl IntoParams) -> Result<Rows> {
        match &self.0 {
            Backend::Libsql(conn) => Ok(Rows::Libsql(conn.query(sql, params).await?)),
            Backend::Postgres(conn) => {
                let params = positional(params)?;
                let sql = placeholders(sql)?;
                let mut client = conn.client().await?;
                let mut call = PgCall::new(conn);
                let values = if conn.session == PgSession::Transaction {
                    pg_query(&*client, &sql, params).await?
                } else {
                    let tx = client.transaction().await?;
                    pg_write_lock(&tx, &conn.config.schema).await?;
                    let values = pg_query(&tx, &sql, params).await?;
                    tx.commit().await?;
                    values
                };
                call.complete = true;
                Ok(Rows::Postgres(values.into()))
            }
        }
    }

    pub async fn execute(&self, sql: &str, params: impl IntoParams) -> Result<u64> {
        match &self.0 {
            Backend::Libsql(conn) => Ok(conn.execute(sql, params).await?),
            Backend::Postgres(conn) => {
                let params = positional(params)?;
                let sql = placeholders(sql)?;
                let mut client = conn.client().await?;
                let mut call = PgCall::new(conn);
                let control = matches!(
                    sql.trim()
                        .trim_end_matches(';')
                        .to_ascii_uppercase()
                        .as_str(),
                    "COMMIT" | "ROLLBACK"
                );
                if control {
                    client.batch_execute(&sql).await?;
                    conn.active.store(false, Ordering::Release);
                    call.complete = true;
                    return Ok(0);
                }
                let result = if conn.session == PgSession::Transaction {
                    pg_execute(&*client, &sql, params).await
                } else {
                    let tx = client.transaction().await?;
                    pg_write_lock(&tx, &conn.config.schema).await?;
                    let count = pg_execute(&tx, &sql, params).await?;
                    tx.commit().await?;
                    Ok(count)
                };
                call.complete = result.is_ok();
                result
            }
        }
    }

    pub async fn execute_batch(&self, sql: &str) -> Result<()> {
        match &self.0 {
            Backend::Libsql(conn) => {
                conn.execute_batch(sql).await?;
            }
            Backend::Postgres(conn) => {
                let mut client = conn.client().await?;
                let mut call = PgCall::new(conn);
                if conn.session == PgSession::Transaction {
                    client.batch_execute(sql).await?;
                } else {
                    let tx = client.transaction().await?;
                    pg_write_lock(&tx, &conn.config.schema).await?;
                    tx.batch_execute(sql).await?;
                    tx.commit().await?;
                }
                call.complete = true;
            }
        }
        Ok(())
    }

    pub async fn transaction_with_behavior(
        &self,
        behavior: libsql::TransactionBehavior,
    ) -> anyhow::Result<Transaction> {
        match &self.0 {
            Backend::Libsql(conn) => {
                let native = conn.transaction_with_behavior(behavior).await?;
                let conn = Self::from((*native).clone());
                Ok(Transaction {
                    conn,
                    native: Some(native),
                })
            }
            Backend::Postgres(conn) => Ok(Transaction {
                conn: Self::connect_postgres(conn.config.clone(), PgSession::Transaction).await?,
                native: None,
            }),
        }
    }

    pub fn is_autocommit(&self) -> bool {
        match &self.0 {
            Backend::Libsql(conn) => conn.is_autocommit(),
            Backend::Postgres(conn) => !conn.active.load(Ordering::Acquire),
        }
    }

    pub async fn prepare(&self, sql: &str) -> Result<Statement> {
        Ok(Statement {
            conn: self.clone(),
            sql: sql.to_string(),
        })
    }
}

pub struct Transaction {
    conn: Connection,
    native: Option<libsql::Transaction>,
}

impl std::ops::Deref for Transaction {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if let Some(native) = self.native.take() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(native))).is_err() {
                tracing::warn!("Suppressed libSQL rollback panic while closing a transaction");
            }
        } else if !self.conn.is_autocommit()
            && let Backend::Postgres(connection) = &self.conn.0
        {
            connection.abort();
        }
    }
}

pub struct Statement {
    conn: Connection,
    sql: String,
}
impl Statement {
    pub async fn query(&self, params: impl IntoParams) -> Result<Rows> {
        self.conn.query(&self.sql, params).await
    }
    pub async fn execute(&self, params: impl IntoParams) -> Result<u64> {
        self.conn.execute(&self.sql, params).await
    }
}

pub enum Rows {
    Libsql(libsql::Rows),
    Postgres(VecDeque<Row>),
}
impl Rows {
    pub async fn next(&mut self) -> Result<Option<Row>> {
        match self {
            Self::Libsql(rows) => Ok(rows.next().await?.map(Row::Libsql)),
            Self::Postgres(rows) => Ok(rows.pop_front()),
        }
    }
}

pub enum Row {
    Libsql(libsql::Row),
    Postgres(Vec<Value>),
}
impl Row {
    pub fn get<T: ReadValue>(&self, index: i32) -> Result<T> {
        let value = match self {
            Self::Libsql(row) => row.get_value(index)?,
            Self::Postgres(values) => values
                .get(
                    usize::try_from(index)
                        .map_err(|_| Error::Value("negative column index".into()))?,
                )
                .ok_or_else(|| Error::Value("column index is out of range".into()))?
                .clone(),
        };
        T::read(value)
    }
}

pub trait ReadValue: Sized {
    fn read(value: Value) -> Result<Self>;
}
impl ReadValue for Value {
    fn read(value: Value) -> Result<Self> {
        Ok(value)
    }
}
impl<T: ReadValue> ReadValue for Option<T> {
    fn read(value: Value) -> Result<Self> {
        if matches!(value, Value::Null) {
            Ok(None)
        } else {
            T::read(value).map(Some)
        }
    }
}
macro_rules! integer_value {
    ($($kind:ty),*) => {$ (impl ReadValue for $kind {
        fn read(value: Value) -> Result<Self> {
            match value {
                Value::Integer(value) => <$kind>::try_from(value).map_err(|_| Error::Value("integer out of range".into())),
                _ => Err(Error::Value("expected an integer".into())),
            }
        }
    })*};
}
integer_value!(i32, i64, u32, u64, usize);
impl ReadValue for f64 {
    fn read(value: Value) -> Result<Self> {
        match value {
            Value::Real(v) => Ok(v),
            Value::Integer(v) => Ok(v as f64),
            _ => Err(Error::Value("expected a number".into())),
        }
    }
}
impl ReadValue for String {
    fn read(value: Value) -> Result<Self> {
        match value {
            Value::Text(v) => Ok(v),
            _ => Err(Error::Value("expected text".into())),
        }
    }
}
impl ReadValue for Vec<u8> {
    fn read(value: Value) -> Result<Self> {
        match value {
            Value::Blob(v) => Ok(v),
            _ => Err(Error::Value("expected bytes".into())),
        }
    }
}

fn positional(params: impl IntoParams) -> Result<Vec<Value>> {
    match params.into_params()? {
        Params::None => Ok(Vec::new()),
        Params::Positional(values) => Ok(values),
        Params::Named(_) => Err(Error::Value(
            "PostgreSQL queries require positional parameters".into(),
        )),
    }
}

#[derive(Debug)]
struct Parameter(Value);
impl ToSql for Parameter {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Send + Sync>> {
        match &self.0 {
            Value::Null => Ok(IsNull::Yes),
            Value::Integer(v) if *ty == Type::INT2 => i16::try_from(*v)?.to_sql(ty, out),
            Value::Integer(v) if *ty == Type::INT4 => i32::try_from(*v)?.to_sql(ty, out),
            Value::Integer(v) if *ty == Type::BOOL => (*v != 0).to_sql(ty, out),
            Value::Integer(v) => v.to_sql(ty, out),
            Value::Real(v) => v.to_sql(ty, out),
            Value::Text(v) if *ty == Type::BYTEA => v.as_bytes().to_sql(ty, out),
            Value::Text(v) => v.to_sql(ty, out),
            Value::Blob(v) => v.to_sql(ty, out),
        }
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    tokio_postgres::types::to_sql_checked!();
}

async fn statement(
    client: &impl tokio_postgres::GenericClient,
    sql: &str,
    _params: &[Parameter],
) -> Result<tokio_postgres::Statement> {
    Ok(client.prepare(sql).await?)
}

async fn pg_query(
    client: &impl tokio_postgres::GenericClient,
    sql: &str,
    params: Vec<Value>,
) -> Result<Vec<Row>> {
    let params: Vec<Parameter> = params.into_iter().map(Parameter).collect();
    let statement = statement(client, sql, &params).await?;
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
    client
        .query(&statement, &refs)
        .await?
        .into_iter()
        .map(|row| {
            (0..row.len())
                .map(|i| pg_value(&row, i))
                .collect::<Result<Vec<_>>>()
                .map(Row::Postgres)
        })
        .collect()
}

async fn pg_write_lock(client: &impl tokio_postgres::GenericClient, schema: &str) -> Result<()> {
    client
        .query_one(
            "SELECT pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtext($1),$2)",
            &[&schema, &WRITE_LOCK],
        )
        .await?;
    Ok(())
}

async fn pg_execute(
    client: &impl tokio_postgres::GenericClient,
    sql: &str,
    params: Vec<Value>,
) -> Result<u64> {
    let params: Vec<Parameter> = params.into_iter().map(Parameter).collect();
    let statement = statement(client, sql, &params).await?;
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
    Ok(client.execute(&statement, &refs).await?)
}

fn pg_value(row: &tokio_postgres::Row, i: usize) -> Result<Value> {
    let ty = row.columns()[i].type_();
    macro_rules! get {
        ($kind:ty, $variant:ident) => {
            row.try_get::<_, Option<$kind>>(i)?
                .map(Value::$variant)
                .unwrap_or(Value::Null)
        };
    }
    Ok(match *ty {
        Type::INT8 => get!(i64, Integer),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(i)?
            .map(|v| Value::Integer(v.into()))
            .unwrap_or(Value::Null),
        Type::INT2 => row
            .try_get::<_, Option<i16>>(i)?
            .map(|v| Value::Integer(v.into()))
            .unwrap_or(Value::Null),
        Type::BOOL => row
            .try_get::<_, Option<bool>>(i)?
            .map(|v| Value::Integer(i64::from(v)))
            .unwrap_or(Value::Null),
        Type::FLOAT8 => get!(f64, Real),
        Type::FLOAT4 => row
            .try_get::<_, Option<f32>>(i)?
            .map(|v| Value::Real(v.into()))
            .unwrap_or(Value::Null),
        Type::BYTEA => get!(Vec<u8>, Blob),
        Type::NUMERIC => row
            .try_get::<_, Option<Numeric>>(i)?
            .map(|v| v.0)
            .unwrap_or(Value::Null),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            get!(String, Text)
        }
        _ => {
            return Err(Error::Value(format!(
                "unsupported PostgreSQL result type {ty}"
            )));
        }
    })
}

struct Numeric(Value);
impl<'a> FromSql<'a> for Numeric {
    fn from_sql(
        _: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 8 || !raw.len().is_multiple_of(2) {
            return Err("invalid PostgreSQL numeric length".into());
        }
        let word = |i| i16::from_be_bytes([raw[i], raw[i + 1]]);
        let count = usize::try_from(word(0))?;
        let weight = i32::from(word(2));
        let sign = u16::from_be_bytes([raw[4], raw[5]]);
        let scale = word(6);
        if count * 2 + 8 != raw.len() || !matches!(sign, 0 | 0x4000) {
            return Err("invalid or non-finite PostgreSQL numeric".into());
        }
        let mut real = 0.0;
        let mut integer = 0i128;
        for i in 0..count {
            let digit = word(8 + i * 2);
            if !(0..10000).contains(&digit) {
                return Err("invalid PostgreSQL numeric digit".into());
            }
            let power = weight - i as i32;
            real += f64::from(digit) * 10000f64.powi(power);
            if scale == 0 && digit != 0 {
                let power = u32::try_from(power)?;
                integer = integer
                    .checked_add(
                        i128::from(digit)
                            .checked_mul(10000i128.checked_pow(power).ok_or("numeric overflow")?)
                            .ok_or("numeric overflow")?,
                    )
                    .ok_or("numeric overflow")?;
            }
        }
        if sign == 0x4000 {
            real = -real;
            integer = -integer;
        }
        Ok(Self(if scale == 0 {
            Value::Integer(i64::try_from(integer)?)
        } else {
            Value::Real(real)
        }))
    }
    fn accepts(ty: &Type) -> bool {
        *ty == Type::NUMERIC
    }
}

/// Convert positional placeholders while preserving SQL literals and comments.
fn placeholders(sql: &str) -> Result<String> {
    let mut output = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut next = 1usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let quote = c;
            output.push(c);
            i += 1;
            while i < chars.len() {
                let c = chars[i];
                output.push(c);
                i += 1;
                if c == quote {
                    if chars.get(i) == Some(&quote) {
                        output.push(quote);
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() {
                let c = chars[i];
                output.push(c);
                i += 1;
                if c == '\n' {
                    break;
                }
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            output.push_str("/*");
            i += 2;
            let mut depth = 1;
            while i < chars.len() && depth > 0 {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    output.push_str("/*");
                    i += 2;
                    depth += 1;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    output.push_str("*/");
                    i += 2;
                    depth -= 1;
                } else {
                    output.push(chars[i]);
                    i += 1;
                }
            }
        } else if c == '?' {
            i += 1;
            let start = i;
            while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
            let n = if start == i {
                next
            } else {
                chars[start..i]
                    .iter()
                    .collect::<String>()
                    .parse::<usize>()
                    .map_err(|_| Error::Value("invalid SQL parameter index".into()))?
            };
            if n == 0 {
                return Err(Error::Value("SQL parameter index starts at one".into()));
            }
            next = next.max(n + 1);
            output.push('$');
            output.push_str(&n.to_string());
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while chars
                .get(i)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            output.push_str(match word.to_ascii_lowercase().as_str() {
                "like" => "ILIKE",
                "group_concat" => "string_agg",
                "strftime" => "sashiko_strftime",
                "datetime" => "sashiko_datetime",
                "json_each" => "sashiko_json_ids",
                "json_set" => "sashiko_json_set",
                "json_extract" => "sashiko_json_flag",
                _ => &word,
            });
        } else {
            output.push(c);
            i += 1;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placeholders_preserve_literals_comments_and_repeated_parameters() {
        assert_eq!(
            placeholders("SELECT ?1, '?', \"?\", ?1, ? -- ?\n/* ? */").unwrap(),
            "SELECT $1, '?', \"?\", $1, $2 -- ?\n/* ? */"
        );
        assert!(placeholders("SELECT ?0").is_err());
    }

    #[test]
    fn query_vocabulary_preserves_quoted_names_and_nested_comments() {
        assert_eq!(
            placeholders("SELECT strftime('%s','now'), datetime(?, 'unixepoch'), group_concat(name, ','), json_each(?), json_set(data, '$.x', ?), json_extract(data, '$.x') WHERE title NOT LIKE ?").unwrap(),
            "SELECT sashiko_strftime('%s','now'), sashiko_datetime($1, 'unixepoch'), string_agg(name, ','), sashiko_json_ids($2), sashiko_json_set(data, '$.x', $3), sashiko_json_flag(data, '$.x') WHERE title NOT ILIKE $4"
        );
        assert_eq!(
            placeholders("SELECT \"like\", 'datetime ''?''', ?1 /* outer ? /* json_set */ ? */ -- json_each ?\n").unwrap(),
            "SELECT \"like\", 'datetime ''?''', $1 /* outer ? /* json_set */ ? */ -- json_each ?\n"
        );
    }

    fn decode_numeric(weight: i16, negative: bool, scale: i16, digits: &[i16]) -> Value {
        let header = [
            i16::try_from(digits.len()).unwrap(),
            weight,
            if negative { 0x4000 } else { 0 },
            scale,
        ];
        let bytes: Vec<_> = header
            .iter()
            .chain(digits)
            .flat_map(|v| v.to_be_bytes())
            .collect();
        Numeric::from_sql(&Type::NUMERIC, &bytes).unwrap().0
    }

    #[test]
    fn numeric_wire_values_preserve_integer_limits_signs_and_fractions() {
        assert_eq!(decode_numeric(0, false, 0, &[]), Value::Integer(0));
        assert_eq!(
            decode_numeric(1, true, 0, &[42, 1]),
            Value::Integer(-420001)
        );
        assert_eq!(
            decode_numeric(4, false, 0, &[922, 3372, 368, 5477, 5807]),
            Value::Integer(i64::MAX)
        );
        assert_eq!(
            decode_numeric(4, true, 0, &[922, 3372, 368, 5477, 5808]),
            Value::Integer(i64::MIN)
        );
        for (weight, digits, expected) in [(0, vec![12, 3456], 12.3456), (-1, vec![1], 0.0001)] {
            let Value::Real(actual) = decode_numeric(weight, false, 4, &digits) else {
                panic!("expected a fractional value")
            };
            assert!((actual - expected).abs() < 1e-12);
        }
        for words in [
            vec![0],
            vec![2, 0, 0, 0, 1],
            vec![1, 0, 0, 0, 10000],
            vec![0, 0, -16384, 0],
            vec![5, 4, 0, 0, 922, 3372, 368, 5477, 5808],
        ] {
            let bytes: Vec<_> = words.into_iter().flat_map(i16::to_be_bytes).collect();
            assert!(Numeric::from_sql(&Type::NUMERIC, &bytes).is_err());
        }
    }
}
