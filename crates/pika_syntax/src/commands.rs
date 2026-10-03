//! The names of the built-in commands, and suggestions for misspelled ones.

/// The built-in forms: commands that the parser reads with a syntax of their own, such as
/// `:struct Name {...}` and `:if (cond) do={...}`.
pub const FORMS: &[&str] = &[
    "local", "const", "global", "set", "if", "while", "do", "for", "foreach", "match", "onerror",
    "unsafe", "fn", "struct", "enum", "trait", "impl", "use", "extern", "test",
];

/// Every built-in command name: the forms, and the commands called like functions. Functions
/// cannot be given these names (spec section 3.6).
pub const RESERVED_COMMANDS: &[&str] = &[
    "local", "const", "global", "set", "if", "do", "while", "for", "foreach", "match", "break",
    "continue", "return", "error", "onerror", "panic", "fn", "struct", "enum", "trait", "impl",
    "use", "extern", "unsafe", "test", "put", "len", "typeof", "tostr", "assert", "nothing",
    "default",
];

/// The name among `candidates` that `name` is most likely a misspelling of, if one is close
/// enough: one edit away, or for names of four letters or more, two edits away from a name
/// with the same first letter; a single letter is too short to guess from. An edit inserts,
/// removes or replaces a letter, or swaps two letters next to each other. Ties go to the first
/// candidate.
pub fn closest<'c>(name: &str, candidates: impl IntoIterator<Item = &'c str>) -> Option<&'c str> {
    let limit = match name.chars().count() {
        0 | 1 => return None,
        2 | 3 => 1,
        _ => 2,
    };
    candidates
        .into_iter()
        .filter(|&candidate| candidate != name)
        .map(|candidate| (edit_distance(name, candidate), candidate))
        // Two edits make many unrelated names close (`area` and `break`): they count only for
        // a name that starts like the candidate.
        .filter(|&(distance, candidate)| {
            distance == 1 || (distance <= limit && candidate.chars().next() == name.chars().next())
        })
        .min_by_key(|&(distance, _)| distance)
        .map(|(_, candidate)| candidate)
}

/// The number of edits between `a` and `b`: insertions, removals, replacements and swaps of
/// adjacent characters (the optimal string alignment distance).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // `rows[i][j]`: the distance between the first `i` characters of `a` and `j` of `b`.
    let mut rows = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let replace = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + replace);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::{FORMS, RESERVED_COMMANDS, closest};

    #[test]
    fn misspellings_are_matched() {
        let commands = || RESERVED_COMMANDS.iter().copied();
        assert_eq!(closest("emun", commands()), Some("enum"));
        assert_eq!(closest("strcut", commands()), Some("struct"));
        assert_eq!(closest("fucn", commands()), Some("fn"));
        assert_eq!(closest("retrun", commands()), Some("return"));
        assert_eq!(closest("putt", commands()), Some("put"));
        assert_eq!(closest("fun", commands()), Some("fn"));
        assert_eq!(closest("put", commands()), None);
        assert_eq!(closest("area", commands()), None);
        assert_eq!(closest("x", commands()), None);
    }

    #[test]
    fn every_form_is_reserved() {
        for form in FORMS {
            assert!(RESERVED_COMMANDS.contains(form), "`{form}` is not reserved");
        }
    }
}
