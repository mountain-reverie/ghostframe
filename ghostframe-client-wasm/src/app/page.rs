// SPDX-License-Identifier: MIT OR Apache-2.0
//! What the page URL asks for.

use crate::web::logic::Cdf53Mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFlags {
    /// `e2e` is present, with any value: the e2e suite drives this page.
    pub e2e: bool,
    /// One `e2e` value is `lossless` (`?e2e=1&e2e=lossless`): no page chrome,
    /// the canvas pinned top-left and no stats panel, so a screenshot holds
    /// only the canvas.
    pub lossless: bool,
    /// The `cdf53` parameter (`cpu` forces the CPU decoder; default `auto`).
    pub cdf53: Cdf53Mode,
}

impl PageFlags {
    /// `e2e_values` is `URLSearchParams.getAll("e2e")` (one empty string for a
    /// bare `?e2e`), `cdf53` is `URLSearchParams.get("cdf53")`.
    pub fn from_params(e2e_values: &[String], cdf53: Option<&str>) -> Self {
        PageFlags {
            e2e: !e2e_values.is_empty(),
            lossless: e2e_values.iter().any(|v| v == "lossless"),
            cdf53: Cdf53Mode::parse(cdf53.unwrap_or("auto")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_harness_url_is_e2e_and_lossless_when_asked() {
        // ghostframe-e2e: `?e2e=1&debug=1` plus `&e2e=lossless` for the golden.
        let f = PageFlags::from_params(&v(&["1", "lossless"]), None);
        assert!(f.e2e && f.lossless);
        assert_eq!(f.cdf53, Cdf53Mode::Auto);
        let f = PageFlags::from_params(&v(&["1"]), Some("cpu"));
        assert!(f.e2e && !f.lossless);
        assert_eq!(f.cdf53, Cdf53Mode::Cpu);
    }

    #[test]
    fn a_bare_e2e_counts_and_no_e2e_is_production() {
        assert!(PageFlags::from_params(&v(&[""]), None).e2e);
        let f = PageFlags::from_params(&[], None);
        assert!(!f.e2e && !f.lossless);
        assert!(
            !PageFlags::from_params(&v(&["losless"]), None).lossless,
            "a typo is not lossless"
        );
        // A bare prefix is not a match either: `loss` must not count as `lossless`.
        assert!(
            !PageFlags::from_params(&v(&["loss"]), None).lossless,
            "a prefix is not lossless"
        );
    }
}
