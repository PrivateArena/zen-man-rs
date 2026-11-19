//! The name matcher the live search runs over every entry name (plan §5.3 `services/search_live.rs`
//! row: `pub fn compile_name_match(q, mode) -> Result<NameMatcher, CoreError>`; it lives in its own file
//! because the regex half is a whole engine and a directory walk should not need to be read together
//! with one).
//!
//! # The modes, and where each comes from
//!
//! `api/search.go:79-105` has exactly two behaviours, chosen by `regex=true`:
//!
//! * **substring** — `strings.Fields(strings.ToLower(q))`, and a name matches when its lowercased form
//!   `Contains` **every** token (`:96-105`). An empty (or whitespace-only) query therefore matches
//!   everything, which is why `search-live-empty-q.json` returns the whole directory.
//! * **regex** — `regexp.Compile("(?i)" + q)` then `MatchString(name)` (`:82-91`): Go's **RE2**, always
//!   case-insensitive (the `(?i)` is hardcoded), unanchored, and the name is *not* lowercased.
//!
//! [`NameMode::Word`] is the plan's third spelling and has no Go counterpart in the live path: substring
//! matching with each token required to sit on a word boundary. Documented as a port-side addition
//! (plan §9.2 step-11a).
//!
//! # The regex dialect, honestly stated
//!
//! There is no regex crate anywhere in this workspace's dependency graph (`regex` appears nowhere in
//! `Cargo.lock`) and "no new dependencies" is a hard rule, so the engine is in-tree: a parser for the
//! RE2 subset a filename filter needs, compiled to a Thompson NFA and simulated with a state set (the
//! Pike-VM shape, minus captures). Consequences:
//!
//! * **Linear time, like RE2.** No backtracking, so `a*b*c*d` cannot park the UI thread the way a
//!   backtracker can — the very property that made RE2 safe server-side.
//! * **Boolean matching only.** The search asks `MatchString`, so leftmost-first vs leftmost-longest
//!   submatch semantics never surface; that removes the largest RE2/PCRE divergence class by construction.
//! * **Supported syntax**: literals, `.`, `[...]` classes with ranges/escapes/POSIX names/negation,
//!   `^`/`$`, `(…)`/`(?:…)`/`(?flags)`/`(?flags:…)`, `|`, `*` `+` `?` `{n,m}`, `\d\D\s\S\w\W\b\B`,
//!   `\n\t\r\f\v\a`, `\xNN`, `\x{…}`, `\uNNNN`, `\u{…}`, and `\`-quoted punctuation. Flags `i`, `s`, `m`;
//!   `U` is accepted and ignored (with no captures there is nothing to make ungreedy).
//! * **Refused** (a typed error, never a silent mis-match): named groups, backreferences, look-around,
//!   `\p{…}` property classes, and programs over [`MAX_PROGRAM`].
//! * **Case folding** is `char::to_lowercase`/`to_uppercase` per character where Go uses
//!   `unicode.SimpleFold` orbits. They agree on every ASCII name and on essentially all Unicode letters;
//!   the residue is exotic single-step orbits (U+212A KELVIN SIGN folding to `k`), which Go folds and we
//!   may not. A bounded, recorded divergence.
//! * **Perl classes are ASCII**, as in Go's default flag-free mode (`\d` is `[0-9]`, not `\p{Nd}`), and
//!   so is `\b`.
//! * **Error text** carries Go's `error parsing regexp: …` prefix plus the per-case detail; the case the
//!   goldens pin (`err-search-bad-regex.json`, pattern `[`) is byte-identical, backquoted remainder included.

use serde::Serialize;

use crate::error::CoreError;

/// Cap on the compiled program, mirroring Go's size cap (`expression too large`). Filename filters are
/// tens of instructions; this only refuses hostile patterns.
pub const MAX_PROGRAM: usize = 4096;

/// Cap on a `{n,m}` bound — Go's `maxRepeat` (`regexp/syntax` refuses a count over 1000 with
/// `invalid repeat count`).
pub const MAX_REPEAT: u32 = 1000;

// ------------------------------------------------------------------------------------- public surface

/// How a query becomes a name predicate (`api/search.go:79-105`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NameMode {
    /// Every whitespace-separated token must be a substring of the lowercased name — Go's default.
    #[default]
    Substring,
    /// As [`Self::Substring`], but each token must also sit on a word boundary. Port-side mode.
    Word,
    /// Go's `regex=true`: `regexp.Compile("(?i)" + q)`, therefore case-insensitive by construction.
    Regex,
}

impl NameMode {
    /// The machine spelling (the HTTP form had `regex=true|false` where this has a name).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Substring => "substring",
            Self::Word => "word",
            Self::Regex => "regex",
        }
    }
}

/// Go's token split: `strings.Fields(strings.ToLower(q))` (`api/search.go:96-97`).
///
/// `split_whitespace` and Go's `Fields` both split on the Unicode `White_Space` property, so the token
/// lists agree.
#[must_use]
pub fn tokenize(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// A compiled predicate.
#[derive(Clone, Debug)]
pub enum NameMatcher {
    /// Go's substring path: every token must be `Contains`-able in the lowercased name.
    Substring {
        /// The lowercased tokens; empty means "matches everything" (Go's `len(tokens) == 0`).
        tokens: Vec<String>,
    },
    /// Substring on word boundaries (port-side mode).
    Word {
        /// The lowercased tokens.
        tokens: Vec<String>,
    },
    /// Go's regex path, with `(?i)` already folded in.
    Regex(Regex),
}

impl NameMatcher {
    /// The mode this matcher was compiled with.
    #[must_use]
    pub const fn mode(&self) -> NameMode {
        match self {
            Self::Substring { .. } => NameMode::Substring,
            Self::Word { .. } => NameMode::Word,
            Self::Regex(_) => NameMode::Regex,
        }
    }

    /// `true` when `name` passes. Only the base name is tested, never the path (`api/search.go:127`,
    /// `:189`: `name := d.Name()`).
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::Substring { tokens } => {
                if tokens.is_empty() {
                    return true;
                }
                let lower = name.to_lowercase();
                tokens.iter().all(|t| lower.contains(t.as_str()))
            }
            Self::Word { tokens } => {
                if tokens.is_empty() {
                    return true;
                }
                let lower = name.to_lowercase();
                tokens.iter().all(|t| contains_word(&lower, t))
            }
            Self::Regex(re) => re.matches(name),
        }
    }
}

/// Compile a query into a matcher.
///
/// # Errors
/// [`CoreError::BadRegex`] when `mode` is [`NameMode::Regex`] and the pattern is outside the supported
/// RE2 subset — Go's typed 400 (`err-search-bad-regex.json`), raised at request time, never mid-walk.
pub fn compile_name_match(query: &str, mode: NameMode) -> Result<NameMatcher, CoreError> {
    Ok(match mode {
        NameMode::Substring => NameMatcher::Substring {
            tokens: tokenize(query),
        },
        NameMode::Word => NameMatcher::Word {
            tokens: tokenize(query),
        },
        // Go only compiles a regex when the query is non-empty (`api/search.go:81`); an empty query
        // falls through to the substring path, which matches everything. Same here, so an emptied regex
        // box does not flash a parse error at the user.
        NameMode::Regex if query.is_empty() => NameMatcher::Substring { tokens: Vec::new() },
        NameMode::Regex => NameMatcher::Regex(Regex::new(query)?),
    })
}

/// The word-boundary token test [`NameMode::Word`] is built from.
fn contains_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h: Vec<char> = hay.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).enumerate().any(|(start, win)| {
        *win == n[..] && !word_before(&h, start) && !word_after(&h, start + n.len())
    })
}

/// The character before `start`, a word character only when there is one (`start > 0`).
fn word_before(chars: &[char], start: usize) -> bool {
    start > 0 && chars.get(start - 1).copied().is_some_and(is_word_char)
}

/// The character just after the span.
fn word_after(chars: &[char], end: usize) -> bool {
    chars.get(end).copied().is_some_and(is_word_char)
}

/// Go's `\w` set, ASCII-only without the Unicode flag (`regexp/syntax` `PerlWord`).
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

// ---------------------------------------------------------------------------------------------- regex

/// A compiled pattern: a Thompson NFA over [`Frag`] instructions plus its entry state.
#[derive(Clone, Debug)]
pub struct Regex {
    pattern: String,
    prog: Vec<Frag>,
    start: usize,
}

impl std::fmt::Display for Regex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.pattern)
    }
}

impl Regex {
    /// Compile with Go's forced case-insensitivity (`regexp.Compile("(?i)" + q)`, `api/search.go:82`).
    ///
    /// # Errors
    /// [`CoreError::BadRegex`] carrying Go's `error parsing regexp: …` text.
    pub fn new(pattern: &str) -> Result<Self, CoreError> {
        let bad = |reason: String| CoreError::BadRegex {
            pattern: pattern.to_string(),
            reason,
        };
        let mut parser = Parser::new(pattern);
        let ast = parser.parse().map_err(bad)?;
        let mut builder = Builder::default();
        let match_pc = builder.emit(Op::Match, 0);
        let start = builder.compile(&ast, match_pc).map_err(bad)?;
        Ok(Self {
            pattern: pattern.to_string(),
            prog: builder.prog,
            start,
        })
    }

    /// The pattern as typed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// Go's `MatchString`: unanchored, so the match may start at any position.
    #[must_use]
    pub fn matches(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        if self.prog.is_empty() {
            return false;
        }
        // `seen[pc] == pos + 1` means "this state is already queued at input position `pos`": the
        // epsilon-closure dedupe that keeps the simulation linear.
        let mut seen = vec![0u32; self.prog.len()];
        let mut cur: Vec<usize> = Vec::new();
        let mut nxt: Vec<usize> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        for pos in 0..=chars.len() {
            if add(
                &self.prog, self.start, pos, &chars, &mut seen, &mut stack, &mut cur,
            ) {
                return true;
            }
            for &pc in cur.iter() {
                let Some(ch) = chars.get(pos).copied() else {
                    break;
                };
                let frag = match self.prog.get(pc) {
                    Some(f) => f,
                    None => continue,
                };
                let pass = match &frag.op {
                    Op::Class(class) => class.test(ch),
                    Op::Any { dotall } => *dotall || ch != '\n',
                    _ => false,
                };
                if pass
                    && add(
                        &self.prog,
                        frag.succ,
                        pos + 1,
                        &chars,
                        &mut seen,
                        &mut stack,
                        &mut nxt,
                    )
                {
                    return true;
                }
            }
            std::mem::swap(&mut cur, &mut nxt);
            nxt.clear();
        }
        false
    }
}

/// One NFA instruction plus its fall-through successor (unused by `Match`/`Split`).
#[derive(Clone, Debug)]
struct Frag {
    op: Op,
    succ: usize,
}

#[derive(Clone, Debug)]
enum Op {
    Match,
    Class(Class),
    Any { dotall: bool },
    Assert(Assert),
    Split { a: usize, b: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assert {
    StartText,
    EndText,
    StartLine,
    EndLine,
    WordBoundary,
    NotWordBoundary,
}

impl Assert {
    fn holds(self, pos: usize, chars: &[char]) -> bool {
        let prev = if pos == 0 {
            None
        } else {
            chars.get(pos - 1).copied()
        };
        let next = chars.get(pos).copied();
        let word = |c: Option<char>| c.is_some_and(is_word_char);
        match self {
            Self::StartText => pos == 0,
            Self::EndText => pos == chars.len(),
            Self::StartLine => pos == 0 || prev == Some('\n'),
            Self::EndLine => pos == chars.len() || next == Some('\n'),
            Self::WordBoundary => word(prev) != word(next),
            Self::NotWordBoundary => word(prev) == word(next),
        }
    }
}

/// A character class: ranges over runes, optionally negated, optionally case-folded.
#[derive(Clone, Debug, PartialEq)]
pub struct Class {
    ranges: Vec<(char, char)>,
    negated: bool,
    fold: bool,
}

impl Class {
    fn of(ch: char, fold: bool) -> Self {
        Self {
            ranges: vec![(ch, ch)],
            negated: false,
            fold,
        }
    }

    fn negate(mut self) -> Self {
        self.negated = !self.negated;
        self
    }

    fn union(&mut self, other: &Self) {
        self.ranges.extend(other.ranges.iter().copied());
    }

    /// Go's class test: with `fold` a name character passes when any case variant of it is in the set
    /// (`(?i)[a-z]` matches `A`).
    fn test(&self, ch: char) -> bool {
        if self.fold {
            fold_variants(ch).any(|v| self.raw_test(v))
        } else {
            self.raw_test(ch)
        }
    }

    fn raw_test(&self, ch: char) -> bool {
        self.ranges.iter().any(|(lo, hi)| ch >= *lo && ch <= *hi) != self.negated
    }
}

/// The case variants of one character (see the module doc for the difference from Go).
fn fold_variants(ch: char) -> impl Iterator<Item = char> {
    let lower = ch.to_lowercase().next().unwrap_or(ch);
    let upper = ch.to_uppercase().next().unwrap_or(ch);
    [ch, lower, upper].into_iter()
}

/// Epsilon-closure insertion: follow `Jmp`/`Split`/`Assert` edges for free, queue the consuming states.
/// Returns `true` when `Match` is reachable, which is the whole answer.
fn add(
    prog: &[Frag],
    pc: usize,
    pos: usize,
    chars: &[char],
    seen: &mut [u32],
    stack: &mut Vec<usize>,
    out: &mut Vec<usize>,
) -> bool {
    let stamp = pos as u32 + 1;
    stack.clear();
    stack.push(pc);
    while let Some(p) = stack.pop() {
        let Some(slot) = seen.get_mut(p) else {
            continue;
        };
        if *slot == stamp {
            continue;
        }
        *slot = stamp;
        let frag = match prog.get(p) {
            Some(f) => f,
            None => continue,
        };
        match &frag.op {
            Op::Match => return true,
            Op::Split { a, b } => {
                stack.push(*b);
                stack.push(*a);
            }
            Op::Assert(cond) => {
                if cond.holds(pos, chars) {
                    stack.push(frag.succ);
                }
            }
            Op::Class { .. } | Op::Any { .. } => out.push(p),
        }
    }
    false
}

// ---------------------------------------------------------------------------------------- the compiler

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Class(Class),
    Any {
        dotall: bool,
    },
    Assert(Assert),
    Seq(Vec<Node>),
    Alt(Box<Node>, Box<Node>),
    Star(Box<Node>),
    Plus(Box<Node>),
    Quest(Box<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

#[derive(Default)]
struct Builder {
    prog: Vec<Frag>,
}

impl Builder {
    fn emit(&mut self, op: Op, succ: usize) -> usize {
        self.prog.push(Frag { op, succ });
        self.prog.len() - 1
    }

    fn compile(&mut self, node: &Node, next: usize) -> Result<usize, String> {
        if self.prog.len() > MAX_PROGRAM {
            return Err(goe("expression too large"));
        }
        match node {
            Node::Empty => Ok(next),
            Node::Class(c) => Ok(self.emit(Op::Class(c.clone()), next)),
            Node::Any { dotall } => Ok(self.emit(Op::Any { dotall: *dotall }, next)),
            Node::Assert(a) => Ok(self.emit(Op::Assert(*a), next)),
            Node::Seq(nodes) => {
                let mut pc = next;
                for node in nodes.iter().rev() {
                    pc = self.compile(node, pc)?;
                }
                Ok(pc)
            }
            Node::Alt(left, right) => {
                let split = self.emit(Op::Split { a: 0, b: 0 }, 0);
                let right_pc = self.compile(right, next)?;
                let left_pc = self.compile(left, next)?;
                self.prog[split].op = Op::Split {
                    a: left_pc,
                    b: right_pc,
                };
                Ok(split)
            }
            Node::Star(inner) => {
                let split = self.emit(Op::Split { a: 0, b: 0 }, 0);
                let body = self.compile(inner, split)?;
                self.prog[split].op = Op::Split { a: body, b: next };
                Ok(split)
            }
            Node::Plus(inner) => {
                // `x+` is `x x*`: same loop as `Star`, but the fragment is entered through the body, so
                // one copy is mandatory. The split still has to offer `next`, or the loop never ends.
                let split = self.emit(Op::Split { a: 0, b: 0 }, 0);
                let body = self.compile(inner, split)?;
                self.prog[split].op = Op::Split { a: body, b: next };
                Ok(body)
            }
            Node::Quest(inner) => {
                let split = self.emit(Op::Split { a: 0, b: next }, 0);
                let body = self.compile(inner, next)?;
                self.prog[split].op = Op::Split { a: body, b: next };
                Ok(split)
            }
            Node::Repeat { node, min, max } => self.compile_bounded(node, *min, *max, next),
        }
    }

    /// `min` copies, then (`max - min`) optional ones, or one `*` loop when `max` is `None`.
    fn compile_bounded(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        next: usize,
    ) -> Result<usize, String> {
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT || m < min) {
            return Err(goe("invalid repeat count"));
        }
        let tail_count = max.map_or(1, |m| m.saturating_sub(min));
        if u64::from(min) + u64::from(tail_count) > u64::from(MAX_REPEAT) * 2 {
            return Err(goe("expression too large"));
        }
        let mut pc = next;
        if max.is_none() {
            pc = self.compile(&Node::Star(Box::new(node.clone())), pc)?;
        }
        let optionals = max.map_or(0, |m| m.saturating_sub(min));
        for _ in 0..optionals {
            pc = self.compile(&Node::Quest(Box::new(node.clone())), pc)?;
        }
        for _ in 0..min {
            pc = self.compile(node, pc)?;
        }
        Ok(pc)
    }
}

// ------------------------------------------------------------------------------------------- the parser

#[derive(Clone, Copy, Default, Debug)]
struct Flags {
    fold: bool,
    dotall: bool,
    multiline: bool,
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    flags: Flags,
}

impl Parser {
    fn new(pattern: &str) -> Self {
        Self {
            chars: pattern.chars().collect(),
            pos: 0,
            // Go's live search compiles `(?i)` + pattern, so the flag starts set (`api/search.go:82`).
            flags: Flags {
                fold: true,
                dotall: false,
                multiline: false,
            },
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            return true;
        }
        false
    }

    /// The pattern text from `from` on, which is what Go's syntax errors quote.
    fn tail(&self, from: usize) -> String {
        self.chars[from..].iter().collect()
    }

    fn parse(&mut self) -> Result<Node, String> {
        let node = self.parse_alternation()?;
        if self.peek().is_some() {
            return Err(self.syntax_error(self.pos, "unexpected )"));
        }
        Ok(node)
    }

    fn parse_alternation(&mut self) -> Result<Node, String> {
        let mut node = self.parse_sequence()?;
        while self.eat('|') {
            let rhs = self.parse_sequence()?;
            node = Node::Alt(Box::new(node), Box::new(rhs));
        }
        Ok(node)
    }

    fn parse_sequence(&mut self) -> Result<Node, String> {
        let mut items: Vec<Node> = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.parse_quantified()?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Seq(items),
        })
    }

    fn parse_quantified(&mut self) -> Result<Node, String> {
        let mut atom = self.parse_atom()?;
        let mut quantified = false;
        loop {
            let at = self.pos;
            match self.peek() {
                Some('*') | Some('+') | Some('?') => {
                    if quantified {
                        return Err(self.syntax_error(
                            at,
                            &format!("invalid nested repetition operator: `{}`", self.tail(at)),
                        ));
                    }
                    let op = self.bump();
                    atom = match op {
                        Some('*') => Node::Star(Box::new(atom)),
                        Some('+') => Node::Plus(Box::new(atom)),
                        _ => Node::Quest(Box::new(atom)),
                    };
                    quantified = true;
                }
                Some('{') => match self.try_repeat_bounds() {
                    Some((min, max)) => {
                        atom = Node::Repeat {
                            node: Box::new(atom),
                            min,
                            max,
                        };
                        quantified = true;
                    }
                    // Go treats a `{` that is not a valid repetition as a literal brace.
                    None => {
                        self.pos = at;
                        break;
                    }
                },
                _ => break,
            }
        }
        Ok(atom)
    }

    /// `{n}`, `{n,}`, `{n,m}`; `None` when the braces are not a repetition (then they stay literal).
    fn try_repeat_bounds(&mut self) -> Option<(u32, Option<u32>)> {
        let save = self.pos;
        self.bump(); // '{'
        let min = self.parse_number()?;
        if self.eat('}') {
            return Some((min, Some(min)));
        }
        if self.eat(',') {
            if self.eat('}') {
                return Some((min, None));
            }
            let max = self.parse_number()?;
            if self.eat('}') {
                return Some((min, Some(max)));
            }
            self.pos = save;
            return None;
        }
        self.pos = save;
        None
    }

    fn parse_number(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        let digits: String = self.chars[start..self.pos].iter().collect();
        digits.parse::<u32>().ok()
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        let at = self.pos;
        match self.peek() {
            None => Ok(Node::Empty),
            Some('(') => self.parse_group(),
            Some('[') => self.parse_class(),
            Some('.') => {
                self.pos += 1;
                Ok(Node::Any {
                    dotall: self.flags.dotall,
                })
            }
            Some('^') => {
                self.pos += 1;
                Ok(Node::Assert(if self.flags.multiline {
                    Assert::StartLine
                } else {
                    Assert::StartText
                }))
            }
            Some('$') => {
                self.pos += 1;
                Ok(Node::Assert(if self.flags.multiline {
                    Assert::EndLine
                } else {
                    Assert::EndText
                }))
            }
            Some('\\') => self.parse_escape(),
            Some('|') => Err(self.syntax_error(at, "missing argument to repetition operator")),
            Some(')') => {
                Err(self.syntax_error(at, &format!("missing closing ): `{}`", self.tail(at))))
            }
            Some(c) => {
                self.pos += 1;
                Ok(Node::Class(Class::of(c, self.flags.fold)))
            }
        }
    }

    fn parse_group(&mut self) -> Result<Node, String> {
        let open = self.pos;
        self.pos += 1; // '('
        if !self.eat('?') {
            let saved = self.flags;
            let inner = self.parse_alternation()?;
            if !self.eat(')') {
                return Err(
                    self.syntax_error(open, &format!("missing closing ): `{}`", self.tail(open)))
                );
            }
            self.flags = saved;
            return Ok(inner);
        }
        let mut want = self.flags;
        let mut negating = false;
        let mut saw_flag = false;
        let mut unsupported = None;
        while let Some(c) = self.peek() {
            // `:` closes the flag list of a non-capturing group, `)` closes a flag setting; neither is a
            // flag, and Go accepts an empty list before `:` (`regexp/syntax` `"(?:"`).
            if c == ':' || c == ')' {
                break;
            }
            match c {
                '-' => {
                    negating = true;
                    self.pos += 1;
                }
                'i' => {
                    want.fold = !negating;
                    saw_flag = true;
                    self.pos += 1;
                }
                's' => {
                    want.dotall = !negating;
                    saw_flag = true;
                    self.pos += 1;
                }
                'm' => {
                    want.multiline = !negating;
                    saw_flag = true;
                    self.pos += 1;
                }
                // Accepted and ignored: with no captures there is no greediness to invert.
                'U' => {
                    saw_flag = true;
                    self.pos += 1;
                }
                _ => {
                    unsupported = Some(c);
                    break;
                }
            }
        }
        if unsupported.is_some() {
            return Err(self.syntax_error(open, &format!("bad flag syntax: `{}`", self.tail(open))));
        }
        if self.eat(')') {
            if !saw_flag {
                return Err(self.syntax_error(
                    open,
                    &format!("missing flag setting in (?…): `{}`", self.tail(open)),
                ));
            }
            // A flag setting, in scope to the end of the enclosing group (Go's rule, `regexp/syntax`).
            self.flags = want;
            return Ok(Node::Empty);
        }
        if self.eat(':') {
            let saved = self.flags;
            self.flags = want;
            let inner = self.parse_alternation()?;
            if !self.eat(')') {
                return Err(
                    self.syntax_error(open, &format!("missing closing ): `{}`", self.tail(open)))
                );
            }
            self.flags = saved;
            return Ok(inner);
        }
        Err(self.syntax_error(open, &format!("bad flag syntax: `{}`", self.tail(open))))
    }

    fn parse_escape(&mut self) -> Result<Node, String> {
        let at = self.pos;
        self.pos += 1; // '\\'
        let Some(c) = self.bump() else {
            return Err(goe("trailing backslash at end of expression"));
        };
        let cls = |ranges: &[(char, char)]| Class {
            ranges: ranges.to_vec(),
            negated: false,
            fold: false,
        };
        match c {
            'd' => Ok(Node::Class(cls(&[('0', '9')]))),
            'D' => Ok(Node::Class(cls(&[('0', '9')]).negate())),
            's' => Ok(Node::Class(cls(&[
                ('\t', '\r'),
                (' ', ' '),
                ('\u{85}', '\u{85}'),
                ('\u{A0}', '\u{A0}'),
            ]))),
            'S' => Ok(Node::Class(
                cls(&[
                    ('\t', '\r'),
                    (' ', ' '),
                    ('\u{85}', '\u{85}'),
                    ('\u{A0}', '\u{A0}'),
                ])
                .negate(),
            )),
            'w' => Ok(Node::Class(cls(&[
                ('0', '9'),
                ('A', 'Z'),
                ('_', '_'),
                ('a', 'z'),
            ]))),
            'W' => Ok(Node::Class(
                cls(&[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')]).negate(),
            )),
            'b' => Ok(Node::Assert(Assert::WordBoundary)),
            'B' => Ok(Node::Assert(Assert::NotWordBoundary)),
            'A' | 'z' | 'Z' | '1'..='9' => {
                Err(self.syntax_error(at, &format!("invalid escape sequence: `{}`", self.tail(at))))
            }
            'n' => Ok(self.literal('\n')),
            't' => Ok(self.literal('\t')),
            'r' => Ok(self.literal('\r')),
            'f' => Ok(self.literal('\u{c}')),
            'v' => Ok(self.literal('\u{b}')),
            'a' => Ok(self.literal('\u{7}')),
            'x' | 'u' | 'U' => self.parse_hex_escape(c, at),
            _ => {
                if c.is_ascii_punctuation() {
                    Ok(self.literal(c))
                } else {
                    Err(self
                        .syntax_error(at, &format!("invalid escape sequence: `{}`", self.tail(at))))
                }
            }
        }
    }

    fn literal(&self, c: char) -> Node {
        Node::Class(Class::of(c, self.flags.fold))
    }

    fn parse_hex_escape(&mut self, kind: char, at: usize) -> Result<Node, String> {
        let width = match kind {
            'x' => 2,
            'u' => 4,
            _ => 8,
        };
        let mut braced = false;
        if self.eat('{') {
            braced = true;
        }
        let digits = self.parse_hex_digits(if braced { 8 } else { width })?;
        if braced && !self.eat('}') {
            return Err(
                self.syntax_error(at, &format!("invalid escape sequence: `{}`", self.tail(at)))
            );
        }
        if !braced && digits.len() < width {
            return Err(
                self.syntax_error(at, &format!("invalid escape sequence: `{}`", self.tail(at)))
            );
        }
        let value = u32::from_str_radix(&digits, 16).unwrap_or(u32::MAX);
        let Some(ch) = char::from_u32(value) else {
            return Err(
                self.syntax_error(at, &format!("invalid escape sequence: `{}`", self.tail(at)))
            );
        };
        Ok(self.literal(ch))
    }

    fn parse_hex_digits(&mut self, max: usize) -> Result<String, String> {
        let start = self.pos;
        while self.pos - start < max && self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(goe("invalid escape sequence"));
        }
        Ok(self.chars[start..self.pos].iter().collect())
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        let open = self.pos;
        self.pos += 1; // '['
        let negated = self.eat('^');
        let mut cls = Class {
            ranges: Vec::new(),
            negated,
            fold: self.flags.fold,
        };
        let mut first = true;
        loop {
            match self.peek() {
                None => {
                    return Err(self
                        .syntax_error(open, &format!("missing closing ]: `{}`", self.tail(open))));
                }
                Some(']') if !first => {
                    self.pos += 1;
                    break;
                }
                _ => {}
            }
            if self.peek() == Some('[') && self.peek_at(1) == Some(':') {
                self.parse_posix_class(&mut cls)?;
                first = false;
                continue;
            }
            let lo = self.parse_class_item(open)?;
            first = false;
            let lo_ch = match lo {
                ClassItem::Set(set) => {
                    cls.union(&set);
                    continue;
                }
                ClassItem::Literal(c) => c,
            };
            if self.peek() == Some('-') && !matches!(self.peek_at(1), None | Some(']')) {
                self.pos += 1;
                match self.parse_class_item(open)? {
                    ClassItem::Literal(hi) => cls.ranges.push((lo_ch, hi)),
                    ClassItem::Set(_) => {
                        return Err(self.syntax_error(
                            open,
                            &format!("invalid character class range: `{}`", self.tail(open)),
                        ));
                    }
                }
                continue;
            }
            cls.ranges.push((lo_ch, lo_ch));
        }
        Ok(Node::Class(cls))
    }

    /// One class element: a literal (which may start a range) or a set (`\d`, `\W`, …).
    fn parse_class_item(&mut self, open: usize) -> Result<ClassItem, String> {
        if self.peek() == Some('\\') {
            let at = self.pos;
            self.pos += 1;
            let Some(c) = self.bump() else {
                return Err(goe("trailing backslash at end of expression"));
            };
            if let Some(cls) = perl_class(c) {
                return Ok(ClassItem::Set(cls));
            }
            let ch = match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'f' => '\u{c}',
                'v' => '\u{b}',
                'a' => '\u{7}',
                'x' | 'u' | 'U' => {
                    let node = self.parse_hex_escape(c, at)?;
                    return match class_literal(&node) {
                        Some(c) => Ok(ClassItem::Literal(c)),
                        None => Err(self.syntax_error(
                            at,
                            &format!("invalid escape sequence: `{}`", self.tail(at)),
                        )),
                    };
                }
                _ => c,
            };
            return Ok(ClassItem::Literal(ch));
        }
        match self.bump() {
            Some(c) => Ok(ClassItem::Literal(c)),
            None => {
                Err(self.syntax_error(open, &format!("missing closing ]: `{}`", self.tail(open))))
            }
        }
    }

    fn parse_posix_class(&mut self, cls: &mut Class) -> Result<(), String> {
        let start = self.pos;
        self.pos += 2; // '[' ':'
        let name: String = self
            .chars
            .get(self.pos..)
            .unwrap_or(&[])
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        self.pos += name.chars().count();
        if !self.eat(':') || !self.eat(']') {
            // Not a POSIX name: rewind and let the ordinary item parser take the `[` literally.
            self.pos = start + 1;
            cls.ranges.push(('[', '['));
            return Ok(());
        }
        let Some(ranges) = posix_ranges(&name) else {
            return Err(self.syntax_error(
                start,
                &format!("invalid character class range: `{}`", self.tail(start)),
            ));
        };
        cls.ranges.extend_from_slice(ranges);
        Ok(())
    }

    /// Go's `syntax.Error` shape: `error parsing regexp: <detail>`, where a detail that quotes pattern
    /// text has already been built by the caller (`regexp/syntax::ErrCode.Error`).
    fn syntax_error(&self, _at: usize, detail: &str) -> String {
        goe(detail)
    }
}

/// A class element as parsed.
enum ClassItem {
    Literal(char),
    Set(Class),
}

/// The single character a one-rune [`Node::Class`] stands for.
fn class_literal(node: &Node) -> Option<char> {
    match node {
        Node::Class(c) if !c.negated => {
            let (lo, hi) = c.ranges.first().copied().unwrap_or(('\0', '\0'));
            (lo == hi && c.ranges.len() == 1).then_some(lo)
        }
        _ => None,
    }
}

/// The Perl classes Go's `\d`/`\s`/`\w` compile to (ASCII ranges; `regexp/syntax` `PerlDigits`…).
fn perl_class(c: char) -> Option<Class> {
    let mut ranges: Vec<(char, char)> = match c {
        'd' => vec![('0', '9')],
        'D' => vec![('0', '9')],
        's' => vec![
            ('\t', '\r'),
            (' ', ' '),
            ('\u{85}', '\u{85}'),
            ('\u{A0}', '\u{A0}'),
        ],
        'S' => vec![
            ('\t', '\r'),
            (' ', ' '),
            ('\u{85}', '\u{85}'),
            ('\u{A0}', '\u{A0}'),
        ],
        'w' => vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
        'W' => vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
        _ => return None,
    };
    let negated = matches!(c, 'D' | 'S' | 'W');
    if negated {
        ranges = Vec::new();
    }
    Some(Class {
        ranges,
        negated,
        fold: false,
    })
}

/// The POSIX class names Go accepts inside `[...]` (`regexp/syntax` `group`).
fn posix_ranges(name: &str) -> Option<&'static [(char, char)]> {
    const ALNUM: &[(char, char)] = &[('0', '9'), ('A', 'Z'), ('a', 'z')];
    const ALPHA: &[(char, char)] = &[('A', 'Z'), ('a', 'z')];
    const DIGIT: &[(char, char)] = &[('0', '9')];
    const SPACE: &[(char, char)] = &[('\t', '\r'), (' ', ' ')];
    const UPPER: &[(char, char)] = &[('A', 'Z')];
    const LOWER: &[(char, char)] = &[('a', 'z')];
    const PUNCT: &[(char, char)] = &[('!', '/'), (':', '@'), ('[', '`'), ('{', '~')];
    const WORD: &[(char, char)] = &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')];
    const XDIGIT: &[(char, char)] = &[('0', '9'), ('A', 'F'), ('a', 'f')];
    const CNTRL: &[(char, char)] = &[('\0', '\u{1f}')];
    const GRAPH: &[(char, char)] = &[('!', '\u{7f}')];
    const PRINT: &[(char, char)] = &[(' ', '\u{7e}')];
    const BLANK: &[(char, char)] = &[('\t', '\t'), (' ', ' ')];
    match name {
        "alnum" => Some(ALNUM),
        "alpha" => Some(ALPHA),
        "digit" => Some(DIGIT),
        "space" => Some(SPACE),
        "upper" => Some(UPPER),
        "lower" => Some(LOWER),
        "punct" => Some(PUNCT),
        "word" => Some(WORD),
        "xdigit" => Some(XDIGIT),
        "cntrl" => Some(CNTRL),
        "graph" => Some(GRAPH),
        "print" => Some(PRINT),
        "blank" => Some(BLANK),
        _ => None,
    }
}

/// Go's `(*syntax.Error).Error()` prefix (`regexp/syntax`: `"error parsing regexp: " + Code + [text]`).
fn goe(detail: &str) -> String {
    format!("error parsing regexp: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(pattern: &str) -> Regex {
        Regex::new(pattern).unwrap_or_else(|e| panic!("{pattern}: {e}"))
    }

    fn sub(q: &str) -> NameMatcher {
        compile_name_match(q, NameMode::Substring).unwrap_or_else(|e| panic!("{q}: {e}"))
    }

    #[test]
    fn substring_is_go_case_insensitive_all_tokens() {
        // api/search.go:96-105 — every token must be contained, on the lowercased name.
        let m = sub("FOO bar");
        assert!(m.matches("Bar-Foo-1.txt"));
        assert!(!m.matches("bar-only.txt"));
        assert!(!m.matches("foo-only.txt"));
    }

    #[test]
    fn empty_query_matches_everything() {
        assert!(sub("").matches("anything"));
        assert!(sub("   ").matches("anything"));
        // Go's regex branch is skipped for an empty query (`api/search.go:81`), so this is not a parse.
        assert!(compile_name_match("", NameMode::Regex).is_ok());
        assert!(compile_name_match("", NameMode::Regex).is_ok_and(|m| m.matches("x")));
    }

    #[test]
    fn tokens_split_like_go_fields() {
        assert_eq!(tokenize(" a\tb \n c  "), ["a", "b", "c"]);
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn regex_is_always_case_insensitive_like_go() {
        // `(?i)` is hardcoded at api/search.go:82.
        assert!(re("readme").matches("README.MD"));
        assert!(re("^a").matches("a10.txt"));
        assert!(!re("^a").matches("ba10.txt"));
    }

    #[test]
    fn regex_anchors_and_dot() {
        assert!(re("^a9\\.txt$").matches("a9.txt"));
        assert!(re("^a9.txt$").matches("a9Xtxt"));
        assert!(!re("^a9\\.txt$").matches("a9Xtxt"));
        assert!(!re("a.txt").matches("a\ntxt"));
        assert!(re("(?s)a.txt").matches("a\ntxt"));
    }

    #[test]
    fn regex_classes_ranges_and_negation() {
        assert!(re("[a-c]+").matches("bbb"));
        assert!(!re("[a-c]+").matches("ddd"));
        assert!(re("[^0-9]+").matches("abc"));
        assert!(re("[^0-9]+").matches("a1"));
        assert!(!re("^[^0-9]+$").matches("a1"));
        assert!(re("[[:digit:]]+").matches("x42"));
        assert!(re(r"\d-\d").matches("f0001-2"));
    }

    #[test]
    fn regex_folded_classes() {
        assert!(re("[a-z]+").matches("ABC"));
        assert!(re(r"\w+").matches("K"));
        assert!(!re(r"\d+").matches("K"));
    }

    #[test]
    fn regex_alternation_and_groups() {
        assert!(re("(txt|md)$").matches("notes.md"));
        assert!(re("(?:txt|md)").matches("a.txt.bak"));
        assert!(re("^(a|b)+$").matches("ababa"));
    }

    #[test]
    fn regex_repetitions() {
        assert!(re("a{3}").matches("xaaay"));
        assert!(!re("a{4}").matches("aaay"));
        assert!(re("a{2,}").matches("aaa"));
        assert!(re("a?b").matches("b"));
        assert!(re("a+b").matches("aaab"));
        assert!(re("(ab)*").matches("ababab"));
        assert!(re("(ab)+").matches("ab"));
        assert!(!re("(ab)+").matches("x"));
    }

    #[test]
    fn regex_word_boundaries_and_escapes() {
        assert!(re(r"\bfoo\b").matches("a-foo-1"));
        assert!(!re(r"\bfoo\b").matches("foobar"));
        assert!(re(r"a\.b").matches("a.b"));
        assert!(!re(r"a\.b").matches("axb"));
        assert!(re("\\u0061").matches("a"));
        assert!(re("\\x41").matches("A"));
    }

    #[test]
    fn regex_line_anchors_with_multiline() {
        assert!(!re("^b").matches("a\nb"));
        assert!(re("(?m)^b").matches("a\nb"));
        assert!(!re("a$").matches("a\nb"));
        assert!(re("(?m)a$").matches("a\nb"));
    }

    #[test]
    fn nested_repetition_is_refused_like_go() {
        let e = Regex::new("a**").expect_err("a** is not a valid pattern");
        assert!(
            e.to_string().contains("invalid nested repetition operator"),
            "{e}"
        );
    }

    #[test]
    fn unclosed_class_message_is_go_byte_identical() {
        // err-search-bad-regex.json: `Invalid regex: error parsing regexp: missing closing ]: `[``.
        let e = Regex::new("[").expect_err("`[` must not parse");
        assert_eq!(
            e.to_string(),
            "Invalid regex: error parsing regexp: missing closing ]: `[`"
        );
    }

    #[test]
    fn other_parse_errors_are_typed_and_prefixed() {
        for pattern in ["(", "a)", r"a\", "a{2,1}", "(?x)y"] {
            let e = Regex::new(pattern).expect_err("must not parse");
            assert!(
                e.to_string()
                    .starts_with("Invalid regex: error parsing regexp: "),
                "{pattern}: {e}"
            );
            assert_eq!(e.kind(), "bad_regex");
        }
    }

    #[test]
    fn oversized_repeat_is_refused() {
        let e = Regex::new("a{1001}").expect_err("over maxRepeat");
        assert!(e.to_string().contains("invalid repeat count"), "{e}");
    }

    #[test]
    fn brace_that_is_not_a_repetition_is_literal() {
        // Go keeps `{` literal when it cannot read a count out of it.
        assert!(re("a{b").matches("a{b"));
    }

    #[test]
    fn pathological_patterns_stay_linear() {
        // The RE2 property: a backtracker would explode on this; the NFA simulation is |text| x |prog|.
        let m = re("a*a*a*a*a*b");
        let haystack = "a".repeat(2_000);
        let started = std::time::Instant::now();
        assert!(!m.matches(&haystack));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn word_mode_requires_boundaries() {
        let m = compile_name_match("foo", NameMode::Word).unwrap_or_else(|e| panic!("{e}"));
        assert!(m.matches("foo.txt"));
        assert!(m.matches("my-foo-1"));
        assert!(!m.matches("foobar"));
        assert_eq!(m.mode(), NameMode::Word);
    }

    #[test]
    fn name_modes_serialize_as_snake_case() {
        let json =
            serde_json::to_string(&[NameMode::Substring, NameMode::Word, NameMode::Regex]).unwrap();
        assert_eq!(json, r#"["substring","word","regex"]"#);
    }
}
