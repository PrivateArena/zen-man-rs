//! The indexed-search service (plan §5.4 `services/search_index.rs`, a port of `ParseSearchQuery` +
//! `ExecuteSearch` + the three read handlers in `api/search_index*.go`).
//!
//! # The operator set — Go's, exactly, and it is *not* the plan's
//!
//! Go's filter regex (`api/search_index.go:44`) is `(\bext|\bin|\btype):(?:"([^"]+)"|(\S+))`: the three
//! operators are **`ext:`, `in:`, `type:`**, and there is **no `size:`** (the plan drafted one; the Go
//! binary has never had it, and the schema stores no way to answer it). `type:` accepts
//! `dir`/`directory`/`folder` → directories, `file` → files, anything else leaves the filter at *any*
//! (Go's silent default, kept). Quoted values (`in:"/home/me/my folder"`) are supported; a bare value
//! runs to the next whitespace, and the filter token is *removed* from the text before the plain words
//! are tokenised (`"[^"]+"|\S+`, lowercased) — so `report ext:txt` searches the word `report`, not the
//! word `ext`.
//!
//! # Ordering, paging, `capped`
//!
//! `ORDER BY is_dir DESC, name ASC` over the **lowercased** `name` column, with `id` appended as the
//! tie-break: Go leaves ties to SQLite's index scan order, which is rowid order, which is *walk order* —
//! so the tie-break is only parity-preserving because [`crate::index::walker`] walks deterministically
//! (the reason the parallel walk is a step-12b decision and not a silent change). `limit` defaults to
//! **200** (Go's handler constant, `api/search_index_api.go:26`), `offset` to 0, and
//! `capped = total_matched > limit + offset` — Go's off-by-whichever expression, kept verbatim.
//!
//! The response's `rel_path` carries the **absolute** path (Go's own comment: "for global search,
//! RelPath stores the full absolute path", `api/search_index.go:378`) and `mode` is always `""`
//! (plan §2.4): the index stores no mode, so the port does not invent one.

use rusqlite::Connection;
use serde::Serialize;

use crate::error::CoreError;

/// Go's handler default (`api/search_index_api.go:26`).
pub const DEFAULT_LIMIT: i64 = 200;

/// The `type:` filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TypeFilter {
    /// No type constraint (Go's `"any"`, including an unrecognised `type:` value).
    #[default]
    Any,
    /// `dir` / `directory` / `folder`.
    Dir,
    /// `file`.
    File,
}

/// Go's `ParsedQuery` (`api/search_index.go:35-40`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Plain words, lowercased, quotes stripped.
    pub words: Vec<String>,
    /// `ext:` values, lowercased, without the dot (the column stores them so).
    pub extensions: Vec<String>,
    /// `in:` values, lexically cleaned to absolute form at execution (Go's `filepath.Clean`).
    pub folders: Vec<String>,
    /// The `type:` filter.
    pub type_filter: TypeFilter,
}

impl ParsedQuery {
    /// `true` when the query would match the whole index (Go's "no query and no filters" note).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.extensions.is_empty() && self.folders.is_empty()
    }
}

fn is_word_byte(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

const KEYS: [(&str, u8); 3] = [("ext", 0), ("in", 1), ("type", 2)];

/// Go's `ParseSearchQuery` (`api/search_index.go:246-299`) without the regexes.
#[must_use]
pub fn parse_query(q: &str) -> ParsedQuery {
    let chars: Vec<char> = q.chars().collect();
    let mut pq = ParsedQuery::default();
    let mut residue: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let mut matched = false;
        for (key, which) in KEYS {
            let klen = key.chars().count();
            let head = key.chars().next().unwrap_or('\0');
            if chars[i] != head {
                continue;
            }
            let after = i + klen;
            if chars.get(after) != Some(&':') {
                continue;
            }
            // Go's `\b` before the keyword: a word character may not precede it.
            if i > 0 && is_word_byte(chars[i - 1]) {
                continue;
            }
            let vstart = after + 1;
            let Some(&first) = chars.get(vstart) else {
                continue;
            };
            let (value, next) = if first == '"' {
                let end = chars[vstart + 1..]
                    .iter()
                    .position(|&c| c == '"')
                    .map_or(chars.len(), |p| vstart + 1 + p);
                let v: String = chars[vstart + 1..end.min(chars.len())].iter().collect();
                (v, (end + 1).min(chars.len() + 1))
            } else if first.is_whitespace() {
                // Go's `(\S+)` alternative needs at least one character: `in: ` is a word, not a filter.
                continue;
            } else {
                let mut j = vstart;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                let v: String = chars[vstart..j].iter().collect();
                (v, j)
            };
            let value = value.trim();
            match which {
                0 => pq.extensions.push(value.to_lowercase()),
                1 => pq.folders.push(value.to_string()),
                _ => {
                    let lv = value.to_lowercase();
                    if lv == "dir" || lv == "directory" || lv == "folder" {
                        pq.type_filter = TypeFilter::Dir;
                    } else if lv == "file" {
                        pq.type_filter = TypeFilter::File;
                    }
                }
            }
            // Go replaces the whole matched token with "" and lets the residue tokenise; the loop
            // below preserves the residue the same way (one space keeps neighbouring words apart).
            residue.push(' ');
            i = next;
            matched = true;
            break;
        }
        if matched {
            continue;
        }
        residue.push(chars[i]);
        i += 1;
    }
    // `wordRe = "[^"]+"|\S+` — a quoted span is one token even with spaces inside.
    let text: String = residue.into_iter().collect();
    let cs: Vec<char> = text.chars().collect();
    let mut k = 0;
    while k < cs.len() {
        if cs[k].is_whitespace() {
            k += 1;
            continue;
        }
        let (token, next) = if cs[k] == '"' {
            let end = cs[k + 1..]
                .iter()
                .position(|&c| c == '"')
                .map_or(cs.len(), |p| k + 1 + p);
            let t: String = cs[k + 1..end.min(cs.len())].iter().collect();
            (t, (end + 1).min(cs.len()))
        } else {
            let mut j = k;
            while j < cs.len() && !cs[j].is_whitespace() {
                j += 1;
            }
            let t: String = cs[k..j].iter().filter(|&&c| c != '"').collect();
            (t, j)
        };
        let token = token.trim().to_lowercase();
        if !token.is_empty() {
            pq.words.push(token);
        }
        k = next;
    }
    pq
}

/// Lexical `filepath.Clean` for the `in:` value (Go cleans, it never resolves symlinks here).
#[must_use]
pub fn clean_path(p: &str) -> String {
    let absolute = p.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if stack.last().is_some_and(|l| *l != "..") {
                    stack.pop();
                } else if !absolute {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// One result row, in Go's `SearchEntry` fields (`rel_path` = the absolute path, `mode` = `""`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IndexEntry {
    /// `name_orig` — the name as it is on disk.
    pub name: String,
    /// Directory or file.
    pub is_dir: bool,
    /// Bytes (a symlink's own size).
    pub size: i64,
    /// mtime, seconds.
    pub mod_time: i64,
    /// Always `""` — the index stores no mode (plan §2.4).
    pub mode: String,
    /// The **absolute** path (Go's global-search convention).
    pub rel_path: String,
}

/// The `/api/index/search` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IndexSearchResults {
    /// The page.
    pub entries: Vec<IndexEntry>,
    /// Rows matching the whole WHERE, not just the page.
    pub total_matched: i64,
    /// `total_matched > limit + offset` (Go's expression, kept).
    pub capped: bool,
    /// The index is behind the filesystem.
    pub index_dirty: bool,
    /// Unix seconds of the last successful rebuild.
    pub last_indexed_unix: i64,
}

/// SQLite's `LIKE` escaping (Go's `escapeLikePattern`).
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The FTS5 match string: every word as one quoted phrase, AND-ed (Go's `"… AND …"`,
/// `api/search_index.go:290-297`).
#[must_use]
pub fn fts_match_string(words: &[String]) -> String {
    words
        .iter()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Execute one parsed query. `dirty`/`last_indexed_unix` come from the caller's index state (the
/// database does not carry it; `Session::index_status` owns it, mirroring Go's config-file state).
///
/// # Errors
/// [`CoreError::Sql`] for any query failure (a malformed FTS expression reaches SQLite and comes back
/// typed, not panicked).
pub fn execute(
    conn: &Connection,
    pq: &ParsedQuery,
    limit: i64,
    offset: i64,
    dirty: bool,
    last_indexed_unix: i64,
) -> Result<IndexSearchResults, CoreError> {
    let mut conditions: Vec<String> = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if !pq.words.is_empty() {
        conditions.push("id IN (SELECT rowid FROM files_fts WHERE files_fts MATCH ?)".to_string());
        args.push(rusqlite::types::Value::Text(fts_match_string(&pq.words)));
    }
    if !pq.extensions.is_empty() {
        let conds = pq
            .extensions
            .iter()
            .map(|_| "ext = ?")
            .collect::<Vec<_>>()
            .join(" OR ");
        conditions.push(format!("({conds})"));
        for e in &pq.extensions {
            args.push(rusqlite::types::Value::Text(e.clone()));
        }
    }
    for folder in &pq.folders {
        let cleaned = clean_path(folder);
        conditions.push("(parent = ? OR parent LIKE ? ESCAPE '\\')".to_string());
        args.push(rusqlite::types::Value::Text(cleaned.clone()));
        // Go appends the *separator* before the wildcard (`api/search_index.go:325`:
        // `escapeLikePattern(absF) + string(filepath.Separator) + "%"`), so `in:/root/na` is the
        // directory `/root/na` and not a string prefix of `/root/name`.
        args.push(rusqlite::types::Value::Text(format!(
            "{}{}%",
            escape_like(&cleaned),
            std::path::MAIN_SEPARATOR
        )));
    }
    match pq.type_filter {
        TypeFilter::Any => {}
        TypeFilter::Dir => conditions.push("is_dir = 1".to_string()),
        TypeFilter::File => conditions.push("is_dir = 0".to_string()),
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let total_matched: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM files {where_clause};"),
            rusqlite::params_from_iter(args.iter()),
            |r| r.get(0),
        )
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;

    let mut page_args = args.clone();
    page_args.push(rusqlite::types::Value::Integer(limit));
    page_args.push(rusqlite::types::Value::Integer(offset));
    let mut stmt = conn
        .prepare(&format!(
            "SELECT name_orig, is_dir, size, mtime, path FROM files {where_clause} \
             ORDER BY is_dir DESC, name ASC, id ASC LIMIT ? OFFSET ?;"
        ))
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(page_args.iter()), |r| {
            Ok(IndexEntry {
                name: r.get::<_, String>(0)?,
                is_dir: r.get::<_, i64>(1)? != 0,
                size: r.get::<_, i64>(2)?,
                mod_time: r.get::<_, i64>(3)?,
                mode: String::new(),
                rel_path: r.get::<_, String>(4)?,
            })
        })
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row.map_err(|e| CoreError::Sql { msg: e.to_string() })?);
    }
    Ok(IndexSearchResults {
        entries,
        total_matched,
        capped: total_matched > limit + offset,
        index_dirty: dirty,
        last_indexed_unix,
    })
}

/// `/api/index/search` with the raw query string: parse + [`execute`].
///
/// # Errors
/// As [`execute`].
pub fn search(
    conn: &Connection,
    q: &str,
    limit: i64,
    offset: i64,
    dirty: bool,
    last_indexed_unix: i64,
) -> Result<IndexSearchResults, CoreError> {
    execute(
        conn,
        &parse_query(q),
        limit,
        offset,
        dirty,
        last_indexed_unix,
    )
}

/// `/api/index/status` body (the `index-status.json` keys, plus Go's optional `error`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IndexStatus {
    /// A rebuild is running.
    pub is_indexing: bool,
    /// Rows in `files` after the last successful rebuild.
    pub total_files: i64,
    /// Unix seconds of the last successful rebuild.
    pub last_indexed: i64,
    /// The index is behind the filesystem.
    pub is_dirty: bool,
    /// The indexer's settings, as Go's nested `config` object reports them.
    pub config: IndexStatusConfig,
    /// Live progress (Go counts rows flushed, not rows visited).
    pub progress: IndexStatusProgress,
    /// Why the last rebuild failed, when it did (Go's `im.LastErr`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The `config` half of [`IndexStatus`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IndexStatusConfig {
    /// Indexed roots.
    pub roots: Vec<String>,
    /// Glob excludes (the invalid ones are reported through `IndexStatus::error` instead of silently
    /// dropped, the plan §5.4 fix).
    pub excludes: Vec<String>,
    /// Rebuild on startup.
    pub auto_index: bool,
    /// Walk threads (reserved — the walk is sequential, see `index/walker.rs`).
    pub worker_count: u32,
    /// Follow symlinked directories (Go records this and never acts on it; the port does act on it).
    pub follow_symlinks: bool,
}

/// The `progress` half of [`IndexStatus`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IndexStatusProgress {
    /// Rows written so far (Go's `ProgressCount`).
    pub indexed: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch("PRAGMA journal_mode = DELETE;")
            .expect("journal mode");
        schema::apply(&conn).expect("the Go schema");
        conn
    }

    /// Index rows shaped like the walker's output: `name` is the lowercased base, `parent` Go's
    /// `filepath.Dir`, `ext` the dot-less lowercase extension.
    fn row(conn: &Connection, path: &str, is_dir: bool) {
        let p = std::path::Path::new(path);
        let base = p
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let parent = p
            .parent()
            .map_or_else(|| "/".to_string(), |x| x.display().to_string());
        let ext = p
            .extension()
            .map_or(String::new(), |e| e.to_string_lossy().to_lowercase());
        conn.execute(
            "INSERT INTO files (path,name,name_orig,ext,parent,size,mtime,is_dir,is_symlink) \
             VALUES (?1,?2,?3,?4,?5,1,1700000000,?6,0);",
            rusqlite::params![
                path,
                base.to_lowercase(),
                base,
                ext,
                parent,
                i64::from(is_dir)
            ],
        )
        .expect("row");
        let id: i64 = conn
            .query_row("SELECT id FROM files WHERE path = ?1;", [path], |r| {
                r.get(0)
            })
            .expect("id");
        conn.execute(
            "INSERT INTO files_fts (rowid, name) VALUES (?1, ?2);",
            rusqlite::params![id, base.to_lowercase()],
        )
        .expect("fts row");
    }

    /// The `in:` fixture: a directory, a sibling whose name is a *string* prefix of it, and a
    /// `LIKE`-wildcard twin.
    fn tree(conn: &Connection) {
        for (path, dir) in [
            ("/root/na", true),
            ("/root/na/one.txt", false),
            ("/root/name/two.txt", false),
            ("/root/notes/three.md", false),
            ("/root/100%", true),
            ("/root/100%/four.txt", false),
            ("/root/100x/five.txt", false),
        ] {
            row(conn, path, dir);
        }
    }

    fn paths(res: &IndexSearchResults) -> Vec<String> {
        res.entries.iter().map(|e| e.rel_path.clone()).collect()
    }

    // ---------------------------------------------------------------- the parser

    #[test]
    fn every_operator_takes_a_quoted_value_with_spaces() {
        let pq = parse_query("ext:\"my ext\" in:\"/home/me/my folder\" type:\"file\" report");
        assert_eq!(
            pq.extensions,
            vec!["my ext".to_string()],
            "`ext:` keeps the space"
        );
        assert_eq!(pq.folders, vec!["/home/me/my folder".to_string()]);
        assert_eq!(
            pq.type_filter,
            TypeFilter::File,
            "a quoted `type:` value is still an alias"
        );
        assert_eq!(
            pq.words,
            vec!["report".to_string()],
            "the filter tokens left no residue"
        );
    }

    #[test]
    fn an_unknown_operator_is_an_ordinary_word() {
        // Go's filter regex knows exactly three keywords; `size:` (the plan draft) and anything else
        // survive as searchable text rather than vanishing.
        let pq = parse_query("size:10k notes");
        assert!(pq.extensions.is_empty() && pq.folders.is_empty());
        assert_eq!(pq.type_filter, TypeFilter::Any);
        assert_eq!(pq.words, vec!["size:10k".to_string(), "notes".to_string()]);
    }

    #[test]
    fn a_keyword_may_not_be_glued_to_a_word_and_an_empty_value_is_a_word() {
        // Go's `\b` before the keyword, and its `(\S+)` alternative needing one character.
        let pq = parse_query("join:a/b");
        assert!(pq.folders.is_empty(), "`in` inside `join` is not a filter");
        assert_eq!(pq.words, vec!["join:a/b".to_string()]);
        let pq = parse_query("in: x");
        assert!(pq.folders.is_empty(), "`in: ` is a word, not a filter");
        assert_eq!(pq.words, vec!["in:".to_string(), "x".to_string()]);
        // The keywords themselves are case-sensitive in Go's regex, so `EXT:` is text.
        let pq = parse_query("EXT:PDF");
        assert!(pq.extensions.is_empty());
        assert_eq!(pq.words, vec!["ext:pdf".to_string()]);
    }

    #[test]
    fn the_type_aliases_collapse_and_an_unknown_one_is_any() {
        for (q, want) in [
            ("type:dir", TypeFilter::Dir),
            ("type:directory", TypeFilter::Dir),
            ("type:folder", TypeFilter::Dir),
            ("type:FOLDER", TypeFilter::Dir),
            ("type:file", TypeFilter::File),
            ("type:File", TypeFilter::File),
            ("type:banana", TypeFilter::Any),
            ("", TypeFilter::Any),
        ] {
            assert_eq!(parse_query(q).type_filter, want, "{q}");
        }
    }

    #[test]
    fn an_unterminated_quote_keeps_the_path_clean_the_port_side_of_a_known_divergence() {
        // Go's `(?:"([^"]+)"|(\S+))` fails the quoted alternative and falls back to `(\S+)`, which
        // captures the quote itself: `in:"/a/b` yields the folder `"/a/b` (`search_index.go:44`,
        // `:246-262`). The port's scanner consumes the opening quote and yields the clean path. Logged
        // as a deviation in plan §6.2, not "fixed" — see the same paragraph for the interior-quote
        // half below.
        let pq = parse_query("in:\"/a/b");
        assert_eq!(pq.folders, vec!["/a/b".to_string()]);
        assert!(
            pq.words.is_empty(),
            "the dangling quote left no word behind"
        );
        // …and the residue half: an unquoted token with an interior quote.
        let pq = parse_query("ab\"cd");
        assert_eq!(
            pq.words,
            vec!["abcd".to_string()],
            "the port strips interior quotes from an unquoted token; Go's `strings.Trim(w, \"\\\"\")` keeps them"
        );
    }

    #[test]
    fn clean_path_is_filepaths_clean_not_a_resolver() {
        for (input, want) in [
            ("/a/b/../c", "/a/c"),
            ("/a//b/./c", "/a/b/c"),
            ("/a/b/", "/a/b"),
            ("/", "/"),
            ("", "."),
            ("a/../b", "b"),
            ("../a", "../a"),
            ("/../a", "/a"),
            ("/a/../../b", "/b"),
        ] {
            assert_eq!(clean_path(input), want, "clean_path({input:?})");
        }
    }

    #[test]
    fn fts_match_string_quotes_each_word_as_a_phrase() {
        // Go's `"…" AND "…"` (`api/search_index.go:290-297`); a `"` inside a word is doubled, not
        // allowed to close the phrase.
        let quoted = |w: &[&str]| -> Vec<String> { w.iter().map(|s| (*s).to_string()).collect() };
        assert_eq!(fts_match_string(&quoted(&[])), "");
        assert_eq!(fts_match_string(&quoted(&["one"])), "\"one\"");
        assert_eq!(
            fts_match_string(&quoted(&["one", "two"])),
            "\"one\" AND \"two\""
        );
        assert_eq!(
            fts_match_string(&quoted(&["a\"b"])),
            "\"a\"\"b\"",
            "the quote is escaped, the phrase stays one phrase"
        );
    }

    // ---------------------------------------------------------------- execution

    #[test]
    fn in_a_folder_is_a_directory_boundary_not_a_string_prefix() {
        // The regression the fix exists for: a bare `parent LIKE '/root/na%'` would also return
        // `/root/name/…`. Go appends the separator before the wildcard (`search_index.go:325`).
        let conn = db();
        tree(&conn);
        let res = search(&conn, "in:/root/na", DEFAULT_LIMIT, 0, false, 0).expect("query");
        assert_eq!(
            paths(&res),
            vec!["/root/na/one.txt".to_string()],
            "only the children of `/root/na`"
        );
        assert_eq!(res.total_matched, 1);
        assert!(
            !paths(&res).iter().any(|p| p.starts_with("/root/name")),
            "{:?}",
            paths(&res)
        );
        // The exact-parent arm still answers, and a trailing slash cleans to the same directory.
        let with_slash = search(&conn, "in:/root/na/", DEFAULT_LIMIT, 0, false, 0).expect("query");
        assert_eq!(
            paths(&with_slash),
            paths(&res),
            "`filepath.Clean` first, then match"
        );
        let deeper = search(&conn, "in:/root", DEFAULT_LIMIT, 0, false, 0).expect("query");
        assert_eq!(
            deeper.total_matched, 7,
            "the two `/root` children plus the five rows under them — and `/root` itself is not a hit"
        );
    }

    #[test]
    fn folder_values_are_like_escaped_before_the_wildcard() {
        let conn = db();
        tree(&conn);
        let res = search(&conn, "in:/root/100%", DEFAULT_LIMIT, 0, false, 0).expect("query");
        assert_eq!(
            paths(&res),
            vec!["/root/100%/four.txt".to_string()],
            "`%` is data; `/root/100x` is not a child of `/root/100%`"
        );
    }

    #[test]
    fn query_text_is_always_bound_never_interpolated() {
        // The bind-safety proof: the classic `' OR '1'='1` and friends must never return the whole
        // table, and the schema must still be there afterwards.
        let conn = db();
        tree(&conn);
        let all: i64 = conn
            .query_row("SELECT COUNT(*) FROM files;", [], |r| r.get(0))
            .expect("count");
        assert_eq!(all, 7);
        let mut answered = 0;
        for q in [
            "' OR '1'='1",
            "in:' OR '1'='1",
            "ext:' OR '1'='1",
            "in:/root/na' --",
            "x'); DROP TABLE files;--",
            "in:/root/na\") OR 1=1 --",
        ] {
            let res = search(&conn, q, DEFAULT_LIMIT, 0, false, 0)
                .expect("a typed result, not a failure");
            answered += 1;
            assert!(
                res.total_matched < all,
                "{q}: an injected predicate returned {}/{} rows",
                res.total_matched,
                all
            );
        }
        assert_eq!(
            answered, 6,
            "every injection was answered, none was a SQL failure"
        );
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM files;", [], |r| r.get(0))
            .expect("the table survived every injection attempt");
        assert_eq!(after, all, "nothing was dropped or rewritten");
    }

    #[test]
    fn the_other_filters_and_the_page_fields_beat_the_where_clause() {
        let conn = db();
        tree(&conn);
        let res = search(&conn, "ext:txt", DEFAULT_LIMIT, 0, false, 0).expect("query");
        assert_eq!(
            res.total_matched, 4,
            "the dot is not stored, the case is folded"
        );
        let res = search(&conn, "type:dir", DEFAULT_LIMIT, 0, false, 0).expect("query");
        assert_eq!(
            paths(&res),
            vec!["/root/100%".to_string(), "/root/na".to_string()]
        );
        let res = search(
            &conn,
            "in:/root/na ext:txt type:file",
            DEFAULT_LIMIT,
            0,
            true,
            42,
        )
        .expect("query");
        assert_eq!(
            paths(&res),
            vec!["/root/na/one.txt".to_string()],
            "the filters AND"
        );
        assert!(
            res.index_dirty && res.last_indexed_unix == 42,
            "state is carried through"
        );
        let page = search(&conn, "ext:txt", 2, 0, false, 0).expect("query");
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.total_matched, 4);
        assert!(
            page.capped,
            "total_matched {} > limit 2 + offset 0",
            page.total_matched
        );
        let last = search(&conn, "ext:txt", 2, 2, false, 0).expect("query");
        assert_eq!(last.entries.len(), 2);
        assert!(!last.capped, "the tail of the page is not capped");
        let empty = search(&conn, "", DEFAULT_LIMIT, 0, false, 0).expect("query");
        assert_eq!(empty.total_matched, 7, "an empty query is the whole index");
    }
}
