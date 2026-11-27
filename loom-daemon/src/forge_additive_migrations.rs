//! `loom-daemon forge additive-migrations <pr>` (#13102) — the fail-closed decision behind
//! Champion's opt-in `additiveMigrations` standing-authorization class key.
//!
//! # What the question is
//!
//! `*migrations/*` is in Champion's kept set, so every D1 migration used to be a critical-file
//! hold needing an operator release. The operator ruled (2026-10-23) that purely *additive*
//! schema migrations should not. This verb answers, for one PR: is **every** migration file
//! it touches a newly added `.sql` file whose statements are all additive?
//!
//! Allowed statements (matched on the leading tokens, case- and whitespace-insensitive, so a
//! statement may span lines): `CREATE TABLE IF NOT EXISTS`, `CREATE [UNIQUE] INDEX IF NOT
//! EXISTS`, `ALTER TABLE <t> ADD COLUMN`, and `INSERT OR IGNORE INTO <t> [(cols)] SELECT …`.
//! Comments (`--`, `/* */`) are ignored. Quoted text (`'…'`, `"…"`, `` `…` ``, `[…]`) is one
//! opaque token, so a `DROP` inside a string literal neither makes an allowed statement
//! forbidden nor makes a forbidden one allowed: the statement is judged by its leading tokens.
//!
//! # Fail closed
//!
//! Exit 0 (`ADDITIVE_MIGRATIONS=1` on stdout) requires every fetched page of the file list to
//! have parsed, at least one migration file, and every migration file (path containing
//! `migrations/`, or ending `_migration.py`) to be `status == "added"` with no
//! `previous_filename`, a `.sql` extension, readable content at the PR head, at least some
//! content, and only allowed statements. Anything else — a forbidden statement, an
//! edited/removed/renamed file, a non-SQL migration file, unterminated quote or comment, an
//! unreadable file, a forge error — exits 1 with the reason on stderr. A caller treats **any**
//! non-zero exit (clap's 2 on a daemon predating this verb, 126/127 for a missing binary) as a
//! hold.

use std::path::Path;

use anyhow::Result;

use crate::forge_pr_diff::{self, FileEntry, ListError, PrHeadReader, RepoError};

/// The `forge_call_stats` operation label of this verb's forge reads.
const OP: &str = "forge.additive_migrations";

/// Why a PR is NOT cleared. Every variant exits 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// `--repo` was not a plain `OWNER/REPO`.
    InvalidRepo,
    /// A page fetch failed or did not parse as the expected file list.
    FetchFailed(String),
    /// The file list may be truncated (the forge caps it).
    ListTruncated,
    /// The PR touches no migration file.
    NoMigrationFiles,
    /// A migration path that is not a plain added file (edited, removed, renamed, ...).
    NotAdded { file: String, status: String },
    /// A migration-path file that is not `.sql`.
    NotSql(String),
    /// The file's content could not be read at the PR head.
    ReadFailed { file: String, why: String },
    /// The file has no content at all.
    Empty(String),
    /// The SQL could not be split (unterminated quote or comment).
    Unparseable { file: String, why: String },
    /// A statement outside the allow-list.
    Forbidden { file: String, statement: String },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRepo => f.write_str("--repo is not a plain OWNER/REPO"),
            Self::FetchFailed(why) => write!(f, "could not read the PR's file list: {why}"),
            Self::ListTruncated => f.write_str("the PR's file list may be truncated"),
            Self::NoMigrationFiles => f.write_str("the PR touches no migration file"),
            Self::NotAdded { file, status } => {
                write!(f, "{file} is not a newly added file (status: {status})")
            }
            Self::NotSql(file) => write!(f, "{file} is a migration path but not a .sql file"),
            Self::ReadFailed { file, why } => write!(f, "could not read {file}: {why}"),
            Self::Empty(file) => write!(f, "{file} is empty"),
            Self::Unparseable { file, why } => write!(f, "{file} could not be parsed: {why}"),
            Self::Forbidden { file, statement } => {
                write!(f, "{file} has a non-additive statement: {statement}")
            }
        }
    }
}

impl From<ListError> for Reason {
    fn from(e: ListError) -> Self {
        match e {
            ListError::Fetch(why) => Self::FetchFailed(why),
            ListError::Truncated => Self::ListTruncated,
        }
    }
}

/// Does `path` fall under Champion's `*migrations/*` / `*_migration.py` kept-set patterns?
fn is_migration_path(path: &str) -> bool {
    path.contains("migrations/") || path.ends_with("_migration.py")
}

// ---------------------------------------------------------------- SQL

/// Split `sql` into statements of tokens, dropping comments. Quoted text is one token (quotes
/// kept); `(`, `)` and `,` are their own tokens. Errors on an unterminated quote or comment.
fn statements(sql: &str) -> Result<Vec<Vec<String>>, String> {
    let chars: Vec<char> = sql.chars().collect();
    let (mut out, mut cur, mut word) = (Vec::new(), Vec::<String>::new(), String::new());
    let flush = |word: &mut String, cur: &mut Vec<String>| {
        if !word.is_empty() {
            cur.push(std::mem::take(word));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '-' if chars.get(i + 1) == Some(&'-') => {
                flush(&mut word, &mut cur);
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                flush(&mut word, &mut cur);
                i += 2;
                loop {
                    match (chars.get(i), chars.get(i + 1)) {
                        (Some('*'), Some('/')) => break,
                        (Some(_), _) => i += 1,
                        (None, _) => return Err("unterminated /* comment".into()),
                    }
                }
                i += 2;
                continue;
            }
            '\'' | '"' | '`' | '[' => {
                flush(&mut word, &mut cur);
                let close = if c == '[' { ']' } else { c };
                let mut tok = String::from(c);
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(format!("unterminated {c} quote")),
                        Some(&q) if q == close => {
                            tok.push(q);
                            // A doubled quote is an escaped quote (not for `[...]`).
                            if c != '[' && chars.get(i + 1) == Some(&close) {
                                tok.push(close);
                                i += 2;
                                continue;
                            }
                            i += 1;
                            break;
                        }
                        Some(&q) => {
                            tok.push(q);
                            i += 1;
                        }
                    }
                }
                cur.push(tok);
                continue;
            }
            ';' => {
                flush(&mut word, &mut cur);
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '(' | ')' | ',' => {
                flush(&mut word, &mut cur);
                cur.push(c.to_string());
            }
            c if c.is_whitespace() => flush(&mut word, &mut cur),
            c => word.push(c),
        }
        i += 1;
    }
    flush(&mut word, &mut cur);
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Do `tokens` start with `words` (case-insensitive, unquoted)?
fn starts_with_words(tokens: &[String], words: &[&str]) -> bool {
    tokens.len() >= words.len()
        && words
            .iter()
            .zip(tokens)
            .all(|(w, t)| t.eq_ignore_ascii_case(w))
}

/// Is this statement on the allow-list?
fn is_additive(t: &[String]) -> bool {
    if starts_with_words(t, &["CREATE", "TABLE", "IF", "NOT", "EXISTS"])
        || starts_with_words(t, &["CREATE", "INDEX", "IF", "NOT", "EXISTS"])
        || starts_with_words(t, &["CREATE", "UNIQUE", "INDEX", "IF", "NOT", "EXISTS"])
    {
        return true;
    }
    // ALTER TABLE <name> ADD COLUMN ...  (the name is one token, quoted or not)
    if starts_with_words(t, &["ALTER", "TABLE"]) {
        return t.len() >= 5
            && t[3].eq_ignore_ascii_case("ADD")
            && t[4].eq_ignore_ascii_case("COLUMN");
    }
    // INSERT OR IGNORE INTO <name> [( cols )] SELECT ...
    if starts_with_words(t, &["INSERT", "OR", "IGNORE", "INTO"]) {
        let mut i = 5; // past the table name
        if t.get(i).map(String::as_str) == Some("(") {
            match t[i..].iter().position(|x| x == ")") {
                Some(close) => i += close + 1,
                None => return false,
            }
        }
        return t.get(i).is_some_and(|x| x.eq_ignore_ascii_case("SELECT"));
    }
    false
}

/// Classify one migration file's SQL.
///
/// # Errors
///
/// [`Reason::Empty`], [`Reason::Unparseable`] or [`Reason::Forbidden`] for `file`.
pub fn classify_sql(file: &str, sql: &str) -> Result<(), Reason> {
    if sql.trim().is_empty() {
        return Err(Reason::Empty(file.to_string()));
    }
    let stmts = statements(sql).map_err(|why| Reason::Unparseable {
        file: file.to_string(),
        why,
    })?;
    for s in &stmts {
        if !is_additive(s) {
            let mut statement = s.join(" ");
            statement.truncate(
                statement
                    .char_indices()
                    .nth(120)
                    .map_or(statement.len(), |(i, _)| i),
            );
            return Err(Reason::Forbidden {
                file: file.to_string(),
                statement,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- PR

/// Decide a whole PR: `entries` is the full file list, `read` returns a file's text at the
/// PR head.
///
/// # Errors
///
/// The [`Reason`] the PR is not cleared.
fn decide_files<R>(entries: &[FileEntry], mut read: R) -> Result<(), Reason>
where
    R: FnMut(&str) -> Result<String, String>,
{
    let mut seen = false;
    for e in entries {
        let prev = e.previous_filename.as_deref();
        if !is_migration_path(&e.filename) && !prev.is_some_and(is_migration_path) {
            continue;
        }
        seen = true;
        if e.status != "added" || prev.is_some() {
            return Err(Reason::NotAdded {
                file: e.filename.clone(),
                status: if e.status.is_empty() {
                    "unknown".into()
                } else {
                    e.status.clone()
                },
            });
        }
        if !e.filename.ends_with(".sql") {
            return Err(Reason::NotSql(e.filename.clone()));
        }
        let sql = read(&e.filename).map_err(|why| Reason::ReadFailed {
            file: e.filename.clone(),
            why,
        })?;
        classify_sql(&e.filename, &sql)?;
    }
    if seen {
        Ok(())
    } else {
        Err(Reason::NoMigrationFiles)
    }
}

/// The whole verdict with the forge reads injected.
///
/// # Errors
///
/// The [`Reason`] the PR is not cleared.
pub fn decide<F, R>(fetch_page: F, read: R) -> Result<(), Reason>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
    R: FnMut(&str) -> Result<String, String>,
{
    decide_files(&forge_pr_diff::list_files(fetch_page)?, read)
}

/// Parsed `forge additive-migrations` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Pull request number.
    pub pr: u32,
    /// `OWNER/REPO`; `None` resolves it from the cwd (never a `{owner}/{repo}` placeholder).
    pub repo: Option<String>,
}

/// Handle `loom-daemon forge additive-migrations <pr> [--repo OWNER/REPO]`.
/// Never returns (exits): 0 with `ADDITIVE_MIGRATIONS=1` when every migration file is
/// additive, otherwise 1 with the reason on stderr.
pub fn handle(args: &Args) -> Result<()> {
    let pr = args.pr;
    let verdict = match forge_pr_diff::resolve_gh_repo(args.repo.as_deref()) {
        Err(RepoError::Invalid) => Err(Reason::InvalidRepo),
        Err(RepoError::Unresolved(why)) => Err(Reason::FetchFailed(why)),
        Ok((gh, repo)) => {
            let gh = Path::new(&gh);
            let mut head = PrHeadReader::new(OP, gh, &repo, pr);
            decide(
                |page| forge_pr_diff::gh_fetch_page(OP, gh, &repo, pr, page),
                |file| head.read(file),
            )
        }
    };
    match verdict {
        Ok(()) => {
            println!("ADDITIVE_MIGRATIONS=1");
            std::process::exit(0);
        }
        Err(reason) => {
            eprintln!("loom-daemon forge additive-migrations: PR #{pr} held: {reason}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ok(sql: &str) {
        assert_eq!(classify_sql("m/1.sql", sql), Ok(()), "{sql}");
    }

    fn held(sql: &str) {
        assert!(classify_sql("m/1.sql", sql).is_err(), "should hold: {sql}");
    }

    #[test]
    fn allowed_forms_pass() {
        for sql in [
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, a TEXT);",
            "CREATE INDEX IF NOT EXISTS i ON t(a);",
            "CREATE UNIQUE INDEX IF NOT EXISTS i ON t(a);",
            "ALTER TABLE t ADD COLUMN b TEXT DEFAULT 'x';",
            "INSERT OR IGNORE INTO t SELECT * FROM u;",
            "INSERT OR IGNORE INTO t (a, b) SELECT a, b FROM u WHERE a > 1;",
            "-- only a comment\n/* and a block */",
            "create table if not exists t (a int)",
        ] {
            ok(sql);
        }
    }

    #[test]
    fn forbidden_forms_hold() {
        for sql in [
            "DROP TABLE t;",
            "DELETE FROM t;",
            "UPDATE t SET a = 1;",
            "ALTER TABLE t RENAME TO u;",
            "ALTER TABLE t RENAME COLUMN a TO b;",
            "ALTER TABLE t DROP COLUMN a;",
            "REPLACE INTO t VALUES (1);",
            "INSERT INTO t VALUES (1);",
            "INSERT OR REPLACE INTO t SELECT * FROM u;",
            "INSERT OR IGNORE INTO t VALUES (1);",
            "CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1; END;",
            "PRAGMA foreign_keys = OFF;",
            "CREATE TABLE t (a int);",
            "CREATE INDEX i ON t(a);",
            "CREATE TEMP TABLE IF NOT EXISTS t (a int);",
            "CREATE TABLE IF NOT EXISTS t (a int); DROP TABLE t;",
        ] {
            held(sql);
        }
    }

    #[test]
    fn multiline_statement_passes() {
        ok("CREATE TABLE\n  IF NOT EXISTS\n  t (\n    a TEXT,\n    b TEXT\n  );\n");
        ok("ALTER TABLE t\n  ADD\n  COLUMN c INTEGER;");
    }

    #[test]
    fn comments_are_ignored() {
        ok("-- DROP TABLE x;\nCREATE TABLE IF NOT EXISTS t (a int); -- DELETE FROM t\n");
        ok("/* DROP TABLE x; */ CREATE TABLE IF NOT EXISTS t (a int);");
        held("-- fine\nDROP TABLE t; -- CREATE TABLE IF NOT EXISTS t (a int);");
    }

    #[test]
    fn semicolon_and_drop_inside_string_literal() {
        // Judged by leading tokens: a literal never turns a statement forbidden...
        ok("INSERT OR IGNORE INTO t SELECT 'a; DROP TABLE x' FROM u;");
        // ...and never launders a forbidden one.
        held("DELETE FROM t WHERE a = 'CREATE TABLE IF NOT EXISTS x';");
    }

    #[test]
    fn unparsable_sql_holds() {
        held("CREATE TABLE IF NOT EXISTS t (a TEXT DEFAULT 'oops);");
        held("CREATE TABLE IF NOT EXISTS t (a int); /* never closed");
        held("");
        held("   \n\t ");
    }

    #[test]
    fn crlf_and_mixed_case() {
        ok("Create Table If Not Exists t (a int);\r\nAlter Table t Add Column b int;\r\n");
    }

    fn entry(name: &str, status: &str, prev: Option<&str>) -> FileEntry {
        FileEntry {
            filename: name.into(),
            status: status.into(),
            previous_filename: prev.map(str::to_string),
            patch: None,
        }
    }

    fn good(_: &str) -> Result<String, String> {
        Ok("CREATE TABLE IF NOT EXISTS t (a int);".into())
    }

    #[test]
    fn added_sql_migration_passes() {
        let e = [
            entry("migrations/0042_x.sql", "added", None),
            entry("src/lib.rs", "modified", None),
        ];
        assert_eq!(decide_files(&e, good), Ok(()));
    }

    #[test]
    fn edited_removed_renamed_migration_holds() {
        for (status, prev) in [
            ("modified", None),
            ("removed", None),
            ("renamed", Some("migrations/old.sql")),
            ("added", Some("elsewhere/old.sql")),
            ("", None),
        ] {
            let e = [entry("migrations/0042_x.sql", status, prev)];
            assert!(matches!(decide_files(&e, good), Err(Reason::NotAdded { .. })));
        }
        // Renamed *out of* the migrations dir still counts.
        let e = [entry("docs/x.sql", "renamed", Some("migrations/0001.sql"))];
        assert!(matches!(decide_files(&e, good), Err(Reason::NotAdded { .. })));
    }

    #[test]
    fn non_sql_migration_path_holds() {
        let e = [entry("db/0003_add_migration.py", "added", None)];
        assert_eq!(decide_files(&e, good), Err(Reason::NotSql("db/0003_add_migration.py".into())));
        let e = [entry("migrations/README.md", "added", None)];
        assert!(matches!(decide_files(&e, good), Err(Reason::NotSql(_))));
    }

    #[test]
    fn no_migration_file_holds() {
        let e = [entry("src/lib.rs", "added", None)];
        assert_eq!(decide_files(&e, good), Err(Reason::NoMigrationFiles));
        assert_eq!(decide_files(&[], good), Err(Reason::NoMigrationFiles));
    }

    #[test]
    fn one_bad_file_among_good_holds() {
        let e = [
            entry("migrations/1.sql", "added", None),
            entry("migrations/2.sql", "added", None),
        ];
        let r = |f: &str| {
            Ok(if f.ends_with("2.sql") {
                "DROP TABLE t;".into()
            } else {
                "CREATE TABLE IF NOT EXISTS t (a int);".into()
            })
        };
        assert!(matches!(decide_files(&e, r), Err(Reason::Forbidden { .. })));
    }

    #[test]
    fn unreadable_file_holds() {
        let e = [entry("migrations/1.sql", "added", None)];
        let r = |_: &str| Err("boom".to_string());
        assert!(matches!(decide_files(&e, r), Err(Reason::ReadFailed { .. })));
    }

    #[test]
    fn list_fetch_errors_hold() {
        let r = decide(|_| Err("api down".into()), good);
        assert!(matches!(r, Err(Reason::FetchFailed(_))));
        let r = decide(|_| Ok(b"not json".to_vec()), good);
        assert!(matches!(r, Err(Reason::FetchFailed(_))));
    }

    /// Pagination itself is covered in `forge_pr_diff`; this pins the verb's mapping of a
    /// truncated list to a hold.
    #[test]
    fn full_list_at_cap_holds() {
        let page: Vec<String> = (0..crate::forge_pr_diff::PAGE_SIZE)
            .map(|i| format!(r#"{{"filename":"src/f{i}.rs","status":"modified"}}"#))
            .collect();
        let page = format!("[{}]", page.join(","));
        let r = decide(|_| Ok(page.clone().into_bytes()), good);
        assert_eq!(r, Err(Reason::ListTruncated));
    }
}
