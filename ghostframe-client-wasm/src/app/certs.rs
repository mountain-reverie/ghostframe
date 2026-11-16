// SPDX-License-Identifier: MIT OR Apache-2.0
//! Which certificate hashes to pin, from `/config.json`.

/// A hash as `/config.json` carries it: non-empty, even-length hex.
pub fn is_hex_hash(s: &str) -> bool {
    !s.is_empty() && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `listed` is `certHashes` when it is an array (`None` per entry that is not
/// a string), `single` is `certHash` when it is a string. The full list wins
/// when it is non-empty and every entry is valid; an older server sends only
/// `certHash`. Every hash pinned: current first, then the successor.
pub fn pick_cert_hashes(
    listed: Option<&[Option<String>]>,
    single: Option<&str>,
) -> Result<Vec<String>, String> {
    if let Some(list) = listed {
        if !list.is_empty() && list.iter().all(|h| h.as_deref().is_some_and(is_hex_hash)) {
            return Ok(list.iter().flatten().cloned().collect());
        }
    }
    match single {
        Some(h) if is_hex_hash(h) => Ok(vec![h.to_string()]),
        _ => Err("bootstrap: /config.json missing certHash".to_string()),
    }
}

/// The bytes of a hash `is_hex_hash` accepted.
pub fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if !is_hex_hash(hex) {
        let head: String = hex.chars().take(32).collect();
        return Err(format!("bootstrap: invalid certHash hex: {head}…"));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn a_lone_cert_hash_is_pinned() {
        let h = "00112233445566778899aabbccddeeff".repeat(2);
        assert_eq!(pick_cert_hashes(None, Some(&h)), Ok(vec![h.clone()]));
        let b = hex_to_bytes(&h).unwrap();
        assert_eq!((b.len(), b[0], b[1], b[31]), (32, 0x00, 0x11, 0xff));
    }

    #[test]
    fn every_listed_hash_is_pinned_so_a_rotation_still_connects() {
        let (a, b) = ("aa".repeat(32), "bb".repeat(32));
        let list = [s(&a), s(&b)];
        assert_eq!(pick_cert_hashes(Some(&list), Some(&a)), Ok(vec![a, b]));
    }

    #[test]
    fn a_malformed_list_falls_back_to_cert_hash() {
        let a = "aa".repeat(32);
        assert_eq!(
            pick_cert_hashes(Some(&[s(&"zz".repeat(32))]), Some(&a)),
            Ok(vec![a.clone()])
        );
        assert_eq!(
            pick_cert_hashes(Some(&[None]), Some(&a)),
            Ok(vec![a.clone()]),
            "non-string entry"
        );
        assert_eq!(
            pick_cert_hashes(Some(&[]), Some(&a)),
            Ok(vec![a.clone()]),
            "empty list"
        );
        // A mixed list must not pin its valid entries: one bad entry rejects the list.
        let mixed = [s(&a), s(&"zz".repeat(32))];
        assert_eq!(
            pick_cert_hashes(Some(&mixed), Some(&a)),
            Ok(vec![a]),
            "mixed list"
        );
    }

    #[test]
    fn no_usable_hash_is_an_error_that_names_cert_hash() {
        for single in [None, Some(""), Some("abc"), Some("zz")] {
            let e = pick_cert_hashes(None, single).unwrap_err();
            assert!(e.contains("certHash"), "{e}");
        }
        assert!(hex_to_bytes("abc").is_err());
    }
}
