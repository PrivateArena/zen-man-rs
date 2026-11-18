//! The operation layer (plan §5.3): what the user asks the engine to *do*, as opposed to what it asks
//! it to *list*.
//!
//! Step 9a owns the two pieces that every operation needs before it can touch a byte:
//!
//! * [`guard`] — the pre-flight answers (write-target checks, the copy-into-itself containment rule);
//! * [`clipboard`] — the session clipboard model, its `text/uri-list` wire form, and the system-clipboard
//!   sync Go did with `wl-copy`/`xclip` (`api/ops.go:549-635`).
//!
//! The mutating operations themselves (`paste`, `copy`, `move`, `delete`, `rename`, `mkdir`, `chmod`,
//! `open`) and plan §5.3's `OpCtx` arrive with the steps that give them behaviour; they will call
//! [`guard`] rather than re-deriving any of it, which is the reason the guard is a module and not a
//! private function inside `paste`.
//!
//! Two rules bind everything under `ops/`:
//!
//! * **a rejection is typed, never a silent skip.** Go `continue`d past a copy-into-itself and reported
//!   success (plan §6.2); an `ops` function returns the [`crate::error::CoreError`] that names why.
//! * **no path logic of its own.** Containment, cleaning and name validation come from
//!   [`crate::path`] (`AbsPath::is_ancestor_of`, `contains_dot_component`, `validate_dest_name`), so there
//!   is exactly one implementation of "is this inside that" in the crate.
//!
//! [`crate::error::CoreError`]: crate::error::CoreError

pub mod clipboard;
pub mod guard;
