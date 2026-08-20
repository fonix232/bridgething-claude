// Mirrors daemon/src/persist.js: atomic on-disk JSON state (temp file +
// rename) so a crash mid-write can never leave a half-written file that
// fails to parse on the next boot. Missing or corrupt is not an error, just
// "nothing saved yet".

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::path::Path;

pub fn read_state<T: DeserializeOwned>(state_dir: &Path, name: &str) -> Option<T> {
    let path = state_dir.join(format!("{name}.json"));
    match fs::read_to_string(&path) {
        Ok(body) => serde_json::from_str(&body).ok(),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                crate::daemon::log::log("ST", &format!("could not read {name} state: {err}"));
            }
            None
        }
    }
}

pub fn write_state<T: Serialize>(state_dir: &Path, name: &str, value: &T) -> bool {
    let target = state_dir.join(format!("{name}.json"));
    let tmp = state_dir.join(format!("{name}.json.{}.tmp", std::process::id()));

    let attempt = fs::create_dir_all(state_dir)
        .and_then(|_| serde_json::to_string(value).map_err(std::io::Error::other))
        .and_then(|body| fs::write(&tmp, body))
        .and_then(|_| fs::rename(&tmp, &target));

    match attempt {
        Ok(()) => true,
        Err(err) => {
            crate::daemon::log::log("ST", &format!("could not write {name} state: {err}"));
            let _ = fs::remove_file(&tmp);
            false
        }
    }
}
