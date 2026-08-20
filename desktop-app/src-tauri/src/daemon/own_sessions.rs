// The daemon shells out to Claude Code to read usage, and each of those runs is
// itself a Claude Code session — which `claude agents --json` reports (that
// listing is project-scoped, so the daemon sees its own spawns) and which would
// otherwise show up on the device as phantom "daemon" sessions.
//
// Every spawn gets an explicit --session-id recorded here, and the session
// sources skip anything in this set. Bounded so it cannot grow without limit.
// Mirrors daemon/src/own-sessions.js.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

const MAX: usize = 40;

struct OwnSessions {
    order: VecDeque<String>,
    set: HashSet<String>,
}

fn store() -> &'static Mutex<OwnSessions> {
    static OWN: OnceLock<Mutex<OwnSessions>> = OnceLock::new();
    OWN.get_or_init(|| {
        Mutex::new(OwnSessions {
            order: VecDeque::new(),
            set: HashSet::new(),
        })
    })
}

pub fn mark_own_session(id: impl Into<String>) {
    let id = id.into();
    let mut s = store().lock().unwrap();
    if s.set.insert(id.clone()) {
        s.order.push_back(id);
    }
    while s.order.len() > MAX {
        if let Some(oldest) = s.order.pop_front() {
            s.set.remove(&oldest);
        }
    }
}

pub fn is_own_session(id: &str) -> bool {
    store().lock().unwrap().set.contains(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_and_drops_oldest() {
        for i in 0..(MAX + 5) {
            mark_own_session(format!("own-sessions-test-{i}"));
        }
        assert!(!is_own_session("own-sessions-test-0"));
        assert!(is_own_session(&format!("own-sessions-test-{}", MAX + 4)));
    }
}
