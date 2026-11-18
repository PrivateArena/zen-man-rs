//! The source-text gates `cargo xtask ci` runs over `crates/zenman-ui/src` (rule 7, plan §5.7).
//!
//! Both guards are pure functions of a file's text, so `cargo test -p xtask` can feed them the
//! snippets they must catch and the snippets they must pass — a gate nobody can demonstrate is a
//! gate nobody can trust. The file walking, `file:line` naming and error text live in `main.rs`.
//!
//! Every scan runs on [`code_lines`]: comments and string/char literals blanked out, line numbers
//! and columns preserved. Without that a doc comment that mentions `std::fs::` (this repository has
//! several) fails the IO guard and a `"1024"` string fails the geometry guard — both of which is
//! noise that trains people to skim the gate output.
//!
//! The `#[cfg(test)]` exemption is **scope-aware**: an attribute exempts the item it is attached to
//! — the attribute line through that item's own closing brace — and nothing after it. The old
//! "exempt to end of file" rule let a mid-file test module whitelist every line below it, which is
//! exactly how an IO call sneaks into production code.

use std::collections::BTreeSet;

/// The smallest magnitude this gate calls a *measurement*. `0.0`, `1.0`, `2.0` and every ratio
/// below it are arithmetic identities (`Stroke::new(1.0, …)`, `x / 2.0`, `scale * 1.5`), not points
/// off the token table.
pub const MIN_MEASUREMENT: f64 = 3.0;

/// egui constructors that take points in an **integer** type. Outside these calls an integer in
/// this crate is a count or an index (the dock slot order, `MRU_CAP`, the four-pane check), which
/// is why the magnitude rule applies to floats everywhere and to integers only here.
pub const INTEGER_GEOMETRY_CALLS: &[&str] = &[
    "CornerRadius::same(",
    "CornerRadius::new(",
    "Margin::same(",
    "Margin::symmetric(",
    "Margin::from_box(",
];

/// One rejected line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offence {
    /// 1-based line number in the file.
    pub line: usize,
    /// The file's own text of that line, trimmed.
    pub text: String,
}

impl Offence {
    fn new(line: usize, original: &str) -> Self {
        Self {
            line,
            text: original.trim().to_string(),
        }
    }
}

/// Every line with comments and string/char literals replaced by spaces, so line and column
/// numbering are unchanged.
pub fn code_lines(text: &str) -> Vec<String> {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '/' && bytes.get(i + 1) == Some(&'/') {
            // A line comment — including every doc comment, which is how this crate writes about
            // `std::fs::` and about pixel sizes.
            while i < bytes.len() && bytes[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if c == '/' && bytes.get(i + 1) == Some(&'*') {
            let depth_start = i;
            let mut depth = 0usize;
            while i < bytes.len() {
                if bytes[i] == '/' && bytes.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                    out.push_str("  ");
                } else if bytes[i] == '*' && bytes.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    out.push_str("  ");
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(if bytes[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
            debug_assert!(i > depth_start);
        } else if c == '"' || (c == 'r' && quote_is_raw(&bytes, i)) {
            let hashes = if c == 'r' {
                i += 1;
                count_hashes(&bytes, i)
            } else {
                0
            };
            i = blank_string(&bytes, i, hashes, &mut out);
        } else if c == '\'' && literal_char_at(&bytes, i) {
            // A char literal: `'a'`, `'\n'`, `'\u{1F600}'`. A bare `'` (a lifetime) falls through.
            let mut j = i + 1; // the first character of the literal
            if bytes.get(j) == Some(&'\\') {
                j += 1;
                if bytes.get(j) == Some(&'u') {
                    while j < bytes.len() && bytes[j] != '\'' {
                        j += 1;
                    }
                } else {
                    j += 1; // the escaped character
                }
            } else {
                j += 1; // the character itself
            }
            if bytes.get(j) == Some(&'\'') {
                j += 1; // and its closing quote
            }
            while i < j.min(bytes.len()) {
                out.push(' ');
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out.lines().map(str::to_string).collect()
}

fn count_hashes(bytes: &[char], mut i: usize) -> usize {
    let mut n = 0;
    while bytes.get(i) == Some(&'#') {
        n += 1;
        i += 1;
    }
    n
}

fn quote_is_raw(bytes: &[char], i: usize) -> bool {
    bytes.get(i) == Some(&'r')
        && (bytes.get(i + 1) == Some(&'"')
            || (bytes.get(i + 1) == Some(&'#') && bytes.get(i + 2) == Some(&'"')))
}

/// Blank a string literal that starts at `i` (already past any `r`), `hashes` = the raw-string
/// `#` count. Returns the index just past the closing quote.
fn blank_string(bytes: &[char], mut i: usize, hashes: usize, out: &mut String) -> usize {
    if bytes.get(i) == Some(&'r') {
        i += 1;
    }
    for _ in 0..hashes {
        if bytes.get(i) == Some(&'#') {
            i += 1;
        }
    }
    if bytes.get(i) != Some(&'"') {
        out.push(' ');
        return i + 1;
    }
    i += 1;
    out.push(' ');
    loop {
        match bytes.get(i) {
            None => return i,
            Some('"') if (0..hashes).all(|h| bytes.get(i + 1 + h) == Some(&'#')) => {
                out.push(' ');
                return i + 1 + hashes;
            }
            Some('\\') if hashes == 0 => {
                out.push(' ');
                i += 1;
                if i < bytes.len() {
                    out.push(if bytes[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
            Some(c) => {
                out.push(if *c == '\n' { '\n' } else { ' ' });
                i += 1;
            }
        }
    }
}

/// Whether the `'` at `i` opens a char literal rather than a lifetime.
fn literal_char_at(bytes: &[char], i: usize) -> bool {
    let Some(next) = bytes.get(i + 1) else {
        return false;
    };
    if *next == '\\' {
        return true;
    }
    !next.is_alphanumeric() && *next != '_' || bytes.get(i + 2) == Some(&'\'')
}

/// `'`-aware split of a line into top-level `,`-separated segments.
///
/// The lines of `text` with an unbalanced `use std::{…}` import list merged into one logical line, so
/// a multi-line `use std::{\n fs,\n};` is seen whole. Returns `(1-based line, merged text)` pairs
/// plus, for each, the lines it covers (for the test-module exemption).
fn logical_lines(code: &[String]) -> Vec<(usize, usize, String)> {
    let mut out: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;
    while i < code.len() {
        let line = &code[i];
        let opens = line.matches('{').count();
        let closes = line.matches('}').count();
        let wants_merge = line.replace(' ', "").contains("std::{") && opens > closes;
        if !wants_merge {
            out.push((i + 1, i + 1, line.clone()));
            i += 1;
            continue;
        }
        let mut merged = line.clone();
        let mut depth = opens as isize - closes as isize;
        let mut j = i + 1;
        while j < code.len() && depth > 0 {
            merged.push(' ');
            merged.push_str(&code[j]);
            depth += code[j].matches('{').count() as isize - code[j].matches('}').count() as isize;
            j += 1;
        }
        out.push((i + 1, j, merged));
        i = j;
    }
    out
}

/// The 1-based lines covered by each `#[cfg(test)]` item in `text`: the attribute line through that
/// item's own closing brace (or its `;`), never "the rest of the file".
pub fn test_item_lines(text: &str) -> BTreeSet<usize> {
    let code = code_lines(text);
    let mut exempt = BTreeSet::new();
    let mut i = 0;
    while i < code.len() {
        if !code[i]
            .trim_start()
            .replace(' ', "")
            .starts_with("#[cfg(test)]")
        {
            i += 1;
            continue;
        }
        let attr = i;
        let mut depth = 0isize;
        let mut seen_brace = false;
        let mut end = attr;
        'scan: for (line, code_line) in code.iter().enumerate().skip(attr) {
            for c in code_line.chars() {
                if c == ';' && !seen_brace {
                    // `#[cfg(test)] use …;` / `#[cfg(test)] const …` — one item, no block.
                    end = line;
                    break 'scan;
                }
                if c == '{' {
                    seen_brace = true;
                    depth += 1;
                } else if c == '}' && seen_brace {
                    depth -= 1;
                    if depth == 0 {
                        end = line;
                        break 'scan;
                    }
                }
            }
            if line > attr && !seen_brace {
                // A separate item line with no brace and no `;` yet (an attribute on e.g. `fn`
                // without a body on this line): keep scanning for the brace, which is the block.
            }
            end = line;
        }
        for line in attr..=end {
            exempt.insert(line + 1);
        }
        i = end + 1;
    }
    exempt
}

/// Whether this (code-only) line reaches for `std::fs`, in any of the spellings that matter:
/// `std::fs…`, `std::io::fs…`, and an import list naming `fs` (`use std::{fs, io};`,
/// `use std::{io::fs};`).
fn mentions_std_fs(line: &str) -> bool {
    let tight = line.replace(' ', "");
    if tight.contains("std::fs") || tight.contains("std::io::fs") {
        return true;
    }
    let mut rest = tight.as_str();
    while let Some(at) = rest.find("std::{") {
        let list = &rest[at + "std::".len()..];
        if names_fs_list(list) {
            return true;
        }
        rest = &rest[at + "std::".len()..];
    }
    false
}

/// `"{ a, b::c }"` — does any segment name `fs` as a whole path element?
fn names_fs_list(list: &str) -> bool {
    let Some(end) = find_close_brace(list) else {
        return false;
    };
    list[1..end].split(',').any(|segment| {
        // `fs`, `fs as F`, `io::fs`, `self::fs` — the first and last path elements are the ones
        // that can bind the name `fs`.
        let first = segment.split_whitespace().next().unwrap_or("");
        first == "fs" || segment.split("::").any(|part| part == "fs")
    })
}

fn find_close_brace(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (ix, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(ix);
                }
            }
            _ => {}
        }
    }
    None
}

/// Rule 7: `zenman-ui` does no IO; `zenman-core` and `craft-settings` own every file operation
/// (plan §5.7). Test items may build the directories the *engine* opens; production code may not.
pub fn io_offences(text: &str) -> Vec<Offence> {
    let code = code_lines(text);
    let exempt = test_item_lines(text);
    logical_lines(&code)
        .into_iter()
        .filter(|(from, to, _)| !(exempt.contains(from) && exempt.contains(to)))
        .filter(|(_, _, line)| mentions_std_fs(line))
        .map(|(line, _, _)| Offence::new(line, text.lines().nth(line - 1).unwrap_or("")))
        .collect()
}

/// Rule 7's second half and plan §5.7's "the theme owns every measurement": a *point* value — a
/// float of [`MIN_MEASUREMENT`] or more, or an integer of that size inside one of egui's
/// integer-point constructors — must be named. Naming it in `theme.rs`'s token table is the rule;
/// naming it `const` in the widget that alone uses it is the documented local exception (the shape
/// `widgets/disk_meter.rs::BAR_HEIGHT` already had).
pub fn geometry_offences(text: &str) -> Vec<Offence> {
    let code = code_lines(text);
    let exempt = test_item_lines(text);
    let mut out = Vec::new();
    for (ix, line) in code.iter().enumerate() {
        let number = ix + 1;
        if exempt.contains(&number) || is_naming_line(line) {
            continue;
        }
        for (at, value, integral) in numbers(line) {
            if value < MIN_MEASUREMENT {
                continue;
            }
            if integral && !inside_integer_geometry_call(line, at) {
                continue;
            }
            out.push(Offence::new(number, text.lines().nth(ix).unwrap_or("")));
            break; // one report per line is enough to point at the fix
        }
    }
    out
}

/// A line that *declares* a name for a number (`const`/`static`, with or without `pub`), which is
/// what the gate asks for instead of a bare literal.
fn is_naming_line(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let mut word = words.next().unwrap_or("");
    if word == "pub" || word.starts_with("pub(") {
        word = words.next().unwrap_or("");
    }
    word == "const" || word == "static"
}

/// Every numeric literal on a code-only line: `(byte offset, value, is_integer)`. Prefixed by a
/// word character or a `.` (a tuple index, a method name) or followed by an identifier character
/// (a version-ish `2.0` inside `utf8_name`) is not a measurement.
fn numbers(line: &str) -> Vec<(usize, f64, bool)> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        if i > 0
            && (bytes[i - 1].is_ascii_alphanumeric()
                || bytes[i - 1] == b'_'
                || bytes[i - 1] == b'.')
        {
            i += 1;
            continue;
        }
        let start = i;
        let mut integral = true;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
            i += 1;
        }
        if bytes.get(i) == Some(&b'.')
            && bytes
                .get(i + 1)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
            && !bytes[i + 1].is_ascii_digit()
        {
            // `vec.0`, `1.5` — the dot only continues the number when a digit follows.
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            continue;
        } else if bytes.get(i) == Some(&b'.') {
            integral = false;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                i += 1;
            }
        }
        if bytes.get(i) == Some(&b'e') || bytes.get(i) == Some(&b'E') {
            integral = false;
        }
        let token: String = line[start..i].chars().filter(|c| *c != '_').collect();
        let Ok(value) = token.parse::<f64>() else {
            continue;
        };
        out.push((start, value, integral));
    }
    out
}

/// Whether `offset` sits in the argument list of one of [`INTEGER_GEOMETRY_CALLS`] on this line.
fn inside_integer_geometry_call(line: &str, offset: usize) -> bool {
    INTEGER_GEOMETRY_CALLS.iter().any(|needle| {
        let mut search = 0;
        while let Some(at) = line[search..].find(needle) {
            let open = search + at + needle.len();
            if let Some(close) = matching_paren(line, open) {
                if offset >= open && offset < close {
                    return true;
                }
                search = close;
            } else {
                search = open;
            }
        }
        false
    })
}

fn matching_paren(line: &str, open: usize) -> Option<usize> {
    // `open` is the index just past the `(`, so the depth is already one.
    let mut depth = 1isize;
    for (ix, c) in line[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + ix);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caught(text: &str) -> Vec<String> {
        io_offences(text)
            .into_iter()
            .map(|o| format!("{}:{}", o.line, o.text))
            .collect()
    }

    // ---- the IO guard: what it must catch -------------------------------------------------------

    #[test]
    fn io_guard_catches_the_bypasses_the_substring_grep_had() {
        // A bare `use` of the module: no `std::fs::` anywhere, one call later in the file.
        assert_eq!(
            caught("use std::fs;\nfn f() { let _ = fs::read_dir(\"x\"); }\n"),
            vec!["1:use std::fs;"]
        );
        // `std::fs` without `::` — `std::fs` as a path expression.
        assert_eq!(
            caught("fn f() {\n    let _ = std::fs::File::open(\"x\");\n}\n"),
            vec!["2:let _ = std::fs::File::open(\"x\");"]
        );
        assert_eq!(
            caught("fn f() {\n    let _ = std::fs(read_dir);\n}\n"),
            vec!["2:let _ = std::fs(read_dir);"]
        );
        // `std::io::fs` is the same surface under a different module.
        assert_eq!(
            caught("fn f() {\n    let _ = std::io::fs::read(\"x\");\n}\n"),
            vec!["2:let _ = std::io::fs::read(\"x\");"]
        );
        // An import list, on one line and across several.
        assert_eq!(
            caught("use std::{fs, io::Write};\n"),
            vec!["1:use std::{fs, io::Write};"]
        );
        assert_eq!(
            caught("use std::{\n    io::Write,\n    fs,\n};\n"),
            vec!["1:use std::{"]
        );
    }

    #[test]
    fn io_guard_reports_the_line_that_names_the_surface() {
        let offences =
            io_offences("fn a() {}\nfn b() { let _ = std::fs::read_to_string(\"x\"); }\n");
        assert_eq!(offences.len(), 1);
        assert_eq!(offences[0].line, 2);
        assert!(offences[0].text.contains("std::fs::read_to_string"));
    }

    // ---- the IO guard: what it must let through --------------------------------------------------

    #[test]
    fn io_guard_lets_a_test_item_use_the_engine_its_testing() {
        let text = "fn prod() {}\n\n#[cfg(test)]\nmod tests {\n    use std::fs;\n\n    #[test]\n    fn t() {\n        std::fs::write(\"x\", \"\").unwrap();\n    }\n}\n";
        assert_eq!(caught(text), Vec::<String>::new());
    }

    #[test]
    fn io_guard_does_not_exempt_past_the_test_module_closing_brace() {
        // The defect this rewrite exists to close: the old rule exempted everything after the first
        // `#[cfg(test)]`, so this sneaks IO into production code below the test module.
        let text = "#[cfg(test)]\nmod tests {\n    use std::fs;\n}\n\nfn shipped() { let _ = std::fs::read_dir(\".\"); }\n";
        assert_eq!(
            caught(text),
            vec!["6:fn shipped() { let _ = std::fs::read_dir(\".\"); }"]
        );
    }

    #[test]
    fn io_guard_still_reports_process_and_other_io_after_a_test_module() {
        // `std::process` is not IO, but the point of the assertion is the *position*: the line is
        // outside the test item, so the guard reads it at all.
        let text = "#[cfg(test)]\nmod t {\n    #[test]\n    fn a() { let _ = std::fs::read(\"x\"); }\n}\nfn later() { let _ = std::process::id(); let _ = std::fs::read(\"y\"); }\n";
        assert_eq!(
            caught(text),
            vec!["6:fn later() { let _ = std::process::id(); let _ = std::fs::read(\"y\"); }"]
        );
    }

    #[test]
    fn io_guard_ignores_prose_and_strings_about_the_surface() {
        let text = "// this module never calls std::fs:: directly\n/// `std::fs::read_dir` lives in zenman-core\nfn f() { let _ = \"std::fs::read\"; }\n";
        assert_eq!(caught(text), Vec::<String>::new());
    }

    #[test]
    fn io_guard_handles_a_nested_block_and_a_second_test_item() {
        let text = "mod a {\n    #[cfg(test)]\n    fn helper() { let _ = std::fs::read(\"x\"); }\n    fn b() { let _ = std::fs::read(\"y\"); }\n}\n";
        assert_eq!(
            caught(text),
            vec!["4:fn b() { let _ = std::fs::read(\"y\"); }"]
        );
    }

    // ---- the geometry guard ---------------------------------------------------------------------

    #[test]
    fn geometry_guard_catches_unnamed_measurements() {
        let text = "fn f(ui: &mut Ui) { ui.add_space(24.0); }\n";
        let hits = geometry_offences(text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
        // …and an integer where egui takes points.
        let text = "fn f(ui: &mut Ui) { let r = egui::CornerRadius::same(3); }\n";
        assert_eq!(geometry_offences(text).len(), 1);
        // …and the `100_000.0` form.
        let text = "let max = egui::vec2(100_000.0, 100_000.0);";
        assert_eq!(geometry_offences(text).len(), 1);
    }

    #[test]
    fn geometry_guard_allows_structural_values_and_named_ones() {
        let text = "Stroke::new(1.0, tokens.border);\nrect_filled(rect, 0.0, fill);\nlet h = row / 2.0;\nlet s = tokens.font_size * 0.55;\npainter.rect_filled(tab, 2.0, fill);\n";
        assert_eq!(geometry_offences(text), Vec::<Offence>::new());
        let named = "pub const BAR_HEIGHT: f32 = 6.0;\nconst KIB: f64 = 1024.0;\n";
        assert_eq!(geometry_offences(named), Vec::<Offence>::new());
    }

    #[test]
    fn geometry_guard_ignores_prose_strings_and_test_items() {
        let text = "/// the 38 px strip and `rgba(88,101,242,0.15)` tint\nfn f() { let _ = \"40.0\"; }\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = 240.0; }\n}\nfn later() { let _ = 240.0; }\n";
        let hits = geometry_offences(text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 7);
    }

    #[test]
    fn geometry_guard_leaves_indices_and_counts_alone() {
        // Integers outside an integer-point constructor are counts/indices, not points.
        let text = "match slot { _ => 3 }\nfn quad(&self) -> bool { self.panes.len() == 4 }\nconst MRU_CAP: usize = 5;\n";
        assert_eq!(geometry_offences(text), Vec::<Offence>::new());
    }

    #[test]
    fn code_lines_blanks_comments_and_strings_only() {
        let lines =
            code_lines("let a = \"x\"; // std::fs\nlet b = 'q';\n/* block\n still */ let c = 1;\n");
        assert_eq!(lines.len(), 4, "line numbering is preserved");
        assert_eq!(lines[0].replace(' ', ""), "leta=;");
        assert_eq!(lines[1].replace(' ', ""), "letb=;");
        assert_eq!(lines[2].trim(), "");
        assert!(lines[3].trim_start().starts_with("let c = 1;"));
    }

    #[test]
    fn test_item_lines_stops_at_the_items_own_brace() {
        let text = "#[cfg(test)]\nmod t {\n    fn a() {}\n}\nfn b() {}\n";
        let exempt = test_item_lines(text);
        assert_eq!(exempt, BTreeSet::from([1, 2, 3, 4]));
    }
}
