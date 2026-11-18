//! Shared fixture for the `zenman-ui` integration tests: a live `Session` over a private
//! temporary data directory. The *tests* do IO — the crate under test never does (rule 7); the
//! engine IO is the sanctioned path (`Session::open` is core's own seam).

use zenman_core::platform;
use zenman_core::session::{Session, SessionPaths};
use zenman_core::store::config::ZenConfig;

/// A session in `zenman-ui-test-<pid>-<tag>`; two tests with different tags never collide.
#[must_use]
pub fn session(tag: &str) -> Session {
    let dir = std::env::temp_dir().join(format!("zenman-ui-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the temp directory is creatable");
    let paths = SessionPaths::new(
        dir.join("config.json"),
        dir.join("action.log"),
        None,
        dir.join("probe"),
    )
    .with_db_open(1, std::time::Duration::from_millis(1));
    let (session, _notes) = Session::open(
        paths,
        ZenConfig::default(),
        platform::default_env(),
        ZenConfig::default().limits(),
    )
    .expect("a session opens over a fresh directory");
    session
}
