//! The panel's open state, remembered in `localStorage`. Every failure (no
//! storage, a private window, a quota error, a thrown accessor) means "start
//! closed" and is otherwise ignored: the panel must work without it.

const KEY: &str = "ghostframe.statsPanel.open";

/// `"1"` is open; anything else, or nothing, is closed.
pub(crate) fn parse_open(stored: Option<&str>) -> bool {
    stored == Some("1")
}

fn storage() -> Option<web_sys::Storage> {
    // `local_storage()` is `Err` when access is denied and `Ok(None)` when
    // there is none; both are "no storage".
    web_sys::window()?.local_storage().ok()?
}

pub(crate) fn load_open() -> bool {
    let stored = storage().and_then(|s| s.get_item(KEY).ok()).flatten();
    parse_open(stored.as_deref())
}

pub(crate) fn save_open(open: bool) {
    if let Some(s) = storage() {
        let _ = s.set_item(KEY, if open { "1" } else { "0" });
    }
}

#[cfg(test)]
mod tests {
    use super::parse_open;

    #[test]
    fn only_an_explicit_one_is_open() {
        assert!(parse_open(Some("1")));
        assert!(!parse_open(Some("0")));
        assert!(!parse_open(Some("true")));
        assert!(!parse_open(Some("")));
        assert!(!parse_open(None));
    }
}
