//! Batch rename — the preview engine and the collision-aware apply (plan §5.3's `ops/rename.rs` row,
//! batch half; Go `frontend/js/batch-rename.js`).
//!
//! # Why it lives here
//!
//! §5.3 has no `services/batch_rename.rs`: the *write* is a rename, and [`crate::ops::rename`] already
//! owns the batch semantics — positional targets ([`RenameTarget::Batch`], §6.2's replacement for Go's
//! JSON-in-`name`) and, critically, the **parent rewrite** step 9c proved (renaming `/root/a` → `/root/A`
//! must rewrite the later source `/root/a/b`, `api/ops.go:466-474`). A preview engine that lived
//! anywhere else would have to re-implement that, or hand the UI a plan that `Session::rename` then
//! performs differently. So: [`preview`] is pure and cheap enough to run on every keystroke (the
//! 2 ms / 10 k-row criterion lives in `crates/zenman-core/tests/perf_batch_rename.rs`), and
//! [`apply_request`] turns a preview into the one [`RenameReq`] the session executes.
//!
//! # The rules, exactly as the browser ran them (`calculatePreviews`, `batch-rename.js:191-318`)
//!
//! 1. split the name: for a *file*, `lastDot > 0` puts everything from the dot into the extension;
//! 2. if `find` is non-empty, replace **every** occurrence in the stem (or, with
//!    [`Rules::include_extension`], in the whole name, then re-split);
//! 3. expand the counter tokens in both halves — `[n]` and `[n:W]`, `n = start + index × step` where
//!    `index` is the row's position in the selection;
//! 4. classify: an empty target is a conflict, an identical target is *unchanged*, a name already in the
//!    folder that is not itself part of the batch is a conflict, and a target two rows picked is a
//!    conflict.
//!
//! Two Go details are load-bearing: the collision set is *directory names minus the selected rows*
//! (`otherExistingNames`, `:227-229`) — otherwise renaming `a.txt` → `b.txt` over a *selected* `b.txt`
//! would report a collision the batch itself resolves — and an invalid pattern leaves every target
//! **unmodified** rather than erroring the row (`:262-263`), with the compile error surfaced once for the
//! whole panel.
//!
//! # What the port changes
//!
//! * Go's `applyBatchRename` refused to apply *anything* while one row conflicted (`:139-144`). That is
//!   kept as [`ConflictPolicy::SkipExisting`] behaviour — the conflicting rows are reported, never
//!   silently dropped — and [`ConflictPolicy::Rename`] adds the **second pass** Go never had: the row gets
//!   a `(2)`-style suffix computed against the same in-memory name set, so two rows racing for one name
//!   rename to two names instead of the second clobbering the first (`os.Rename` would have, silently, on
//!   POSIX).
//! * the regex dialect is **this module's [`pattern`] engine**, not JavaScript's: there is no regex crate
//!   anywhere in this workspace's dependency graph, and step 11 established the precedent of writing the
//!   engine rather than adding one (`services/name_match.rs`). It is the RE2-flavoured subset a rename
//!   actually needs — literals, `.`, `\d \w \s \D \W \S`, escapes, `[…]` classes with ranges and negation,
//!   `^ $`, greedy and lazy `* + ? {m,n}`, groups with numbered captures, alternation. JS-only syntax
//!   (`\b`, lookaround, backreferences, named groups) is refused at compile time with
//!   [`CoreError::BadRegex`], which is a *reportable* error, not a wrong rename. Substitution understands
//!   `$1`-`$99`, `$0`/`$&` (whole match) and `$$`.

use std::collections::HashSet;

use crate::error::CoreError;
use crate::ops::ConflictPolicy;
use crate::ops::rename::{RenameReq, RenameTarget};
use crate::path::AbsPath;

/// One row of the selection: what it is called now, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Full path — `ops::rename` takes sources, not names, so the parent rewrite can find them.
    pub path: AbsPath,
    /// The listing's name for it (the string the rules run over).
    pub name: String,
    /// Directories keep their whole name; only files split into stem + extension.
    pub is_dir: bool,
}

impl Row {
    /// A row from a listing entry.
    #[must_use]
    pub fn new(path: AbsPath, name: impl Into<String>, is_dir: bool) -> Self {
        Self {
            path,
            name: name.into(),
            is_dir,
        }
    }
}

/// The rule set the dialog edits, one-for-one with Go's seven inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    /// The find field. Empty means "no replacement step at all" (counter-only renames still run).
    pub find: String,
    /// The replace field. Group references in [`Rules::regex`] mode; literal otherwise.
    pub replace: String,
    /// Go's `batch-rename-regex` checkbox: `find` is a pattern, not text.
    pub regex: bool,
    /// Go's `batch-rename-case` checkbox — wired to the `i` **flag**, so `true` means *ignore* case
    /// (`new RegExp(find, isCase ? 'gi' : 'g')`, `batch-rename.js:211-215`). The label is misleading and
    /// the port keeps the behaviour, not the name.
    pub ignore_case: bool,
    /// Go's `batch-rename-ext` checkbox: run the replacement over the whole file name, extension
    /// included, instead of over the stem only.
    pub include_extension: bool,
    /// `counter-start`. Go's `parseInt(x,10) || 1`, i.e. 0 and garbage both read as 1 — see [`Rules::counter`].
    pub counter_start: i64,
    /// `counter-step`, same defaulting.
    pub counter_step: i64,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            find: String::new(),
            replace: String::new(),
            regex: false,
            ignore_case: false,
            include_extension: false,
            counter_start: 1,
            counter_step: 1,
        }
    }
}

impl Rules {
    /// Go's `parseInt(x,10) || 1` for the two counter fields: `0` and garbage read as `1`.
    #[must_use]
    pub fn counter(&self) -> (i64, i64) {
        (
            if self.counter_start == 0 {
                1
            } else {
                self.counter_start
            },
            if self.counter_step == 0 {
                1
            } else {
                self.counter_step
            },
        )
    }
}

/// Why a row is not going to be renamed as typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStatus {
    /// A real change, ready to apply.
    Success,
    /// The target equals the current name; Go's grey row, and *not* a conflict.
    Unchanged,
    /// Empty, already present in the folder, or claimed by another row.
    Conflict,
}

/// One preview row — Go's `{original, preview, isDir, status, reason}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewRow {
    /// The current name.
    pub original: String,
    /// The proposed name (`""` when the rules produced nothing, which is a conflict).
    pub preview: String,
    /// Carried through for the row's icon.
    pub is_dir: bool,
    /// [`RowStatus`].
    pub status: RowStatus,
    /// Go's `reason`, verbatim wording (`"Filename cannot be empty"`, `"File already exists in folder"`,
    /// `"Duplicate target name collision"`); empty for the two non-conflict statuses.
    pub reason: String,
}

/// The whole preview: rows in selection order, plus the one panel-level fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewSet {
    /// One entry per input row, same order.
    pub rows: Vec<PreviewRow>,
    /// Go's `regexError`: a bad pattern is reported once for the panel and leaves every target
    /// unmodified (`Unchanged` for every row, which is what the browser showed too).
    pub regex_error: Option<String>,
}

impl PreviewSet {
    /// Rows the user has to fix before applying.
    pub fn conflicts(&self) -> impl Iterator<Item = &PreviewRow> {
        self.rows.iter().filter(|r| r.status == RowStatus::Conflict)
    }

    /// `true` when at least one row conflicts.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        self.rows.iter().any(|r| r.status == RowStatus::Conflict)
    }

    /// The first conflict, for the warning strip (Go's `previews.find(p => p.status === 'conflict')`).
    #[must_use]
    pub fn first_conflict(&self) -> Option<&PreviewRow> {
        self.rows.iter().find(|r| r.status == RowStatus::Conflict)
    }

    /// Rows that would change something — the dialog's "N items will be renamed".
    #[must_use]
    pub fn changed(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == RowStatus::Success)
            .count()
    }
}

/// The compiled form of the `find` field.
enum Engine {
    /// Literal, case-sensitive: a plain string replace, which is byte-exact JS parity and needs no
    /// engine at all.
    Literal,
    /// A literal with the case flag: still not a pattern — Go builds `new RegExp(escape(find), "gi")`,
    /// which matches exactly the case-insensitive occurrences of the text, so a scan does it in one pass
    /// with no allocation per row (the difference between 4 ms and 2 ms at 10 k rows).
    Fold(String),
    /// A real pattern.
    Pattern(pattern::Regex),
}

/// Compile `find` under the rule flags.
///
/// # Errors
/// [`CoreError::BadRegex`] for a pattern this engine does not implement (Go's `Invalid regex` 400,
/// golden `err-search-bad-regex.json`).
fn compile(rules: &Rules) -> Result<Option<Engine>, CoreError> {
    if rules.find.is_empty() {
        return Ok(None);
    }
    if !rules.regex && !rules.ignore_case {
        return Ok(Some(Engine::Literal));
    }
    if !rules.regex {
        return Ok(Some(Engine::Fold(rules.find.clone())));
    }
    let text = if rules.regex {
        rules.find.clone()
    } else {
        pattern::escape(&rules.find)
    };
    pattern::Regex::new(&text, rules.ignore_case)
        .map(|re| Some(Engine::Pattern(re)))
        .map_err(|reason| CoreError::BadRegex {
            pattern: rules.find.clone(),
            reason,
        })
}

/// Pure preview (plan §7 step 14: "batch-rename preview recompute under 2 ms at 10 k rows").
///
/// `existing` is the *directory's* listing, selected names included — the difference is Go's
/// `otherExistingNames` and is computed here, not by the caller. A pattern that will not compile is not
/// an error: it is [`PreviewSet::regex_error`] plus an untouched set of targets, exactly as the browser
/// behaved.
#[must_use]
pub fn preview(rows: &[Row], rules: &Rules, existing: &HashSet<String>) -> PreviewSet {
    let engine = match compile(rules) {
        Ok(engine) => (engine, None),
        Err(e) => (None, Some(e.user_message())),
    };
    let (engine, regex_error) = engine;
    let (start, step) = rules.counter();
    // A failed compile kills the replacement step and nothing else (Go's `findText && regexError` arm).
    let replace_live = regex_error.is_none();
    // Three passes, and the shape is deliberate: pass one computes the targets (the only place a name is
    // built), pass two classifies them through borrowed `&str`s, pass three moves the owned strings into
    // the rows. The single-pass shape has to *clone* every target into a `HashSet<String>` to notice two
    // rows picking one name, and at 10 k rows those clones were a quarter of the plan's whole budget.
    let mut targets: Vec<String> = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let ok = i64::try_from(index).unwrap_or(i64::MAX);
        let counter = start.saturating_add(ok.saturating_mul(step));
        targets.push(rename_one(
            row,
            rules,
            engine.as_ref(),
            replace_live,
            counter,
        ));
    }
    let selected: HashSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    let mut used: HashSet<&str> = HashSet::with_capacity(rows.len());
    let mut out = Vec::with_capacity(rows.len());
    for (row, target) in rows.iter().zip(&targets) {
        // Go's `otherExistingNames`, computed on the fly: on disk *and* not part of this batch.
        let on_disk = existing.contains(target.as_str()) && !selected.contains(target.as_str());
        let (status, reason) = if target.is_empty() {
            (RowStatus::Conflict, "Filename cannot be empty".to_string())
        } else if *target == row.name {
            (RowStatus::Unchanged, String::new())
        } else if on_disk {
            (
                RowStatus::Conflict,
                "File already exists in folder".to_string(),
            )
        } else if used.contains(target.as_str()) {
            (
                RowStatus::Conflict,
                "Duplicate target name collision".to_string(),
            )
        } else {
            (RowStatus::Success, String::new())
        };
        used.insert(target.as_str());
        out.push(PreviewRow {
            original: row.name.clone(),
            preview: String::new(),
            is_dir: row.is_dir,
            status,
            reason,
        });
    }
    for (slot, target) in out.iter_mut().zip(targets) {
        slot.preview = target;
    }
    PreviewSet {
        rows: out,
        regex_error,
    }
}

/// The per-row transform: split, replace, counter, rejoin.
fn rename_one(
    row: &Row,
    rules: &Rules,
    engine: Option<&Engine>,
    replace_live: bool,
    counter: i64,
) -> String {
    let (stem, ext) = split_name(&row.name, row.is_dir);
    let (mut stem, mut ext) = (stem.to_string(), ext.to_string());
    if replace_live {
        if rules.include_extension {
            let full = replace(&row.name, engine, rules);
            let (s, e) = split_name(&full, row.is_dir);
            stem = s.to_string();
            ext = e.to_string();
        } else {
            stem = replace(stem.as_str(), engine, rules);
        }
    }
    if let Some(text) = expand_counter(&stem, counter) {
        stem = text;
    }
    if let Some(text) = expand_counter(&ext, counter) {
        ext = text;
    }
    if rules.include_extension {
        return stem;
    }
    // One buffer, not a `format!` — the target string is the only allocation this row needs beyond the
    // replacement itself, and 10 k of them are the plan's budget.
    stem.push_str(&ext);
    stem
}

/// Go's split: `lastDot > 0` on a *file* only, so `.hidden` and `a.` keep their whole name in the stem,
/// and a directory never splits.
fn split_name(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

fn replace(text: &str, engine: Option<&Engine>, rules: &Rules) -> String {
    match engine {
        Some(Engine::Pattern(re)) => re.replace_all(text, &rules.replace),
        Some(Engine::Fold(needle)) => replace_fold(text, needle, &rules.replace),
        Some(Engine::Literal) => text.replace(rules.find.as_str(), rules.replace.as_str()),
        None => text.to_string(),
    }
}

/// Case-insensitive replace-all, `str::replace` semantics (non-overlapping, left to right).
fn replace_fold(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = find_fold(rest, needle) {
        out.push_str(&rest[..at]);
        out.push_str(replacement);
        rest = &rest[at + needle.len()..];
    }
    out.push_str(rest);
    out
}

/// The byte index of the first case-insensitive occurrence of `needle` (simple fold: `a`/`A`-style, plus
/// any character whose lowercase mapping agrees).
fn find_fold(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    let mut start = 0;
    while start + n.len() <= h.len() {
        if h[start].eq_ignore_ascii_case(&n[0]) && begins_fold(hay[start..].into(), needle) {
            return Some(start);
        }
        start += 1;
        while start < h.len() && !hay.is_char_boundary(start) {
            start += 1;
        }
    }
    None
}

/// `true` when `head` starts with exactly `needle`, case-insensitively.
fn begins_fold(head: &str, needle: &str) -> bool {
    let mut chars = head.chars();
    for want in needle.chars() {
        let Some(got) = chars.next() else {
            return false;
        };
        if !(got == want || got.to_lowercase().eq(want.to_lowercase())) {
            return false;
        }
    }
    true
}

/// `[n]` and `[n:W]`, in both halves; `W` zero-pads (Go's `padStart(width, '0')`).
fn expand_counter(text: &str, counter: i64) -> Option<String> {
    if !text.contains("[n") {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'['
            && let Some(end) = text[i..].find(']')
        {
            let inner = &text[i + 1..i + end];
            if let Some(rendered) = counter_token(inner, counter) {
                out.push_str(&rendered);
                i += end + 1;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\u{0}');
        out.push(ch);
        i += ch.len_utf8();
    }
    Some(out)
}

/// `n` → the counter, `n:W` → the zero-padded counter; `None` for anything else (which is copied
/// verbatim, so a name containing `[x]` or `[n:x]` is untouched).
fn counter_token(inner: &str, counter: i64) -> Option<String> {
    let rest = inner.strip_prefix('n')?;
    if rest.is_empty() {
        return Some(counter.to_string());
    }
    let width: usize = rest.strip_prefix(':')?.parse().ok()?;
    if width == 0 {
        return None;
    }
    let text = counter.to_string();
    Some(if text.len() >= width {
        text
    } else {
        format!("{}{}", "0".repeat(width - text.len()), text)
    })
}

/// The rows a batch apply will submit: `(source, new name)`, in selection order, unchanged rows left out.
///
/// With [`ConflictPolicy::Rename`] a conflicting row is given the first free `(2)`-style suffix computed
/// against the *same* in-memory name set, which is what makes two rows racing for one name a
/// deterministic pair rather than a clobber.
#[must_use]
pub fn apply_request(
    rows: &[Row],
    preview: &PreviewSet,
    existing: &HashSet<String>,
    policy: ConflictPolicy,
) -> (RenameReq, Vec<String>) {
    let mut taken: HashSet<String> = existing.clone();
    let mut sources = Vec::new();
    let mut names = Vec::new();
    let mut skipped = Vec::new();
    for (row, pre) in rows.iter().zip(&preview.rows) {
        match pre.status {
            RowStatus::Unchanged => {}
            RowStatus::Success => {
                taken.insert(pre.preview.clone());
                sources.push(row.path.clone());
                names.push(pre.preview.clone());
            }
            RowStatus::Conflict if pre.preview.is_empty() => skipped.push(pre.original.clone()),
            RowStatus::Conflict if policy == ConflictPolicy::Rename => {
                let name = disambiguate(&mut taken, &pre.preview, pre.is_dir);
                sources.push(row.path.clone());
                names.push(name);
            }
            RowStatus::Conflict => skipped.push(pre.original.clone()),
        }
    }
    let req = RenameReq {
        sources,
        target: RenameTarget::Batch(names),
        policy,
    };
    (req, skipped)
}

/// `name (2)`, `name (3)`, … before the extension, against `taken` (and inserting the winner). The
/// in-memory twin of [`crate::ops::unique_target`], which cannot see the other rows of this batch.
fn disambiguate(taken: &mut HashSet<String>, name: &str, is_dir: bool) -> String {
    let (stem, ext) = split_name(name, is_dir);
    let mut n = 2_u32;
    let candidate = loop {
        let candidate = format!("{stem} ({n}){ext}");
        if !taken.contains(&candidate) {
            break candidate;
        }
        n = n.saturating_add(1);
    };
    taken.insert(candidate.clone());
    candidate
}

/// What the apply step produced.
#[derive(Clone, Debug)]
pub struct ApplyReport {
    /// The preview the user was shown, so a toast can name the row that failed.
    pub preview: PreviewSet,
    /// Names skipped because they conflicted under a policy that does not disambiguate.
    pub skipped: Vec<String>,
    /// How many rows were submitted.
    pub renamed_count: usize,
}

/// Build the [`RenameReq`] for an apply (plan §5.3: the batch is one call, not one per row).
///
/// # Errors
/// [`CoreError::Empty`] when nothing would change — Go's `changedIndexes.length === 0` early return,
/// which the port reports instead of silently closing the dialog.
pub fn apply(
    rows: &[Row],
    preview: &PreviewSet,
    existing: &HashSet<String>,
    policy: ConflictPolicy,
) -> Result<(RenameReq, ApplyReport), CoreError> {
    let (req, skipped) = apply_request(rows, preview, existing, policy);
    if req.sources.is_empty() {
        return Err(CoreError::Empty {
            what: "batch rename selection",
        });
    }
    let count = req.sources.len();
    Ok((
        req,
        ApplyReport {
            preview: preview.clone(),
            skipped,
            renamed_count: count,
        },
    ))
}

// --------------------------------------------------------------------------------------------- pattern
//
// A backtracking matcher over `char`s: compiled to a small instruction list (`Save`/`Split`/`Char`/…),
// run recursively. It exists because the workspace has no regex crate and rule "no new packages" keeps
// it that way — the same trade step 11 made in `services/name_match.rs`, and the reason it is
// deliberately *small*: only what the rename dialog can be asked for.

/// The rename pattern engine (see the module docs of [`crate::ops::batch_rename`]).
///
/// A compiled program of tiny instructions, run by a recursive backtracker with explicit `Save` slots —
/// the textbook Thompson-plus-backtracking shape, in ~200 lines, which is the whole argument for
/// writing it here instead of taking a dependency.
pub mod pattern {
    /// One instruction of the compiled program.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Inst {
        /// One character (compared with the `i` flag in force).
        Char(char),
        /// `.` — any character but a newline.
        Any,
        /// `[…]` or `[^…]`, over inclusive ranges.
        Class {
            negated: bool,
            lo: char,
            hi: char,
        },
        /// `^`.
        Start,
        /// `$`.
        End,
        /// Try `a` first, then `b`.
        Split(usize, usize),
        /// Record the position in capture slot `n` (slot `2g` opens group `g`, `2g+1` closes it).
        Save(usize),
        Jmp(usize),
        Accept,
    }

    /// A compiled pattern.
    #[derive(Clone, Debug)]
    pub struct Regex {
        prog: Vec<Inst>,
        slots: usize,
        ignore_case: bool,
        source: String,
    }

    /// A whole-match plus its numbered captures, all as character spans.
    #[derive(Clone, Debug)]
    pub struct Found {
        /// Start index, in characters.
        pub start: usize,
        /// End index, in characters.
        pub end: usize,
        slots: Vec<Option<usize>>,
    }

    impl Found {
        /// Group `n`'s span, `None` when the group did not take part.
        #[must_use]
        pub fn group(&self, n: usize) -> Option<(usize, usize)> {
            let lo = *self.slots.get(n * 2)?.as_ref()?;
            let hi = *self.slots.get(n * 2 + 1)?.as_ref()?;
            (lo <= hi).then_some((lo, hi))
        }
    }

    impl Regex {
        /// Compile. `ignore_case` is JavaScript's `i` flag.
        ///
        /// # Errors
        /// A human-readable reason — the caller turns it into [`CoreError::BadRegex`](crate::error::CoreError::BadRegex).
        pub fn new(source: &str, ignore_case: bool) -> Result<Self, String> {
            let mut parser = Parser {
                chars: source.chars().collect(),
                at: 0,
                groups: 0,
            };
            let tree = parser.alternation()?;
            if parser.at < parser.chars.len() {
                return Err(format!("unexpected {:?}", parser.chars[parser.at]));
            }
            let mut b = Builder::default();
            b.push(Inst::Save(0));
            b.emit(&tree)?;
            b.push(Inst::Save(1));
            b.push(Inst::Accept);
            Ok(Self {
                prog: b.prog,
                slots: (b.groups + 1) * 2,
                ignore_case,
                source: source.to_string(),
            })
        }

        /// The pattern text, for an error message.
        #[must_use]
        pub fn as_str(&self) -> &str {
            &self.source
        }

        fn same(&self, a: char, b: char) -> bool {
            if a == b {
                return true;
            }
            self.ignore_case
                && (lower(a) == lower(b) || upper(a) == b || upper(b) == a || lower(a) == b)
        }

        fn in_range(&self, c: char, lo: char, hi: char) -> bool {
            let cases = if self.ignore_case {
                [Some(c), Some(lower(c)), Some(upper(c))]
            } else {
                [Some(c), None, None]
            };
            cases.into_iter().flatten().any(|v| v >= lo && v <= hi)
        }

        fn step(
            &self,
            i: usize,
            input: &[char],
            pos: usize,
            slots: &mut [Option<usize>],
        ) -> Option<usize> {
            match self.prog[i] {
                Inst::Accept => Some(pos),
                Inst::Jmp(t) => self.step(t, input, pos, slots),
                Inst::Split(a, b) => self
                    .step(a, input, pos, slots)
                    .or_else(|| self.step(b, input, pos, slots)),
                Inst::Save(n) => {
                    let old = slots[n];
                    slots[n] = Some(pos);
                    let out = self.step(i + 1, input, pos, slots);
                    if out.is_none() {
                        slots[n] = old;
                    }
                    out
                }
                Inst::Start => (pos == 0).then(|| self.step(i + 1, input, pos, slots))?,
                Inst::End => (pos == input.len()).then(|| self.step(i + 1, input, pos, slots))?,
                Inst::Any => (pos < input.len() && input[pos] != '\n')
                    .then(|| self.step(i + 1, input, pos + 1, slots))?,
                Inst::Char(c) => (pos < input.len() && self.same(input[pos], c))
                    .then(|| self.step(i + 1, input, pos + 1, slots))?,
                Inst::Class { negated, lo, hi } => {
                    let hit = pos < input.len() && self.in_range(input[pos], lo, hi);
                    (hit != negated).then(|| self.step(i + 1, input, pos + 1, slots))?
                }
            }
        }

        fn try_at(
            &self,
            input: &[char],
            from: usize,
            slots: &mut [Option<usize>],
        ) -> Option<Found> {
            for pos in from..=input.len() {
                slots.iter_mut().for_each(|s| *s = None);
                if let Some(end) = self.step(0, input, pos, slots) {
                    return Some(Found {
                        start: pos,
                        end,
                        slots: slots.to_owned(),
                    });
                }
            }
            None
        }

        /// The leftmost match at or after `from` characters into `text`.
        #[must_use]
        pub fn find(&self, text: &str, from: usize) -> Option<Found> {
            let input: Vec<char> = text.chars().collect();
            let mut slots = vec![None; self.slots];
            self.try_at(&input, from.min(input.len()), &mut slots)
        }

        /// `true` when the pattern matches anywhere in `text`.
        #[must_use]
        pub fn is_match(&self, text: &str) -> bool {
            self.find(text, 0).is_some()
        }

        /// Replace every match with `replacement`, whose `$0`/`$&` is the whole match, `$1`…`$99` are
        /// the captures and `$$` a literal dollar.
        #[must_use]
        pub fn replace_all(&self, text: &str, replacement: &str) -> String {
            let input: Vec<char> = text.chars().collect();
            let mut out = String::with_capacity(text.len());
            let mut i = 0;
            let mut guard = 0;
            let mut slots = vec![None; self.slots];
            // The common keystroke case: a replacement with no `$` in it needs neither the matched
            // text nor an expansion pass, so neither is built.
            let literal = !replacement.contains('$');
            while i <= input.len() && guard < 1_000_000 {
                guard += 1;
                match self.try_at(&input, i, &mut slots) {
                    Some(found) => {
                        out.extend(input[i..found.start].iter().copied());
                        if literal {
                            out.push_str(replacement);
                        } else {
                            let matched: String = input[found.start..found.end].iter().collect();
                            out.push_str(&expand(replacement, &matched, &found, &input));
                        }
                        if found.end > found.start {
                            i = found.end;
                        } else if found.start < input.len() {
                            out.push(input[found.start]);
                            i = found.start + 1;
                        } else {
                            break;
                        }
                    }
                    None => {
                        out.extend(input[i..].iter().copied());
                        break;
                    }
                }
            }
            out
        }
    }

    fn lower(c: char) -> char {
        c.to_lowercase().next().unwrap_or(c)
    }

    fn upper(c: char) -> char {
        c.to_uppercase().next().unwrap_or(c)
    }

    /// One match's replacement expansion.
    fn expand(replacement: &str, matched: &str, found: &Found, input: &[char]) -> String {
        let mut out = String::with_capacity(replacement.len());
        let chars: Vec<char> = replacement.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            i += 1;
            if c != '$' {
                out.push(c);
                continue;
            }
            let Some(&next) = chars.get(i) else {
                out.push('$');
                break;
            };
            if next == '$' {
                out.push('$');
                i += 1;
                continue;
            }
            if next == '&' {
                out.push_str(matched);
                i += 1;
                continue;
            }
            if next.is_ascii_digit() {
                let mut digits = String::new();
                while digits.len() < 2 && chars.get(i).is_some_and(|d| d.is_ascii_digit()) {
                    digits.push(chars[i]);
                    i += 1;
                }
                let n: usize = digits.parse().unwrap_or(0);
                if let Some((lo, hi)) = found.group(n) {
                    out.extend(input[lo..hi].iter().copied());
                }
                continue;
            }
            out.push('$');
        }
        out
    }

    /// Escape a literal so the engine reads it as text — Go's
    /// `find.replace(/[-\/\\^$*+?.()|[\]{}]/g, '\\$&')`.
    #[must_use]
    pub fn escape(text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 8);
        for c in text.chars() {
            if "^$.*+?()[]{}|\\/".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    #[derive(Clone, Debug)]
    enum Node {
        Char(char),
        Any,
        Start,
        End,
        /// One inclusive range; a class with several ranges or shorthands becomes an alternation.
        Range {
            lo: char,
            hi: char,
            negated: bool,
        },
        Group {
            inner: Box<Node>,
            index: usize,
        },
        Concat(Vec<Node>),
        Alt(Vec<Node>),
        Repeat {
            node: Box<Node>,
            min: u32,
            max: Option<u32>,
            greedy: bool,
        },
    }

    #[derive(Default)]
    struct Builder {
        prog: Vec<Inst>,
        groups: usize,
    }

    impl Builder {
        fn push(&mut self, inst: Inst) -> usize {
            self.prog.push(inst);
            self.prog.len() - 1
        }

        fn emit(&mut self, node: &Node) -> Result<(), String> {
            match node {
                Node::Char(c) => {
                    self.push(Inst::Char(*c));
                }
                Node::Any => {
                    self.push(Inst::Any);
                }
                Node::Start => {
                    self.push(Inst::Start);
                }
                Node::End => {
                    self.push(Inst::End);
                }
                Node::Range { lo, hi, negated } => {
                    self.push(Inst::Class {
                        negated: *negated,
                        lo: *lo,
                        hi: *hi,
                    });
                }
                Node::Concat(items) => {
                    for item in items {
                        self.emit(item)?;
                    }
                }
                Node::Group { inner, index } => {
                    self.groups = self.groups.max(*index);
                    self.push(Inst::Save(index * 2));
                    self.emit(inner)?;
                    self.push(Inst::Save(index * 2 + 1));
                }
                Node::Alt(alts) => {
                    let mut jumps = Vec::new();
                    for (n, branch) in alts.iter().enumerate() {
                        if n + 1 == alts.len() {
                            self.emit(branch)?;
                            break;
                        }
                        let split = self.push(Inst::Split(0, 0));
                        self.emit(branch)?;
                        let jmp = self.push(Inst::Jmp(0));
                        let next = self.prog.len();
                        self.prog[split] = Inst::Split(split + 1, next);
                        jumps.push(jmp);
                    }
                    let end = self.prog.len();
                    for jmp in jumps {
                        self.prog[jmp] = Inst::Jmp(end);
                    }
                }
                Node::Repeat {
                    node,
                    min,
                    max,
                    greedy,
                } => {
                    for _ in 0..*min {
                        self.emit(node)?;
                    }
                    match max {
                        None => {
                            let head = self.prog.len();
                            let split = self.push(Inst::Split(0, 0));
                            self.emit(node)?;
                            self.push(Inst::Jmp(head));
                            let after = self.prog.len();
                            self.prog[split] = if *greedy {
                                Inst::Split(split + 1, after)
                            } else {
                                Inst::Split(after, split + 1)
                            };
                        }
                        Some(max) => {
                            let mut splits = Vec::new();
                            for _ in *min..*max {
                                splits.push(self.push(Inst::Split(0, 0)));
                                self.emit(node)?;
                            }
                            let end = self.prog.len();
                            for split in splits {
                                self.prog[split] = if *greedy {
                                    Inst::Split(split + 1, end)
                                } else {
                                    Inst::Split(end, split + 1)
                                };
                            }
                        }
                    }
                }
            }
            Ok(())
        }
    }

    struct Parser {
        chars: Vec<char>,
        at: usize,
        groups: usize,
    }

    impl Parser {
        fn peek(&self) -> Option<char> {
            self.chars.get(self.at).copied()
        }

        fn eat(&mut self, c: char) -> bool {
            if self.peek() == Some(c) {
                self.at += 1;
                return true;
            }
            false
        }

        fn alternation(&mut self) -> Result<Node, String> {
            let mut alts = vec![self.concatenation()?];
            while self.eat('|') {
                alts.push(self.concatenation()?);
            }
            if alts.len() == 1 {
                return Ok(alts.swap_remove(0));
            }
            Ok(Node::Alt(alts))
        }

        fn concatenation(&mut self) -> Result<Node, String> {
            let mut items = Vec::new();
            while let Some(c) = self.peek() {
                if c == '|' || c == ')' {
                    break;
                }
                items.push(self.quantified()?);
            }
            Ok(Node::Concat(items))
        }

        fn quantified(&mut self) -> Result<Node, String> {
            let atom = self.atom()?;
            let Some(c) = self.peek() else {
                return Ok(atom);
            };
            let (min, max) = match c {
                '*' => (0, None),
                '+' => (1, None),
                '?' => (0, Some(1)),
                _ => return Ok(atom),
            };
            self.at += 1;
            let greedy = !self.eat('?');
            Ok(Node::Repeat {
                node: Box::new(atom),
                min,
                max,
                greedy,
            })
        }

        fn atom(&mut self) -> Result<Node, String> {
            let Some(c) = self.eat_any() else {
                return Err("pattern ended early".to_string());
            };
            match c {
                '(' => {
                    if self.peek() == Some('?') {
                        return Err(
                            "named groups, lookaround and flags are not supported".to_string()
                        );
                    }
                    self.groups += 1;
                    let index = self.groups;
                    let inner = self.alternation()?;
                    if !self.eat(')') {
                        return Err("missing closing ')'".to_string());
                    }
                    Ok(Node::Group {
                        inner: Box::new(inner),
                        index,
                    })
                }
                ')' => Err("unmatched ')'".to_string()),
                '[' => self.class(),
                '^' => Ok(Node::Start),
                '$' => Ok(Node::End),
                '.' => Ok(Node::Any),
                '\\' => self.escape(),
                '{' | '}' | '*' | '+' | '?' => Err(format!("nothing to repeat before {c:?}")),
                other => Ok(Node::Char(other)),
            }
        }

        fn eat_any(&mut self) -> Option<char> {
            let c = self.peek()?;
            self.at += 1;
            Some(c)
        }

        fn escape(&mut self) -> Result<Node, String> {
            let Some(c) = self.eat_any() else {
                return Err("trailing backslash".to_string());
            };
            Ok(match c {
                'd' | 'D' | 'w' | 's' => {
                    let negated = c.is_ascii_uppercase();
                    let ranges: &[(char, char)] = match c {
                        'd' | 'D' => &[('0', '9')],
                        'w' => &[('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
                        _ => &[(' ', ' '), ('\t', '\r')],
                    };
                    self.ranges(ranges, negated)
                }
                'W' => self.ranges(&[('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')], true),
                'S' => self.ranges(&[(' ', ' '), ('\t', '\r')], true),
                'b' | 'B' | 'k' | 'p' | 'u' | 'x' | 'c' | 'q' | '1'..='9' => {
                    return Err(format!("\\{c} is not supported by the rename engine"));
                }
                other => Node::Char(other),
            })
        }

        /// Several ranges as an alternation of one-range classes.
        fn ranges(&self, ranges: &[(char, char)], negated: bool) -> Node {
            let nodes: Vec<Node> = ranges
                .iter()
                .map(|(lo, hi)| Node::Range {
                    lo: *lo,
                    hi: *hi,
                    negated,
                })
                .collect();
            if nodes.len() == 1 {
                return Node::Concat(vec![nodes[0].clone()]);
            }
            Node::Alt(nodes)
        }

        fn class(&mut self) -> Result<Node, String> {
            let negated = self.eat('^');
            let mut items = Vec::new();
            let mut first = true;
            loop {
                let Some(c) = self.eat_any() else {
                    return Err("missing closing ']'".to_string());
                };
                if c == ']' && !first {
                    break;
                }
                first = false;
                let base = if c == '\\' {
                    match self.eat_any() {
                        Some('d') => {
                            items.extend([('0', '9')]);
                            continue;
                        }
                        Some('w') => {
                            items.extend([('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')]);
                            continue;
                        }
                        Some('s') => {
                            items.extend([(' ', ' '), ('\t', '\r')]);
                            continue;
                        }
                        Some(other) => other,
                        None => return Err("trailing backslash in class".to_string()),
                    }
                } else {
                    c
                };
                let ranged = self.peek() == Some('-')
                    && self.chars.get(self.at + 1).is_some_and(|n| *n != ']');
                if ranged {
                    self.at += 1;
                    let hi = self
                        .eat_any()
                        .ok_or_else(|| "missing class end".to_string())?;
                    let hi = if hi == '\\' {
                        self.eat_any()
                            .ok_or_else(|| "trailing backslash".to_string())?
                    } else {
                        hi
                    };
                    items.push((base, hi));
                } else {
                    items.push((base, base));
                }
            }
            if items.is_empty() {
                return Err("empty character class".to_string());
            }
            Ok(self.ranges(&items, negated))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, is_dir: bool) -> Row {
        Row::new(
            AbsPath::from_path(format!("/d/{name}")).expect("test path"),
            name,
            is_dir,
        )
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn names(preview: &PreviewSet) -> Vec<&str> {
        preview.rows.iter().map(|r| r.preview.as_str()).collect()
    }

    #[test]
    fn split_keeps_leading_dot_and_directories_whole() {
        assert_eq!(split_name(".hidden", false), (".hidden", ""));
        assert_eq!(split_name("a.txt", false), ("a", ".txt"));
        assert_eq!(
            split_name("a.", false),
            ("a", "."),
            "Go splits at `lastDot > 0`, so the trailing dot is the extension"
        );
        assert_eq!(split_name("dir.txt", true), ("dir.txt", ""));
    }

    #[test]
    fn counter_tokens_pad_and_step() {
        let run = |text: &str, n: i64| expand_counter(text, n).unwrap_or_else(|| text.to_string());
        assert_eq!(run("img[n]", 7), "img7");
        assert_eq!(run("img[n:3]", 7), "img007");
        assert_eq!(run("[n]-[n:4]", 12), "12-0012");
        assert_eq!(run("plain", 1), "plain");
        assert_eq!(run("[x]", 1), "[x]", "not a counter token");
        assert_eq!(run("[n:x]", 1), "[n:x]", "a bad width is a literal");
    }

    #[test]
    fn the_pattern_engine_matches_the_rename_subset() {
        let re = pattern::Regex::new(r"^photo (\d)\.jpg$", false).expect("compiles");
        assert_eq!(re.replace_all("photo 1.jpg", "shot$1"), "shot1");
        let re = pattern::Regex::new("[abc]\\.txt", false).expect("compiles");
        assert_eq!(re.replace_all("a.txt b.txt", "X"), "X X");
        let re = pattern::Regex::new(r"^a\d\.txt$", false).expect("compiles");
        assert_eq!(re.replace_all("a7.txt", "taken.txt"), "taken.txt");
        let re = pattern::Regex::new("a+", true).expect("compiles");
        assert_eq!(re.replace_all("Aabca", "z"), "zbcz", "the `i` flag");
        assert!(
            pattern::Regex::new("(", false).is_err(),
            "a bad pattern is a compile error, reported once"
        );
        assert!(
            pattern::Regex::new("(?=x)", false).is_err(),
            "lookaround is refused, not ignored"
        );
        assert_eq!(
            pattern::Regex::new(r"(\w+)\.(\w+)", false)
                .expect("compiles")
                .replace_all("a.md", "$2-$1"),
            "md-a"
        );
        assert_eq!(
            pattern::Regex::new("o", false)
                .expect("compiles")
                .replace_all("foo", "[$&]"),
            "f[o][o]",
            "JS's whole-match reference works here"
        );
    }

    #[test]
    fn the_case_insensitive_literal_arm_is_the_same_rename_as_the_regex_arm() {
        // Go compiles `new RegExp(escape(find), "gi")` for the case box; the port scans for the text
        // instead, so the two must agree.
        let rows = vec![row("Apple.JPG", false), row("a.jpg", false)];
        let existing = set(&["Apple.JPG", "a.jpg"]);
        let fold = preview(
            &rows,
            &Rules {
                find: "a".to_string(),
                replace: "b".to_string(),
                ignore_case: true,
                ..Rules::default()
            },
            &existing,
        );
        assert_eq!(names(&fold), ["bpple.JPG", "b.jpg"]);
        let literal = preview(
            &rows,
            &Rules {
                find: "a".to_string(),
                replace: "b".to_string(),
                ..Rules::default()
            },
            &existing,
        );
        assert_eq!(
            literal.rows[0].preview, "Apple.JPG",
            "case-sensitive does not match `A`"
        );
    }

    #[test]
    fn a_collision_with_a_row_of_the_same_batch_is_a_duplicate_target() {
        let rows = vec![row("aa.txt", false), row("ba.txt", false)];
        let rules = Rules {
            find: "a".to_string(),
            replace: "b".to_string(),
            ..Rules::default()
        };
        let p = preview(&rows, &rules, &set(&["aa.txt", "ba.txt"]));
        assert_eq!(p.rows[0].preview, "bb.txt");
        assert_eq!(p.rows[0].status, RowStatus::Success);
        assert_eq!(p.rows[1].preview, "bb.txt");
        assert_eq!(p.rows[1].status, RowStatus::Conflict);
        assert_eq!(p.rows[1].reason, "Duplicate target name collision");
        assert!(p.has_conflicts());
    }

    #[test]
    fn rename_second_pass_never_overwrites() {
        let rows = vec![row("aa.txt", false), row("ba.txt", false)];
        let rules = Rules {
            find: "a".to_string(),
            replace: "b".to_string(),
            ..Rules::default()
        };
        let existing = set(&["aa.txt", "ba.txt"]);
        let p = preview(&rows, &rules, &existing);
        let (req, skipped) = apply_request(&rows, &p, &existing, ConflictPolicy::Rename);
        assert!(skipped.is_empty());
        assert_eq!(
            req.target.names().join(","),
            "bb.txt,bb (2).txt",
            "deterministic order, no clobber"
        );
        let (skip_req, skip_skipped) =
            apply_request(&rows, &p, &existing, ConflictPolicy::SkipExisting);
        assert_eq!(
            skip_req.target.names().join(","),
            "bb.txt",
            "the conflicting row is reported, not dropped in silence"
        );
        assert_eq!(skip_skipped, ["ba.txt"]);
    }

    #[test]
    fn a_bad_pattern_leaves_every_target_untouched_and_says_why() {
        let rows = vec![row("a.txt", false)];
        let rules = Rules {
            find: "(".to_string(),
            regex: true,
            ..Rules::default()
        };
        let p = preview(&rows, &rules, &set(&["a.txt"]));
        assert!(p.regex_error.is_some(), "the panel shows the compile error");
        assert_eq!(p.rows[0].preview, "a.txt");
        assert_eq!(p.rows[0].status, RowStatus::Unchanged);
    }

    #[test]
    fn an_empty_target_is_a_conflict_not_a_delete() {
        let rows = vec![row("a.txt", false)];
        let rules = Rules {
            find: "a.txt".to_string(),
            include_extension: true,
            ..Rules::default()
        };
        let p = preview(&rows, &rules, &set(&["a.txt"]));
        assert_eq!(p.rows[0].status, RowStatus::Conflict);
        assert_eq!(p.rows[0].reason, "Filename cannot be empty");
    }
}
