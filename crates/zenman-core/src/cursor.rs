//! The pagination cursor (plan §5.2 `cursor.rs` row).
//!
//! **This is an index into a list that is fully re-read and re-sorted on every request**, not a
//! keyset and not an opaque token: `docs/contracts.md` → *Cursor and pagination*, `api/dir.go:111-115`
//! (`startIndex` from `strconv.Atoi`) and `:274` (`nextCursor = strconv.Itoa(endIndex)`).
//! The consequences are documented there and reproduced on purpose — concurrent edits between page
//! fetches shift the index and silently skip or duplicate entries, and paging a big directory is O(n)
//! per page. Keeping the *same* type in the port means the golden captures stay comparable; hiding a
//! keyset inside this newtype would not.
//!
//! The response `cursor` is the index of the *next* item, and the empty string when there is no next
//! page — [`Cursor::IS_LAST`] (`dir-many-last.json`).

use serde::Deserialize;
use serde::Serialize;

/// A decimal offset into the sorted entry list.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Cursor(pub u64);

impl Cursor {
    /// The wire value of the `cursor` field on the last page: `""` (`api/dir.go:275-277`,
    /// `dir-many-last.json`).
    pub const IS_LAST: &str = "";

    /// First page, exactly what Go uses when `cursor` is absent or unparsable
    /// (`api/dir.go:112-115`: `strconv.Atoi` error, or a negative value, leaves `startIndex = 0`).
    pub const FIRST: Cursor = Cursor(0);

    /// Parse Go's decimal cursor. Anything Go's `strconv.Atoi` rejects, or rejects by the `c >= 0`
    /// guard, becomes [`Cursor::FIRST`] — parity, including for the base64-looking `"MTIz"`
    /// (plan §5.2 `cursor.rs` *Done when*).
    #[must_use]
    pub fn decode(raw: &str) -> Self {
        // Go accepts "-0"; `u64` does not, and it means the first page anyway.
        if raw == "-0" {
            return Self::FIRST;
        }
        match raw.parse::<u64>() {
            Ok(v) => Self(v),
            Err(_) => Self::FIRST,
        }
    }

    /// Go's `strconv.Itoa` of the index — always decimal, never empty (use [`Cursor::wire`] for the
    /// response field).
    #[must_use]
    pub fn encode(self) -> String {
        self.0.to_string()
    }

    /// The value the `cursor` field actually carries: `encode()` while more pages follow,
    /// [`Cursor::IS_LAST`] on the last page (`api/dir.go:275-277`).
    #[must_use]
    pub fn wire(self, has_more: bool) -> String {
        if has_more {
            self.encode()
        } else {
            Self::IS_LAST.to_string()
        }
    }

    /// The cursor of the page after `self`, given the page that was just produced held `produced`
    /// entries (Go's `endIndex`, `api/dir.go:222`). Saturating: a listing cannot honestly continue
    /// past `u64::MAX` items, and a wrap to 0 would restart the walk from the first page.
    #[must_use]
    pub fn next(self, produced: usize) -> Self {
        Self(self.0.saturating_add(produced as u64))
    }

    /// `true` for the first page.
    #[must_use]
    pub fn is_first(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.encode())
    }
}

/// One page of a listing: the slice, the cursor for the following page, and whether there is one.
///
/// Mirrors Go's `pageEntries`/`nextCursor`/`hasMore` triple (`api/dir.go:221-231`, `:275`) including
/// the two edges that the goldens pin: `has_more` is false when the page lands exactly on the end
/// (`endIndex < totalCount` is the condition, `:225`), and a cursor past the end yields an empty
/// slice rather than an error.
#[must_use]
pub fn page<T>(all: &[T], cursor: Cursor, limit: usize) -> (&[T], Cursor, bool) {
    let total = all.len();
    let start = cursor.0 as usize;
    if start >= total {
        return (&[], cursor.next(0), false);
    }
    // Go clamps `endIndex` to `totalCount` (`:223-227`); `limit` is clamped to at least 1 upstream
    // (a 0 limit would loop forever), so the caller's floor is `crate::limits::MIN_PAGE_SIZE`.
    let want = limit.max(1);
    let end = start.saturating_add(want).min(total);
    let has_more = end < total;
    (&all[start..end], cursor.next(end - start), has_more)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for n in [0_u64, 1, 199, 200, 999, 1000, 1213, u64::MAX] {
            let c = Cursor(n);
            assert_eq!(Cursor::decode(&c.encode()), c, "roundtrip {n}");
            assert_eq!(c.to_string(), n.to_string());
        }
        // Go's own wire values from the goldens (`dir-many-p1.json` cursor "200").
        assert_eq!(Cursor::decode("200"), Cursor(200));
        assert_eq!(Cursor(200).encode(), "200");
        assert!(
            Cursor::decode("200")
                .encode()
                .bytes()
                .all(|b| b.is_ascii_digit())
        );
    }

    #[test]
    fn decode_is_parity_not_base64() {
        // Plan §5.2 `cursor.rs` *Done when*: `decode("MTIz") == Cursor(0)`.
        assert_eq!(Cursor::decode("MTIz"), Cursor(0));
        assert_eq!(Cursor::decode(""), Cursor(0));
        assert_eq!(Cursor::decode("  "), Cursor(0));
        assert_eq!(Cursor::decode("-1"), Cursor(0)); // Go's `c >= 0` guard, api/dir.go:113
        assert_eq!(Cursor::decode("-0"), Cursor(0));
        assert_eq!(Cursor::decode("1.5"), Cursor(0));
        assert_eq!(Cursor::decode("200x"), Cursor(0));
        // Go's `strconv.Atoi` accepts a leading sign, so parity does too.
        assert_eq!(Cursor::decode("+7"), Cursor(7));
        // ... and rejects surrounding space, so parity leaves it at the first page.
        assert_eq!(Cursor::decode(" 42 "), Cursor(0));
        assert_eq!(Cursor::decode("42 "), Cursor(0));
    }

    #[test]
    fn wire_is_empty_on_the_last_page() {
        assert_eq!(Cursor(1214).wire(false), Cursor::IS_LAST);
        assert_eq!(Cursor::IS_LAST, "");
        assert_eq!(Cursor(200).wire(true), "200");
        assert_eq!(Cursor::decode(Cursor::IS_LAST), Cursor::FIRST);
    }

    #[test]
    fn next_is_go_end_index() {
        assert_eq!(Cursor::FIRST.next(200), Cursor(200));
        assert_eq!(Cursor(200).next(200), Cursor(400));
        assert_eq!(Cursor(u64::MAX).next(10), Cursor(u64::MAX));
    }

    #[test]
    fn paging_matches_the_measured_page_shapes() {
        let all: Vec<u32> = (0..1214).collect();
        // dir-many-p1.json: 200 entries, cursor "200", has_more true, loaded_so_far 200.
        let (p1, c1, more1) = page(&all, Cursor::FIRST, 200);
        assert_eq!(p1.len(), 200);
        assert_eq!(c1, Cursor(200));
        assert!(more1);
        assert_eq!(c1.wire(more1), "200");
        // dir-many-p2.json
        let (p2, c2, more2) = page(&all, c1, 200);
        assert_eq!(p2.first().copied(), Some(200));
        assert_eq!(c2, Cursor(400));
        assert!(more2);
        // dir-many-last.json: a short page closes the walk with cursor "" / has_more false.
        let (last, c_last, more_last) = page(&all, Cursor(1200), 200);
        assert_eq!(last.len(), 14);
        assert!(!more_last);
        assert_eq!(c_last.wire(more_last), "");
        // dir-many-smallpage.json: an exact-multiple final page is *not* has_more (api/dir.go:225).
        let (_, c, more) = page(&[1, 2, 3, 4], Cursor(2), 2);
        assert_eq!(c, Cursor(4));
        assert!(!more);
    }

    #[test]
    fn paging_edges_never_panic() {
        let empty: [u8; 0] = [];
        let (p, c, more) = page(&empty, Cursor::FIRST, 200);
        assert!(p.is_empty());
        assert!(!more);
        assert_eq!(c, Cursor::FIRST);
        // A cursor past the end (the documented consequence of a re-read list, dir.go:111-115).
        let (p, c, more) = page(&[1, 2], Cursor(9), 5);
        assert!(p.is_empty());
        assert!(!more);
        assert_eq!(c, Cursor(9));
        // A zero limit is clamped to 1 rather than looping.
        let (p, _, _) = page(&[1, 2, 3], Cursor::FIRST, 0);
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn serde_is_a_bare_number() {
        let json = serde_json::to_string(&Cursor(200)).unwrap();
        assert_eq!(json, "200");
        assert_eq!(serde_json::from_str::<Cursor>("200").unwrap(), Cursor(200));
    }
}
