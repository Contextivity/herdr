#[cfg(unix)]
use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Opaque identity for a server-owned terminal.
///
/// During the pane-backed transition this is stored one-to-one beside panes,
/// but callers must not derive it from a pane id or layout position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalId(String);

static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(unix)]
static IMPORTED_TERMINAL_IDS: OnceLock<Mutex<HashSet<TerminalId>>> = OnceLock::new();

impl TerminalId {
    pub fn alloc() -> Self {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let next_id = || {
            let counter = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
            Self(format!("term_{micros:x}{counter:x}"))
        };
        #[cfg(unix)]
        loop {
            let id = next_id();
            if IMPORTED_TERMINAL_IDS
                .get()
                .is_some_and(|ids| ids.lock().unwrap_or_else(|p| p.into_inner()).contains(&id))
            {
                continue;
            }
            return id;
        }
        #[cfg(not(unix))]
        next_id()
    }

    #[cfg(unix)]
    pub(crate) fn reserve_imported(ids: impl Iterator<Item = Self>) {
        IMPORTED_TERMINAL_IDS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(ids);
    }

    #[cfg(unix)]
    pub(crate) fn is_valid(&self) -> bool {
        self.0.strip_prefix("term_").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
