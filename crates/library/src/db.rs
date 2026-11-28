//! The catalog's SQL connection. Queries are written once, in the part of SQL
//! that SQLite and Postgres share, so the same code can run against this
//! machine's SQLite file or a catalog shared between machines (#69):
//!
//! - parameters are `$1`, `$2`, … (each may appear more than once);
//! - upserts are `INSERT … ON CONFLICT (…) DO UPDATE SET x = excluded.x` or
//!   `DO NOTHING`, never `INSERT OR REPLACE`;
//! - new ids come back from `RETURNING id`, never `last_insert_rowid()`;
//! - case-insensitive matching is `lower(x) = lower($1)`, never `COLLATE NOCASE`;
//! - integers are 64-bit, reals are doubles, text is text: [`Value`].
//!
//! Only table definitions differ between the two, and stay with each backend.

use std::cell::RefCell;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

/// One SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

/// What can be passed as a parameter.
pub trait ToValue {
    fn to_value(&self) -> Value;
}

impl ToValue for Value {
    fn to_value(&self) -> Value {
        self.clone()
    }
}
impl ToValue for i64 {
    fn to_value(&self) -> Value {
        Value::Int(*self)
    }
}
impl ToValue for i32 {
    fn to_value(&self) -> Value {
        Value::Int(i64::from(*self))
    }
}
impl ToValue for u32 {
    fn to_value(&self) -> Value {
        Value::Int(i64::from(*self))
    }
}
impl ToValue for u64 {
    /// Sizes and the like; past `i64::MAX` they wrap, as SQLite stored them.
    fn to_value(&self) -> Value {
        Value::Int(*self as i64)
    }
}
impl ToValue for f64 {
    fn to_value(&self) -> Value {
        Value::Real(*self)
    }
}
impl ToValue for bool {
    fn to_value(&self) -> Value {
        Value::Int(i64::from(*self))
    }
}
impl ToValue for str {
    fn to_value(&self) -> Value {
        Value::Text(self.to_owned())
    }
}
impl ToValue for String {
    fn to_value(&self) -> Value {
        Value::Text(self.clone())
    }
}
impl ToValue for std::borrow::Cow<'_, str> {
    fn to_value(&self) -> Value {
        Value::Text(self.to_string())
    }
}
impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}
impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Value {
        self.as_ref().map_or(Value::Null, ToValue::to_value)
    }
}

/// What a column can be read as.
pub trait FromValue: Sized {
    fn from_value(v: &Value) -> Result<Self>;
}

impl FromValue for Value {
    fn from_value(v: &Value) -> Result<Self> {
        Ok(v.clone())
    }
}
impl FromValue for i64 {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Int(i) => Ok(*i),
            other => bail!("expected an integer, got {other:?}"),
        }
    }
}
impl FromValue for u64 {
    fn from_value(v: &Value) -> Result<Self> {
        Ok(i64::from_value(v)? as u64)
    }
}
impl FromValue for u32 {
    fn from_value(v: &Value) -> Result<Self> {
        Ok(u32::try_from(i64::from_value(v)?)?)
    }
}
impl FromValue for f64 {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Real(r) => Ok(*r),
            Value::Int(i) => Ok(*i as f64),
            other => bail!("expected a number, got {other:?}"),
        }
    }
}
impl FromValue for bool {
    fn from_value(v: &Value) -> Result<Self> {
        Ok(i64::from_value(v)? != 0)
    }
}
impl FromValue for String {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Text(t) => Ok(t.clone()),
            other => bail!("expected text, got {other:?}"),
        }
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Null => Ok(None),
            v => T::from_value(v).map(Some),
        }
    }
}

/// One result row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row(Vec<Value>);

impl Row {
    /// Column `i` (from 0).
    pub fn get<T: FromValue>(&self, i: usize) -> Result<T> {
        let v = self
            .0
            .get(i)
            .with_context(|| format!("no column {i} in a row of {}", self.0.len()))?;
        T::from_value(v).with_context(|| format!("column {i}"))
    }
}

/// A connection: a SQLite file, or a Postgres server.
pub struct Db {
    kind: Kind,
    /// Why writes are refused, when they are.
    read_only: Option<String>,
}

enum Kind {
    Lite(rusqlite::Connection),
    /// The client (`None` only while being dropped, when it goes back to
    /// the pool), the pool, and its key there.
    Pg(Box<RefCell<Option<postgres::Client>>>, Pool, String),
}

/// Where a Postgres client goes back to when its [`Db`] is dropped.
pub(crate) type Pool = &'static Mutex<Vec<(String, postgres::Client)>>;

impl Db {
    /// A SQLite file, made if it isn't there.
    pub fn sqlite(path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        Ok(Self {
            kind: Kind::Lite(conn),
            read_only: None,
        })
    }

    /// A SQLite file opened only for reading; writes are refused with `why`.
    pub fn sqlite_read_only(path: &Path, why: &str) -> Result<Self> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            kind: Kind::Lite(conn),
            read_only: Some(why.to_owned()),
        })
    }

    /// A Postgres connection, which goes back to `pool` under `key` when
    /// this is dropped (unless it's broken or mid-transaction).
    pub(crate) fn postgres(client: postgres::Client, pool: Pool, key: String) -> Self {
        Self {
            kind: Kind::Pg(Box::new(RefCell::new(Some(client))), pool, key),
            read_only: None,
        }
    }

    /// Why writes are refused, if they are.
    pub fn read_only(&self) -> Option<&str> {
        self.read_only.as_deref()
    }

    pub fn is_postgres(&self) -> bool {
        matches!(self.kind, Kind::Pg(..))
    }

    /// The SQLite connection, for what only SQLite has (`PRAGMA`s, table
    /// definitions, `VACUUM INTO`).
    pub(crate) fn sqlite_conn(&self) -> Result<&rusqlite::Connection> {
        match &self.kind {
            Kind::Lite(conn) => Ok(conn),
            Kind::Pg(..) => bail!("this needs the catalog in a SQLite file, not a shared one"),
        }
    }

    fn writable(&self) -> Result<()> {
        match &self.read_only {
            Some(why) => bail!("{why}"),
            None => Ok(()),
        }
    }

    /// Runs `f` with the Postgres client.
    fn pg<T>(
        cell: &RefCell<Option<postgres::Client>>,
        f: impl FnOnce(&mut postgres::Client) -> Result<T>,
    ) -> Result<T> {
        let mut client = cell.borrow_mut();
        f(client
            .as_mut()
            .context("the catalog connection is closed")?)
    }

    /// Runs statements with no parameters (table definitions, `BEGIN`).
    pub fn batch(&self, sql: &str) -> Result<()> {
        match &self.kind {
            Kind::Lite(conn) => Ok(conn.execute_batch(sql)?),
            Kind::Pg(cell, ..) => Self::pg(cell, |c| Ok(c.batch_execute(sql)?)),
        }
    }

    /// Runs one statement; returns how many rows it changed.
    pub fn execute(&self, sql: &str, params: &[&dyn ToValue]) -> Result<usize> {
        self.writable()?;
        match &self.kind {
            Kind::Lite(conn) => {
                let mut stmt = conn.prepare_cached(&sqlite_sql(sql))?;
                Ok(stmt.execute(rusqlite::params_from_iter(lite_params(params)))?)
            }
            Kind::Pg(cell, ..) => Self::pg(cell, |c| {
                let values: Vec<Value> = params.iter().map(|p| p.to_value()).collect();
                let refs: Vec<&(dyn postgres::types::ToSql + Sync)> = values
                    .iter()
                    .map(|v| v as &(dyn postgres::types::ToSql + Sync))
                    .collect();
                let stmt = c.prepare(sql).with_context(|| sql_context(sql))?;
                Ok(c.execute(&stmt, &refs).with_context(|| sql_context(sql))? as usize)
            }),
        }
    }

    /// Every row a query returns. (A query that writes, `INSERT … RETURNING`,
    /// is refused when the connection is read-only, as SQLite would.)
    pub fn query(&self, sql: &str, params: &[&dyn ToValue]) -> Result<Vec<Row>> {
        let conn = match &self.kind {
            Kind::Lite(conn) => conn,
            Kind::Pg(cell, ..) => {
                if writes(sql) {
                    self.writable()?;
                }
                return Self::pg(cell, |c| {
                    let values: Vec<Value> = params.iter().map(|p| p.to_value()).collect();
                    let refs: Vec<&(dyn postgres::types::ToSql + Sync)> = values
                        .iter()
                        .map(|v| v as &(dyn postgres::types::ToSql + Sync))
                        .collect();
                    let stmt = c.prepare(sql).with_context(|| sql_context(sql))?;
                    c.query(&stmt, &refs)
                        .with_context(|| sql_context(sql))?
                        .iter()
                        .map(|r| {
                            (0..r.len())
                                .map(|i| Ok(r.try_get::<_, Value>(i)?))
                                .collect::<Result<Vec<_>>>()
                                .map(Row)
                        })
                        .collect()
                });
            }
        };
        if writes(sql) {
            self.writable()?;
        }
        let mut stmt = conn.prepare_cached(&sqlite_sql(sql))?;
        let n = stmt.column_count();
        let rows = stmt.query_map(rusqlite::params_from_iter(lite_params(params)), |r| {
            (0..n)
                .map(|i| {
                    Ok(match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(i) => Value::Int(i),
                        rusqlite::types::ValueRef::Real(f) => Value::Real(f),
                        rusqlite::types::ValueRef::Text(t) => {
                            Value::Text(String::from_utf8_lossy(t).into_owned())
                        }
                        rusqlite::types::ValueRef::Blob(b) => {
                            Value::Text(String::from_utf8_lossy(b).into_owned())
                        }
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()
                .map(Row)
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The first row a query returns, if any.
    pub fn query_opt(&self, sql: &str, params: &[&dyn ToValue]) -> Result<Option<Row>> {
        Ok(self.query(sql, params)?.into_iter().next())
    }

    /// The first column of the first row, which must be there.
    pub fn query_one<T: FromValue>(&self, sql: &str, params: &[&dyn ToValue]) -> Result<T> {
        self.query_opt(sql, params)?
            .context("the query returned no row")?
            .get(0)
    }

    /// Runs `f` in a transaction: committed if it returns `Ok`, rolled back
    /// if it fails.
    pub fn transaction<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.writable()?;
        self.batch("BEGIN")?;
        match f() {
            Ok(v) => {
                self.batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Kind::Pg(cell, pool, key) = &self.kind
            && let Some(mut client) = cell.borrow_mut().take()
            && !client.is_closed()
            && client.batch_execute("ROLLBACK").is_ok()
        {
            // ROLLBACK outside a transaction is only a warning: it checks
            // the connection still works, and ends anything left open.
            let mut pool = pool.lock().unwrap_or_else(|e| e.into_inner());
            if pool.len() < POOL_SIZE {
                pool.push((key.clone(), client));
            }
        }
    }
}

/// How many idle Postgres connections are kept for the next [`Db`].
pub(crate) const POOL_SIZE: usize = 4;

/// Whether a statement writes (so a read-only connection refuses it).
fn writes(sql: &str) -> bool {
    let first = sql.split_whitespace().next().unwrap_or("");
    !first.eq_ignore_ascii_case("SELECT") && !first.eq_ignore_ascii_case("WITH")
}

/// The first line of a statement, to say which one failed.
fn sql_context(sql: &str) -> String {
    let line = sql.trim().lines().next().unwrap_or("");
    format!("running `{line}`")
}

impl postgres::types::ToSql for Value {
    fn to_sql(
        &self,
        ty: &postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> std::result::Result<postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    {
        use postgres::types::{IsNull, Type};
        let t = ty.clone();
        match self {
            Value::Null => Ok(IsNull::Yes),
            Value::Int(i) => match t {
                Type::INT8 => i.to_sql(ty, out),
                Type::INT4 => i32::try_from(*i)?.to_sql(ty, out),
                Type::INT2 => i16::try_from(*i)?.to_sql(ty, out),
                Type::FLOAT8 => (*i as f64).to_sql(ty, out),
                Type::FLOAT4 => (*i as f32).to_sql(ty, out),
                Type::BOOL => (*i != 0).to_sql(ty, out),
                _ => i.to_string().to_sql(ty, out),
            },
            Value::Real(r) => match t {
                Type::FLOAT4 => (*r as f32).to_sql(ty, out),
                Type::FLOAT8 => r.to_sql(ty, out),
                _ => Err(format!("a number can't go in a {ty} column").into()),
            },
            Value::Text(s) => s.as_str().to_sql(ty, out),
        }
    }

    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }

    postgres::types::to_sql_checked!();
}

impl<'a> postgres::types::FromSql<'a> for Value {
    fn from_sql(
        ty: &postgres::types::Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        use postgres::types::Type;
        Ok(match ty.clone() {
            Type::INT8 => Value::Int(i64::from_sql(ty, raw)?),
            Type::INT4 => Value::Int(i64::from(i32::from_sql(ty, raw)?)),
            Type::INT2 => Value::Int(i64::from(i16::from_sql(ty, raw)?)),
            Type::BOOL => Value::Int(i64::from(bool::from_sql(ty, raw)?)),
            Type::FLOAT8 => Value::Real(f64::from_sql(ty, raw)?),
            Type::FLOAT4 => Value::Real(f64::from(f32::from_sql(ty, raw)?)),
            _ => Value::Text(String::from_sql(ty, raw)?),
        })
    }

    fn from_sql_null(
        _: &postgres::types::Type,
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Value::Null)
    }

    /// Anything: what isn't a number is read as text (`void`, from a
    /// function called for what it does, as empty text).
    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
}

/// `$1` → `?1`: SQLite reads `$1` as a parameter *named* `$1`, numbered by
/// first appearance, so `$2 … $1` would bind the wrong way round; `?1` is by
/// number. Text in quotes is left alone.
fn sqlite_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut quoted = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            quoted = !quoted;
        }
        if c == '$' && !quoted && chars.peek().is_some_and(char::is_ascii_digit) {
            out.push('?');
        } else {
            out.push(c);
        }
    }
    out
}

fn lite_params(params: &[&dyn ToValue]) -> Vec<rusqlite::types::Value> {
    params
        .iter()
        .map(|p| match p.to_value() {
            Value::Null => rusqlite::types::Value::Null,
            Value::Int(i) => rusqlite::types::Value::Integer(i),
            Value::Real(r) => rusqlite::types::Value::Real(r),
            Value::Text(t) => rusqlite::types::Value::Text(t),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_parameters_bind_by_number_and_values_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::sqlite(&dir.path().join("t.sqlite3")).unwrap();
        db.sqlite_conn()
            .unwrap()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b REAL, c INTEGER)")
            .unwrap();
        let id: i64 = db
            .query_one(
                "INSERT INTO t (a, b, c) VALUES ($2, $3, $1) RETURNING id",
                &[&7i64, &"it's $1", &1.5f64],
            )
            .unwrap();
        db.execute(
            "INSERT INTO t (id, a, b, c) VALUES ($1, $2, NULL, NULL)
             ON CONFLICT (id) DO UPDATE SET a = excluded.a || $2",
            &[&id, &"x"],
        )
        .unwrap();
        let row = db
            .query_opt("SELECT a, b, c, NULL FROM t WHERE id = $1", &[&id])
            .unwrap()
            .unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "xx");
        assert_eq!(row.get::<f64>(1).unwrap(), 1.5);
        assert_eq!(row.get::<u64>(2).unwrap(), 7);
        assert_eq!(row.get::<Option<String>>(3).unwrap(), None);
        assert!(row.get::<String>(2).is_err());
        assert_eq!(
            sqlite_sql("SELECT '$1', $1, $12, $"),
            "SELECT '$1', ?1, ?12, $"
        );
    }

    #[test]
    fn a_failed_transaction_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::sqlite(&dir.path().join("t.sqlite3")).unwrap();
        db.sqlite_conn()
            .unwrap()
            .execute_batch("CREATE TABLE t (a TEXT)")
            .unwrap();
        let failed: Result<()> = db.transaction(|| {
            db.execute("INSERT INTO t VALUES ($1)", &[&"a"])?;
            bail!("no")
        });
        assert!(failed.is_err());
        db.transaction(|| db.execute("INSERT INTO t VALUES ($1)", &[&"b"]))
            .unwrap();
        let rows = db.query("SELECT a FROM t", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String>(0).unwrap(), "b");
    }
}
