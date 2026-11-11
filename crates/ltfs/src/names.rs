//! LTFS names may hold characters Windows forbids in file names (`:` `?`
//! `*` `"` `<` `>` `|` `\`, control characters, a trailing dot or space).
//! They are shown with these characters moved to the private-use area
//! (U+F000 + the character), as Windows' own Linux subsystem does, so every
//! name has a Windows form and comes back unchanged.

const BASE: u32 = 0xF000;

fn forbidden(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20
}

/// The Windows form of an LTFS name.
pub fn to_windows(name: &str) -> String {
    let n = name.chars().count();
    name.chars()
        .enumerate()
        .map(|(i, c)| {
            let trailing = i + 1 == n && (c == '.' || c == ' ') && name != "." && name != "..";
            if forbidden(c) || trailing {
                char::from_u32(BASE + c as u32).unwrap()
            } else {
                c
            }
        })
        .collect()
}

/// The LTFS name of a Windows form.
pub fn from_windows(name: &str) -> String {
    name.chars()
        .map(|c| {
            let v = c as u32;
            if (BASE..BASE + 0x80).contains(&v) {
                let orig = char::from_u32(v - BASE).unwrap();
                if forbidden(orig) || orig == '.' || orig == ' ' {
                    return orig;
                }
            }
            c
        })
        .collect()
}

/// Whether an inner path is safe to write under a folder on disk: no `..`,
/// no drive letters or absolute parts.
pub fn safe_relative(path: &str) -> bool {
    !path.is_empty() && path.split(['\\', '/']).all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains(':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for n in ["plain.txt", "a:b?c*.txt", "trailing.", "space ", "tab\there", "back\\slash", "příliš 🦎"] {
            let w = to_windows(n);
            assert!(!w.chars().any(forbidden), "{w:?}");
            assert!(!w.ends_with('.') && !w.ends_with(' '));
            assert_eq!(from_windows(&w), n);
        }
        assert_eq!(to_windows("plain.txt"), "plain.txt");
        assert!(safe_relative("a\\b.txt") && !safe_relative("..\\x") && !safe_relative("C:\\x") && !safe_relative(""));
    }
}
