use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Opaque identity for a server-owned terminal.
///
/// During the pane-backed transition this is stored one-to-one beside panes,
/// but callers must not derive it from a pane id or layout position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalId(String);

static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);
// A replacement imports at most the handoff descriptor limit's worth of IDs.
// Keep them reserved even after a terminal closes so old receipts cannot rebind.
static IMPORTED_TERMINAL_IDS: OnceLock<Mutex<HashSet<TerminalId>>> = OnceLock::new();

impl TerminalId {
    pub fn alloc() -> Self {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let imported = IMPORTED_TERMINAL_IDS
            .get()
            .map(|ids| ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        Self::alloc_avoiding(micros, &NEXT_TERMINAL_ID, imported.as_deref())
    }

    fn alloc_avoiding(micros: u128, counter: &AtomicU64, imported: Option<&HashSet<Self>>) -> Self {
        loop {
            let counter = counter.fetch_add(1, Ordering::Relaxed);
            let id = Self(format!("term_{micros:x}{counter:x}"));
            if imported.is_none_or(|ids| !ids.contains(&id)) {
                return id;
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn reserve_imported(ids: impl Iterator<Item = Self>) {
        IMPORTED_TERMINAL_IDS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_skips_imported_ids_after_clock_and_counter_repeat() {
        let imported = HashSet::from([
            TerminalId("term_abc1".into()),
            TerminalId("term_abc2".into()),
        ]);
        let counter = AtomicU64::new(1);
        assert_eq!(
            TerminalId::alloc_avoiding(0xabc, &counter, Some(&imported)).as_str(),
            "term_abc3"
        );
        assert_eq!(
            TerminalId::alloc_avoiding(0xabc, &counter, Some(&imported)).as_str(),
            "term_abc4"
        );
        assert_eq!(imported.len(), 2);
    }
}
