//! The Rust half of the fixture: a function a test reaches through a helper,
//! which is the shape `affected` has to walk more than one hop to see.
//!
//! The calls here are deliberately plain. A call written inside a macro —
//! `assert_eq!(total(&[]), 0)` — is a token tree the parser does not walk, so
//! no edge is recorded and no verb can see it. One test below is written that
//! way on purpose: it is what `affected`'s lower bound means, and what `--safe`
//! is for.

/// What the change edits.
pub fn total(items: &[u32]) -> u32 {
    let mut sum = 0;
    for item in items {
        sum += *item;
    }
    sum
}

/// One hop between the change and the test that covers it.
pub fn summary(items: &[u32]) -> String {
    let sum = total(items);
    let mut out = items.len().to_string();
    out.push_str(" items, ");
    out.push_str(&sum.to_string());
    out.push_str(" in all");
    out
}

/// Reaches nothing the change touches, so it must stay out of the set.
pub fn unrelated(name: &str) -> String {
    name.to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_counts_and_sums() {
        let out = summary(&[1, 2, 3]);
        assert_eq!(out, "3 items, 6 in all");
    }

    #[test]
    fn an_empty_slice_totals_zero() {
        let sum = total(&[]);
        assert_eq!(sum, 0);
    }

    #[test]
    fn a_name_is_shouted() {
        let out = unrelated("ada");
        assert_eq!(out, "ADA");
    }

    /// Reaches `total` only from inside a macro, so no edge exists and
    /// `affected` cannot name it. `--safe` is the answer to this test.
    #[test]
    fn a_macro_is_the_only_caller() {
        assert_eq!(total(&[2, 2]), 4);
    }
}
