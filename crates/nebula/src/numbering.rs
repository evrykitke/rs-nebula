//! Document number series — gap-free, human-readable identifiers for the
//! documents an ERP issues: invoices, credit notes, bills, sales orders.
//!
//! Financial documents must usually be numbered **without gaps** (a legal
//! requirement for VAT invoices in most jurisdictions), so a number is
//! never handed out speculatively. Allocation happens inside the caller's
//! own database transaction — the same one that persists the document —
//! against a per-series counter row. The `INSERT … ON CONFLICT … RETURNING`
//! locks that row for the life of the transaction, so concurrent callers
//! serialize on it and, crucially, a rollback un-increments the counter:
//! a document that never commits never burns a number.
//!
//! Modules declare their series in code (like permissions) and the kernel
//! builds one [`Numbering`] registry, shared with handlers as a request
//! extension. A series' format is a template with tokens — `{YYYY}`,
//! `{YY}`, `{MM}` and exactly one `{SEQ}` / `{SEQ:width}` — so
//! `"INV-{YYYY}-{SEQ:5}"` renders `INV-2026-00042`.
//!
//! Configuration is opt-in. The code declaration is the **system
//! default**, so a business that never configures anything still gets
//! working numbers on the fly. A tenant that *does* care can override a
//! series' template and reset policy with [`Numbering::set_override`]; the
//! override lives in that tenant's database and every allocation resolves
//! default-then-override, so one tenant's format never affects another's.
//! An invalid override degrades to the default rather than blocking
//! document creation — a cache-like "never a source of truth" stance for
//! formatting.
//!
//! ```ignore
//! // in a module's configure():
//! ctx.declare_series(SeriesDef::new(
//!     "sales.invoice", "Sales Invoice", "INV-{YYYY}-{SEQ:5}", Reset::Yearly,
//! )?);
//!
//! // in a handler, allocating inside the document's own transaction:
//! let txn = db.begin().await?;
//! let number = numbering.next(&txn, "sales.invoice").await?;
//! // persist the document carrying `number.formatted` on the SAME txn
//! txn.commit().await?; // number and document land together, or neither
//! ```

use crate::error::{Error, Result};
use crate::time::SharedClock;
use chrono::{DateTime, Datelike, Utc};
use sea_orm::{ConnectionTrait, Statement};
use std::collections::HashMap;
use std::sync::Arc;

/// When a series' sequence restarts from one. The period is folded into
/// the counter key, so each period counts independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reset {
    /// One continuous sequence for all time.
    Never,
    /// Restart each calendar year (the common choice for invoices).
    Yearly,
    /// Restart each calendar month.
    Monthly,
}

impl Reset {
    /// The stored form, used in the overrides table.
    pub fn as_str(self) -> &'static str {
        match self {
            Reset::Never => "never",
            Reset::Yearly => "yearly",
            Reset::Monthly => "monthly",
        }
    }

    /// Parse the stored form. Unknown values answer `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "never" => Some(Reset::Never),
            "yearly" => Some(Reset::Yearly),
            "monthly" => Some(Reset::Monthly),
            _ => None,
        }
    }
}

/// A compiled template fragment.
#[derive(Debug, Clone)]
enum Token {
    Literal(String),
    /// Four-digit year, e.g. `2026`.
    Year4,
    /// Two-digit year, e.g. `26`.
    Year2,
    /// Two-digit month, e.g. `07`.
    Month,
    /// The sequence number, zero-padded to the given width (0 = no
    /// padding).
    Seq(usize),
}

/// The definition of a document series: its stable key, a display name,
/// the number format and when the sequence resets. Declared by a module;
/// the template is validated on construction so a typo fails at the call
/// site rather than at runtime.
#[derive(Debug, Clone)]
pub struct SeriesDef {
    key: String,
    name: String,
    template: String,
    reset: Reset,
    tokens: Vec<Token>,
}

impl SeriesDef {
    /// Define a series. `key` is a stable dotted identifier
    /// (`sales.invoice`); `template` must contain exactly one `{SEQ}` or
    /// `{SEQ:width}` token and may use `{YYYY}`, `{YY}` and `{MM}`.
    pub fn new(
        key: impl Into<String>,
        name: impl Into<String>,
        template: impl Into<String>,
        reset: Reset,
    ) -> Result<Self> {
        let key = key.into();
        let ok = !key.is_empty()
            && key.len() <= 64
            && key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'));
        if !ok {
            return Err(Error::Validation(format!(
                "a series key must be 1-64 lowercase letters, digits, dots, dashes or underscores, got {key:?}"
            )));
        }
        let template = template.into();
        let tokens = compile(&template)?;
        Ok(Self {
            key,
            name: name.into(),
            template,
            reset,
            tokens,
        })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn template(&self) -> &str {
        &self.template
    }

    pub fn reset(&self) -> Reset {
        self.reset
    }
}

/// A freshly allocated document number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Number {
    /// The rendered number to put on the document, e.g. `INV-2026-00042`.
    pub formatted: String,
    /// The raw sequence value (`42`) — useful as a sortable column
    /// alongside the formatted string.
    pub sequence: i64,
    /// The period the sequence belongs to (`2026`, `2026-07`, or `-` for
    /// [`Reset::Never`]).
    pub period: String,
}

/// A late-binding handle to the [`Numbering`] registry, for code that is
/// wired during `configure` but runs after boot — event subscribers and
/// background workers. The registry itself cannot exist while modules are
/// still declaring series, so [`ModuleContext::numbering`] hands out this
/// handle instead; the kernel installs the built registry into it, and
/// [`NumberingHandle::get`] cashes it in at runtime.
///
/// [`ModuleContext::numbering`]: crate::module::ModuleContext::numbering
#[derive(Clone, Default)]
pub struct NumberingHandle {
    slot: Arc<std::sync::OnceLock<Numbering>>,
}

impl NumberingHandle {
    pub(crate) fn install(&self, numbering: Numbering) {
        // A second install (two kernels sharing a context never happens;
        // belt and braces) keeps the first registry.
        let _ = self.slot.set(numbering);
    }

    /// The registry. Errors before boot has completed — subscribers and
    /// workers only run afterwards, so this is a wiring bug, not a race.
    pub fn get(&self) -> Result<Numbering> {
        self.slot.get().cloned().ok_or_else(|| {
            Error::internal(
                "the numbering registry is not built until boot completes; \
                 use the handle from event handlers or workers, not during configure",
            )
        })
    }
}

impl std::fmt::Debug for NumberingHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NumberingHandle")
            .field("installed", &self.slot.get().is_some())
            .finish()
    }
}

/// The document-numbering registry: every series declared by the
/// application's modules, plus the clock that dates each number. Cheap to
/// clone (shares one `Arc`); created by the kernel, one per application.
#[derive(Clone)]
pub struct Numbering {
    inner: Arc<Inner>,
}

struct Inner {
    series: HashMap<String, SeriesDef>,
    clock: SharedClock,
}

impl std::fmt::Debug for Numbering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Numbering")
            .field("series", &self.inner.series.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Numbering {
    /// Build the registry from the modules' declarations. A duplicate key
    /// fails the boot — two modules must not own the same series.
    pub(crate) fn build(defs: Vec<SeriesDef>, clock: SharedClock) -> Result<Self> {
        let mut series = HashMap::with_capacity(defs.len());
        for def in defs {
            if series.contains_key(&def.key) {
                return Err(Error::internal(format!(
                    "document series {:?} is declared twice",
                    def.key
                )));
            }
            series.insert(def.key.clone(), def);
        }
        Ok(Self {
            inner: Arc::new(Inner { series, clock }),
        })
    }

    /// The number of declared series.
    pub fn len(&self) -> usize {
        self.inner.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.series.is_empty()
    }

    /// Look up a series definition.
    pub fn series(&self, key: &str) -> Option<&SeriesDef> {
        self.inner.series.get(key)
    }

    /// Allocate the next number for a series. Pass the **transaction that
    /// writes the document**: the counter increment commits (or rolls
    /// back) with it, which is what makes the sequence gap-free. Every
    /// concurrent allocation of the same series waits on the counter row,
    /// so numbers are handed out in order and never duplicated.
    ///
    /// The format is resolved on `conn`: a tenant override in this
    /// database wins, otherwise the module's declared default is used.
    pub async fn next<C: ConnectionTrait>(&self, conn: &C, key: &str) -> Result<Number> {
        let def = self.resolve(conn, key).await?;
        let now = self.inner.clock.now();
        let period = period_key(def.reset, now);
        let sequence = allocate(conn, key, &period).await?;
        Ok(Number {
            formatted: render(&def.tokens, sequence, now),
            sequence,
            period,
        })
    }

    /// The number the next allocation *would* produce, without consuming
    /// it. For previews only — it takes no lock, so a concurrent [`next`]
    /// can make it stale the moment it returns.
    ///
    /// [`next`]: Numbering::next
    pub async fn peek<C: ConnectionTrait>(&self, conn: &C, key: &str) -> Result<String> {
        let def = self.resolve(conn, key).await?;
        let now = self.inner.clock.now();
        let period = period_key(def.reset, now);
        let current = current_value(conn, key, &period).await?;
        Ok(render(&def.tokens, current + 1, now))
    }

    /// Override a series' format for this database (a tenant's own
    /// configuration). Validates the template and that the series is
    /// declared, then upserts the override; from now on [`next`] on this
    /// connection uses it. This is the hook a settings screen — or a
    /// module setting up its own document type — writes through.
    ///
    /// [`next`]: Numbering::next
    pub async fn set_override<C: ConnectionTrait>(
        &self,
        conn: &C,
        key: &str,
        template: &str,
        reset: Reset,
    ) -> Result<()> {
        self.def(key)?;
        compile(template)?;
        upsert_override(conn, key, template, reset).await
    }

    /// Remove a series' override on this connection, reverting it to the
    /// module's declared default.
    pub async fn clear_override<C: ConnectionTrait>(&self, conn: &C, key: &str) -> Result<()> {
        self.def(key)?;
        delete_override(conn, key).await
    }

    /// The effective definition for a series on `conn` — the default with
    /// any override applied. For settings screens that show what is
    /// currently in force.
    pub async fn effective<C: ConnectionTrait>(&self, conn: &C, key: &str) -> Result<SeriesDef> {
        self.resolve(conn, key).await
    }

    /// Resolve the definition in force on `conn`: the declared default,
    /// with a valid override layered on top. A malformed override is
    /// logged and ignored so document creation never breaks on bad
    /// configuration.
    async fn resolve<C: ConnectionTrait>(&self, conn: &C, key: &str) -> Result<SeriesDef> {
        let default = self.def(key)?;
        let Some((template, reset_raw)) = load_override(conn, key).await? else {
            return Ok(default.clone());
        };
        let effective = Reset::parse(&reset_raw)
            .ok_or_else(|| Error::Validation(format!("unknown reset {reset_raw:?}")))
            .and_then(|reset| SeriesDef::new(&default.key, &default.name, &template, reset));
        match effective {
            Ok(def) => Ok(def),
            Err(e) => {
                tracing::warn!(series = %key, error = %e,
                    "invalid document series override; using the declared default");
                Ok(default.clone())
            }
        }
    }

    /// A missing series is a programming error (like an undefined
    /// permission): a module tried to number a document type nobody
    /// declared.
    fn def(&self, key: &str) -> Result<&SeriesDef> {
        self.inner.series.get(key).ok_or_else(|| {
            Error::internal(format!(
                "document series {key:?} is not declared by any module"
            ))
        })
    }
}

/// The counter key for the current period. A non-empty sentinel is used
/// for [`Reset::Never`] so the primary-key column is never blank.
fn period_key(reset: Reset, now: DateTime<Utc>) -> String {
    match reset {
        Reset::Never => "-".to_string(),
        Reset::Yearly => format!("{:04}", now.year()),
        Reset::Monthly => format!("{:04}-{:02}", now.year(), now.month()),
    }
}

/// Increment (or create) the counter row and return the new value, all in
/// one statement so it is atomic and holds the row lock until the caller's
/// transaction ends. Postgres-specific, like the rest of the schema.
async fn allocate<C: ConnectionTrait>(conn: &C, key: &str, period: &str) -> Result<i64> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO document_number_counters (series_key, period, current_value, updated_at) \
         VALUES ($1, $2, 1, now()) \
         ON CONFLICT (series_key, period) \
         DO UPDATE SET current_value = document_number_counters.current_value + 1, updated_at = now() \
         RETURNING current_value",
        [key.to_owned().into(), period.to_owned().into()],
    );
    let row = conn
        .query_one(stmt)
        .await?
        .ok_or_else(|| Error::internal("document number counter upsert returned no row"))?;
    row.try_get::<i64>("", "current_value").map_err(Error::from)
}

/// The current counter value for a period, or zero if none exists yet.
async fn current_value<C: ConnectionTrait>(conn: &C, key: &str, period: &str) -> Result<i64> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "SELECT current_value FROM document_number_counters \
         WHERE series_key = $1 AND period = $2",
        [key.to_owned().into(), period.to_owned().into()],
    );
    match conn.query_one(stmt).await? {
        Some(row) => row.try_get::<i64>("", "current_value").map_err(Error::from),
        None => Ok(0),
    }
}

/// The override `(template, reset)` for a series on this connection, if any.
async fn load_override<C: ConnectionTrait>(
    conn: &C,
    key: &str,
) -> Result<Option<(String, String)>> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "SELECT template, reset FROM document_series WHERE series_key = $1",
        [key.to_owned().into()],
    );
    match conn.query_one(stmt).await? {
        Some(row) => {
            let template = row.try_get::<String>("", "template")?;
            let reset = row.try_get::<String>("", "reset")?;
            Ok(Some((template, reset)))
        }
        None => Ok(None),
    }
}

async fn upsert_override<C: ConnectionTrait>(
    conn: &C,
    key: &str,
    template: &str,
    reset: Reset,
) -> Result<()> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO document_series (series_key, template, reset, updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (series_key) \
         DO UPDATE SET template = $2, reset = $3, updated_at = now()",
        [
            key.to_owned().into(),
            template.to_owned().into(),
            reset.as_str().to_owned().into(),
        ],
    );
    conn.execute(stmt).await?;
    Ok(())
}

async fn delete_override<C: ConnectionTrait>(conn: &C, key: &str) -> Result<()> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "DELETE FROM document_series WHERE series_key = $1",
        [key.to_owned().into()],
    );
    conn.execute(stmt).await?;
    Ok(())
}

/// Parse a template into fragments, validating that it has exactly one
/// sequence token and no unknown ones.
fn compile(template: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut chars = template.chars().peekable();
    let mut seq_count = 0;
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if !literal.is_empty() {
                    tokens.push(Token::Literal(std::mem::take(&mut literal)));
                }
                let mut inner = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(c);
                }
                if !closed {
                    return Err(Error::Validation(format!(
                        "unterminated '{{' in template {template:?}"
                    )));
                }
                let token = parse_token(&inner)?;
                if matches!(token, Token::Seq(_)) {
                    seq_count += 1;
                }
                tokens.push(token);
            }
            '}' => {
                return Err(Error::Validation(format!(
                    "unexpected '}}' in template {template:?}"
                )));
            }
            _ => literal.push(c),
        }
    }
    if !literal.is_empty() {
        tokens.push(Token::Literal(literal));
    }
    if seq_count != 1 {
        return Err(Error::Validation(format!(
            "template {template:?} must contain exactly one {{SEQ}} token, found {seq_count}"
        )));
    }
    Ok(tokens)
}

fn parse_token(inner: &str) -> Result<Token> {
    match inner {
        "YYYY" => Ok(Token::Year4),
        "YY" => Ok(Token::Year2),
        "MM" => Ok(Token::Month),
        "SEQ" => Ok(Token::Seq(0)),
        _ => match inner.strip_prefix("SEQ:") {
            Some(width) => {
                let width: usize = width
                    .parse()
                    .map_err(|_| Error::Validation(format!("invalid SEQ width in {{{inner}}}")))?;
                if !(1..=18).contains(&width) {
                    return Err(Error::Validation(format!(
                        "SEQ width must be 1-18, got {width}"
                    )));
                }
                Ok(Token::Seq(width))
            }
            None => Err(Error::Validation(format!("unknown template token {{{inner}}}"))),
        },
    }
}

fn render(tokens: &[Token], sequence: i64, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Literal(s) => out.push_str(s),
            Token::Year4 => out.push_str(&format!("{:04}", now.year())),
            Token::Year2 => out.push_str(&format!("{:02}", now.year().rem_euclid(100))),
            Token::Month => out.push_str(&format!("{:02}", now.month())),
            Token::Seq(0) => out.push_str(&sequence.to_string()),
            Token::Seq(width) => out.push_str(&format!("{sequence:0width$}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 12, 0, 0).unwrap()
    }

    fn tokens(template: &str) -> Vec<Token> {
        compile(template).expect("valid template")
    }

    #[test]
    fn renders_year_month_and_padded_sequence() {
        let now = at(2026, 7, 9);
        assert_eq!(render(&tokens("INV-{YYYY}-{SEQ:5}"), 42, now), "INV-2026-00042");
        assert_eq!(render(&tokens("CN{YY}{MM}-{SEQ:4}"), 42, now), "CN2607-0042");
        assert_eq!(render(&tokens("SO-{SEQ:6}"), 42, now), "SO-000042");
        assert_eq!(render(&tokens("{SEQ}"), 42, now), "42");
    }

    #[test]
    fn sequence_wider_than_width_is_not_truncated() {
        let now = at(2026, 1, 1);
        assert_eq!(render(&tokens("{SEQ:3}"), 12345, now), "12345");
    }

    #[test]
    fn rejects_templates_without_exactly_one_sequence() {
        assert!(SeriesDef::new("a", "A", "INV-{YYYY}", Reset::Yearly).is_err());
        assert!(SeriesDef::new("a", "A", "{SEQ}-{SEQ}", Reset::Never).is_err());
    }

    #[test]
    fn rejects_malformed_templates() {
        assert!(SeriesDef::new("a", "A", "INV-{YY", Reset::Never).is_err());
        assert!(SeriesDef::new("a", "A", "INV-}{SEQ}", Reset::Never).is_err());
        assert!(SeriesDef::new("a", "A", "{NOPE}-{SEQ}", Reset::Never).is_err());
        assert!(SeriesDef::new("a", "A", "{SEQ:0}", Reset::Never).is_err());
        assert!(SeriesDef::new("a", "A", "{SEQ:x}", Reset::Never).is_err());
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(SeriesDef::new("", "A", "{SEQ}", Reset::Never).is_err());
        assert!(SeriesDef::new("Sales.Invoice", "A", "{SEQ}", Reset::Never).is_err());
        assert!(SeriesDef::new("sales invoice", "A", "{SEQ}", Reset::Never).is_err());
        assert!(SeriesDef::new("sales.invoice", "A", "{SEQ}", Reset::Never).is_ok());
    }

    #[test]
    fn period_key_reflects_the_reset_policy() {
        let now = at(2026, 7, 9);
        assert_eq!(period_key(Reset::Never, now), "-");
        assert_eq!(period_key(Reset::Yearly, now), "2026");
        assert_eq!(period_key(Reset::Monthly, now), "2026-07");
    }
}
