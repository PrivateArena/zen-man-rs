//! Custom actions (plan §5.3's `services/actions.rs` row, §7 step 14a; Go `api/actions.go`).
//!
//! Three responsibilities, and this module is the only place any of them lives:
//!
//! 1. **applicability** — the role + glob predicate that decides which actions a menu offers. Step 10b
//!    left [`crate`]`_ui`'s `menus::custom_specs`/`role_matches` as the placeholders; they move here
//!    (14b deletes the UI copies and calls [`for_context`]).
//! 2. **template expansion** — `{dir} {target_dir} {file} {files} {name} {names} {parent}`. This is the
//!    repository's one *command-injection surface*: a file name is user data that reaches a shell.
//! 3. **execution** — through [`Env::exec_program`], the crate's single spawn site (plan F10).
//!
//! # The injection story, in full
//!
//! Go expanded the template with seven `strings.ReplaceAll` calls over the *raw* strings and then handed
//! the line to `sh -c` (`api/actions.go:215-243`). Two consequences, both real:
//!
//! * a file called `a; rm -rf /.txt` turned `echo {name}` into `echo a; rm -rf /.txt` — the `;` was
//!   shell syntax. Go's only defence was the `"%s"`-wrapping it applied to `{files}`/`{names}`, and even
//!   that escaped `"` with a backslash, which `sh` does not treat that way inside double quotes.
//! * the replacements ran **in sequence**, so a name containing a later token was expanded again: a file
//!   literally called `{dir}` leaked the pane directory, and a name of `{files}` re-entered the line.
//!
//! The port's answers, both tested in `crates/zenman-core/tests/contract_workspaces.rs`:
//!
//! * **single-pass scan** — one left-to-right walk over the template; a substituted value is never
//!   re-scanned (the fix for the second bug);
//! * **explicit quoting per platform** — [`quote`] wraps every substituted value in POSIX single quotes
//!   (`'` → `'\''`), which is the one shell metacharacter-free string form `sh` has, or in
//!   `CommandLineToArgvW`-style double quotes on Windows. The template author still writes the command
//!   line, but a token they place is *one argument*, whatever the file is called.
//! * the line runs as `sh -c <line>` on Unix and as the **argv vector** `cmd /C <line>` on Windows
//!   (plan F10: never string-concatenated into `cmd`'s own command line);
//! * `show_output == false` runs [`ExecRequest::detached`] — Go's `cmd.Start()` — which is also the
//!   step-9b `xclip` lesson: a daemonising child that inherits the output pipes makes a capturing wait
//!   hang forever.
//!
//! An absent value substitutes the empty string, *unquoted* (Go parity: `foo {file}` with nothing
//! selected is `foo `, not `foo ''`).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::CoreError;
use crate::jobs::block_on;
use crate::path::AbsPath;
use crate::platform::{Env, ExecOutput, ExecRequest, Os};
use crate::store::config::CustomAction;

/// The seven tokens Go supported (`api/actions.go:215-221`) — the same set the plan names, and the
/// same set the action editor's preview has to describe.
pub const TOKENS: [&str; 7] = [
    "{dir}",
    "{target_dir}",
    "{file}",
    "{files}",
    "{name}",
    "{names}",
    "{parent}",
];

/// What the action was asked about: the pane's directory and the selection (Go's
/// `{dir}`/`{paths}` request body, `api/actions.go:141-151`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionContext {
    /// The pane's current directory — Go's `req.Dir`, and the child's working directory.
    pub dir: Option<AbsPath>,
    /// The selected paths, in listing order. The **first** one is the "clicked" row: Go derived
    /// `{file}`/`{name}`/`{parent}` from `paths[0]` and matched the pattern against that name alone.
    pub targets: Vec<AbsPath>,
    /// A click on the empty pane area rather than on a row (Go's `isItem == false`).
    pub background: bool,
    /// Whether the clicked row is a directory. The UI knows from its listing; Go paid a `stat` for it
    /// (`api/actions.go:203`), which is both a syscall and wrong for a symlinked directory.
    pub target_is_dir: bool,
}

impl ActionContext {
    /// A background click in `dir`.
    #[must_use]
    pub fn background(dir: Option<AbsPath>) -> Self {
        Self {
            dir,
            targets: Vec::new(),
            background: true,
            target_is_dir: false,
        }
    }

    /// A click on one row (or a selection whose *first* row decides the filters).
    #[must_use]
    pub fn rows(dir: Option<AbsPath>, targets: Vec<AbsPath>, target_is_dir: bool) -> Self {
        Self {
            dir,
            targets,
            background: false,
            target_is_dir,
        }
    }

    /// The clicked path, if any.
    #[must_use]
    pub fn target(&self) -> Option<&AbsPath> {
        self.targets.first()
    }

    /// The text of a path, lossy-free because [`AbsPath`] is already a `PathBuf` under the hood.
    fn text(path: Option<&AbsPath>) -> Option<String> {
        path.map(|p| p.to_string_lossy())
    }

    /// `{file}` — the clicked path.
    #[must_use]
    pub fn file(&self) -> Option<String> {
        Self::text(self.target())
    }

    /// `{files}` — every selected path.
    #[must_use]
    pub fn files(&self) -> Vec<String> {
        self.targets.iter().map(|p| p.to_string_lossy()).collect()
    }

    /// `{name}` — the clicked row's file name.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        self.target()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// `{names}` — every selected row's file name.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.targets
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect()
    }

    /// `{parent}` — the clicked path's parent (`filepath.Dir(firstPath)`, Go's `parentDir`).
    #[must_use]
    pub fn parent(&self) -> Option<String> {
        self.target().and_then(|p| p.parent()).and_then(|p| {
            let text = p.to_string_lossy();
            (!text.is_empty()).then_some(text)
        })
    }

    /// `{target_dir}` — the folder itself when the clicked row is a directory, its parent when it is
    /// a file, and the pane directory on a background click (`api/actions.go:198-209`).
    #[must_use]
    pub fn target_dir(&self) -> Option<String> {
        if self.background || self.target().is_none() {
            return Self::text(self.dir.as_ref());
        }
        if self.target_is_dir {
            return Self::text(self.target());
        }
        self.parent()
    }
}

/// The three role bits Go's checkboxes write (`custom-actions.js:107-110,125-128`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A file row.
    Files,
    /// A directory row.
    Dirs,
    /// The empty pane area.
    Background,
}

/// The role set of an action, with Go's `normalizeRole`: an empty role or the legacy `"both"` means
/// `files|dirs`. Unrecognised words are dropped rather than being allowed to imply everything.
#[must_use]
pub fn roles(action: &CustomAction) -> Vec<Role> {
    let role = action.role.trim();
    if role.is_empty() || role.eq_ignore_ascii_case("both") {
        return vec![Role::Files, Role::Dirs];
    }
    role.split('|')
        .map(str::trim)
        .filter_map(|part| match part.to_ascii_lowercase().as_str() {
            "files" => Some(Role::Files),
            "dirs" => Some(Role::Dirs),
            "background" => Some(Role::Background),
            _ => None,
        })
        .collect()
}

/// Go's role filter alone (`custom-actions.js:340-353`), i.e. [`matches`] without the glob half.
#[must_use]
pub fn role_matches(action: &CustomAction, ctx: &ActionContext) -> bool {
    let want = if ctx.background {
        Role::Background
    } else if ctx.target_is_dir {
        Role::Dirs
    } else {
        Role::Files
    };
    roles(action).contains(&want)
}

/// The full applicability predicate (plan §5.3's `matches`).
///
/// A background click is role-only — there is no file name to test, and Go said so
/// (`custom-actions.js:341-344`). A row click is role **and** pattern, tested against the clicked
/// name only, even for a multi-selection (Go's `targetPath` was the right-clicked row).
#[must_use]
pub fn matches(action: &CustomAction, ctx: &ActionContext) -> bool {
    if !role_matches(action, ctx) {
        return false;
    }
    if ctx.background {
        return true;
    }
    match ctx.name() {
        // A row with no name (a filesystem root) has nothing to match, and Go's empty `fileName`
        // matched only an empty pattern list — which [`patterns_match`] reads as "everything".
        None => patterns_match("", &action.patterns),
        Some(name) => patterns_match(&name, &action.patterns),
    }
}

/// The actions a context menu should offer, in stored order (Go's `getActionsForContext`).
#[must_use]
pub fn for_context(actions: &[CustomAction], ctx: &ActionContext) -> Vec<CustomAction> {
    actions
        .iter()
        .filter(|action| matches(action, ctx))
        .cloned()
        .collect()
}

/// Go's `matchPattern` (`custom-actions.js:324-335`): an empty list matches everything; otherwise the
/// name matches if **any** comma-separated, trimmed, lowercased pattern does.
#[must_use]
pub fn patterns_match(name: &str, patterns: &str) -> bool {
    if patterns.trim().is_empty() {
        return true;
    }
    let name = name.to_lowercase();
    patterns
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|pattern| glob_match(&pattern.to_lowercase(), &name))
}

/// The glob Go's regex built: `*` is any run of characters, **every other character is literal** —
/// including `?`, which Go escaped (`[-\/\\^$+?.()|[\]{}]`), so `a?b` matches the three-character name
/// `a?b` and nothing else. `**` therefore collapses into `*`, which is why `**.js` and `*.js` behave
/// identically in Go and here.
///
/// Written as a scanner rather than through `regex` deliberately: the plan's note ("glob→regex, not
/// `*`→`.*`") is about *escaping every metacharacter except the star*, and a scanner cannot get that
/// wrong, cannot be poisoned by a pathological pattern, and costs no compile per menu open.
#[must_use]
pub fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let head = parts.first().copied().unwrap_or(pattern);
    let (tail, middle) = match parts.split_last() {
        Some((tail, middle)) => (*tail, middle),
        None => (pattern, &[][..]),
    };
    if !name.starts_with(head) {
        return false;
    }
    // Leftmost match per middle segment is optimal: every later segment can only start further right,
    // so an earlier choice never costs a match a later one would have found.
    let mut rest = &name[head.len()..];
    for part in middle {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    tail.is_empty() || rest.ends_with(tail)
}

/// The value a token expands to, before quoting. `None` means "substitute nothing at all" (Go's empty
/// string, unquoted).
fn token_value(token: &str, ctx: &ActionContext, os: Os) -> Option<String> {
    let plain = |value: String| (!value.is_empty()).then_some(value);
    match token {
        "{dir}" => ctx.dir.as_ref().map(|d| quote(os, &d.to_string_lossy())),
        "{target_dir}" => ctx.target_dir().map(|d| quote(os, &d)),
        "{file}" => ctx.file().map(|f| quote(os, &f)),
        "{name}" => ctx.name().map(|n| quote(os, &n)),
        "{parent}" => ctx.parent().map(|p| quote(os, &p)).and_then(plain),
        "{files}" => {
            let all = ctx.files();
            (!all.is_empty()).then(|| {
                all.iter()
                    .map(|p| quote(os, p))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        }
        "{names}" => {
            let all = ctx.names();
            (!all.is_empty()).then(|| {
                all.iter()
                    .map(|n| quote(os, n))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        }
        _ => None,
    }
}

/// Expand a command template for `ctx`, quoting every substituted value for `sh` (the current
/// platform's shell, in practice — see [`substitute_for`] for the explicit-OS form).
#[must_use]
pub fn substitute(template: &str, ctx: &ActionContext) -> String {
    substitute_for(Os::current(), template, ctx)
}

/// Expand for a named platform's quoting rules. Public so the Windows arm is testable from Linux.
#[must_use]
pub fn substitute_for(os: Os, template: &str, ctx: &ActionContext) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(brace) = rest.find('{') {
        // Copy everything up to the brace, then try to read a token at it.
        let (before, after_brace) = rest.split_at(brace);
        out.push_str(before);
        match TOKENS.iter().find(|token| after_brace.starts_with(**token)) {
            Some(token) => {
                if let Some(value) = token_value(token, ctx, os) {
                    out.push_str(&value);
                }
                rest = &after_brace[token.len()..];
            }
            None => {
                out.push('{');
                rest = after_brace;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Quote one value so the shell sees it as a single argument.
///
/// POSIX: wrap in `'…'`, escaping an embedded `'` as `'\''` — the classic idiom, and the only one that
/// makes `;`, `` ` ``, `$()`, `|` and `&` inert without a table of per-character exceptions.
/// Windows: wrap in `"…"`, doubling an embedded `"` (`a"b` → `"a""b"`), which is what
/// `CommandLineToArgvW` parses back to one argument. Note that the *line* still reaches `cmd` as one
/// argv element of `cmd /C` ([`build_request_for`]), never as a concatenation.
#[must_use]
pub fn quote(os: Os, value: &str) -> String {
    if os == Os::Windows {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for ch in value.chars() {
            if ch == '"' {
                out.push_str("\"\"");
            } else {
                out.push(ch);
            }
        }
        out.push('"');
        return out;
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// What an executed action produced — Go's `{output}` body plus the two facts Go did not report (the
/// exit status was folded into an HTTP 200 either way, and the output was uncapped).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutcome {
    /// The exit code, `None` when the child died on a signal.
    pub code: Option<i32>,
    /// Combined stdout+stderr (Go's `CombinedOutput`), cut at the cap.
    pub output: String,
    /// `true` when the cap bit — surfaced, never swallowed (plan F10).
    pub truncated: bool,
}

impl ExecOutcome {
    /// A zero exit status.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Output with the trailing newline Go's HTTP body kept, removed (the toast wants one line).
    #[must_use]
    pub fn trimmed(&self) -> &str {
        self.output.trim_end_matches(['\n', '\r'])
    }
}

impl From<ExecOutput> for ExecOutcome {
    fn from(out: ExecOutput) -> Self {
        let mut output = String::with_capacity(out.stdout.len() + out.stderr.len());
        output.push_str(&out.stdout);
        output.push_str(&out.stderr);
        Self {
            code: out.code,
            output,
            truncated: out.truncated,
        }
    }
}

/// The request that running `action` for `ctx` produces, on this platform, capped at `output_cap`
/// bytes (`Limits::exec_output_cap` in production).
#[must_use]
pub fn build_request(action: &CustomAction, ctx: &ActionContext, output_cap: usize) -> ExecRequest {
    build_request_for(Os::current(), action, ctx, output_cap)
}

/// The explicit-OS form: `sh -c <line>` everywhere (plan F10: `sh -c` *only* on Unix), `cmd /C` as an
/// argv vector on Windows. `cwd` is `ctx.dir`, Go's `cmd.Dir` (`api/actions.go:241`); a pane with no
/// directory spawns in the inherited one.
#[must_use]
pub fn build_request_for(
    os: Os,
    action: &CustomAction,
    ctx: &ActionContext,
    output_cap: usize,
) -> ExecRequest {
    let line = substitute_for(os, &action.command, ctx);
    let mut req = if os == Os::Windows {
        // Plan F10: an argv vector, never a concatenation into `cmd`'s own command line. `cmd /C`
        // still parses the line it is handed, which is why the *values* are quoted above.
        let mut req = ExecRequest::argv("cmd", ["/C", line.as_str()]);
        req.cwd = ctx.dir.clone();
        req
    } else {
        ExecRequest::shell(line, ctx.dir.clone())
    };
    req.output_cap = output_cap;
    // Go's `show_output == false` arm is `cmd.Start()` — and the port's equivalent is the *detached*
    // spawn, not a captured one, because a daemonising child that outlives its parent keeps the output
    // pipes open (step 9b's `xclip` hang).
    req.detached = !action.show_output;
    req
}

/// Run one action. `env` is the session's platform seam — the tests hand a recorder, so no process is
/// ever spawned by a contract test that does not mean to.
///
/// # Errors
/// The [`Env::exec_program`] failure (a missing `sh`, an `exec` refusal), or [`CoreError::Unsupported`]
/// from a platform with no spawn support.
pub fn exec(
    env: &dyn Env,
    action: &CustomAction,
    ctx: &ActionContext,
    output_cap: usize,
) -> Result<ExecOutcome, CoreError> {
    let req = build_request(action, ctx, output_cap);
    let out = block_on(env.exec_program(req))??;
    Ok(out.into())
}

/// The 16-hex id Go generated with `crypto/rand` (`api/actions.go:18-22`).
///
/// The port has no RNG in its dependency graph, and adding one for a *menu key* is not worth the
/// supply-chain surface; ids are never a secret and never an authority (a wrong id is a `NotFound`).
/// This is a splitmix64 over the clock, the pid and a monotonic counter — distinct across threads and
/// processes without coordination, which is the only property the caller needs.
#[must_use]
pub fn generate_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(0));
    let mut x = nanos
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (u64::from(std::process::id()) << 32);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    let mut out = String::with_capacity(16);
    let _ = write!(out, "{x:016x}");
    out
}

// -------------------------------------------------------------------------------------- CRUD (Go's four verbs)

/// `POST /api/actions` `{"action":"create"}`: assign an id and append (`api/actions.go:65-68`).
#[must_use]
pub fn insert(actions: &[CustomAction], mut action: CustomAction) -> Vec<CustomAction> {
    action.id = generate_id();
    let mut out = actions.to_vec();
    out.push(action);
    out
}

/// `{"action":"update"}`: replace by id.
///
/// # Errors
/// [`CoreError::NotFound`] — Go's `{"error":"Action not found"}` 404 (`api/actions.go:78-84`).
pub fn update(
    actions: &[CustomAction],
    action: &CustomAction,
) -> Result<Vec<CustomAction>, CoreError> {
    let mut out = actions.to_vec();
    let slot = out.iter_mut().find(|a| a.id == action.id);
    match slot {
        Some(slot) => {
            *slot = action.clone();
            Ok(out)
        }
        None => Err(unknown(action.id.as_str())),
    }
}

/// `{"action":"delete"}`: remove by id. `false` when no action had that id (Go answered 404, which is
/// [`CoreError::NotFound`] here — the same shape as [`update`]).
///
/// # Errors
/// [`CoreError::NotFound`] for an unknown id.
pub fn remove(actions: &[CustomAction], id: &str) -> Result<Vec<CustomAction>, CoreError> {
    let out: Vec<CustomAction> = actions.iter().filter(|a| a.id != id).cloned().collect();
    if out.len() == actions.len() {
        return Err(unknown(id));
    }
    Ok(out)
}

/// `{"action":"reorder"}`: the given ids first, in order, then anything the list forgot — which is
/// Go's behaviour exactly (`api/actions.go:96-119`) and the reason a partial drag never loses a row.
#[must_use]
pub fn reorder(actions: &[CustomAction], ids: &[String]) -> Vec<CustomAction> {
    let mut out: Vec<CustomAction> = Vec::with_capacity(actions.len());
    for id in ids {
        if let Some(action) = actions.iter().find(|a| &a.id == id)
            && !out.iter().any(|a: &CustomAction| &a.id == id)
        {
            out.push(action.clone());
        }
    }
    for action in actions {
        if !out.iter().any(|a: &CustomAction| a.id == action.id) {
            out.push(action.clone());
        }
    }
    out
}

fn unknown(id: &str) -> CoreError {
    CoreError::NotFound {
        path: format!("custom action {id}"),
        source_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(pattern: &str, role: &str) -> CustomAction {
        CustomAction {
            id: "x".to_string(),
            name: "n".to_string(),
            icon: String::new(),
            command: "echo {name}".to_string(),
            role: role.to_string(),
            patterns: pattern.to_string(),
            show_output: true,
        }
    }

    #[test]
    fn the_glob_is_star_only_because_go_escaped_the_question_mark() {
        assert!(glob_match("*.txt", "a.txt"));
        assert!(!glob_match("*.txt", "a.txtx"));
        assert!(glob_match("**.js", "x.js"), "`**` collapses into `*`");
        assert!(
            glob_match("a?b", "a?b"),
            "`?` is literal in Go's regex, so here too"
        );
        assert!(!glob_match("a?b", "axb"));
        assert!(glob_match("", ""), "an empty pattern matches an empty name");
        assert!(!glob_match("", "a"));
        assert!(patterns_match("A.TXT", "*.txt"), "both sides lowercased");
        assert!(
            patterns_match("whatever", "  "),
            "an empty list matches everything"
        );
        assert!(
            patterns_match("b.JS", " *.css, *.js "),
            "comma list, trimmed"
        );
    }

    #[test]
    fn roles_normalise_and_filter() {
        assert_eq!(roles(&action("", "")), vec![Role::Files, Role::Dirs]);
        assert_eq!(roles(&action("", "both")), vec![Role::Files, Role::Dirs]);
        assert_eq!(
            roles(&action("", "files|background")),
            vec![Role::Files, Role::Background]
        );
        assert_eq!(roles(&action("", "nonsense")), vec![]);
        let dir_ctx = ActionContext {
            dir: None,
            targets: vec![],
            background: false,
            target_is_dir: true,
        };
        assert!(!role_matches(&action("", "files"), &dir_ctx));
        assert!(role_matches(&action("", "dirs"), &dir_ctx));
        assert!(role_matches(
            &action("", "background"),
            &ActionContext::background(None)
        ));
    }

    #[test]
    fn quoting_never_leaks_a_metacharacter() {
        assert_eq!(quote(Os::Linux, "a b"), "'a b'");
        assert_eq!(quote(Os::Linux, "it's"), r"'it'\''s'");
        assert_eq!(quote(Os::Windows, "a\"b"), "\"a\"\"b\"");
    }
}
