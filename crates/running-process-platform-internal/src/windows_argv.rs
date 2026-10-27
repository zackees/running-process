//! Windows CRT argument quoting over raw UTF-16 code units.

/// Quote a Windows CRT argument without losing non-scalar UTF-16 units.
pub(crate) fn quote_arg_wide(arg: &[u16]) -> Vec<u16> {
    const SPACE: u16 = b' ' as u16;
    const QUOTE: u16 = b'"' as u16;
    const SLASH: u16 = b'\\' as u16;
    if !arg.is_empty() && !arg.iter().any(|c| matches!(*c, SPACE | 9..=11 | QUOTE)) {
        return arg.to_vec();
    }
    let mut out = vec![QUOTE];
    let mut index = 0;
    while index < arg.len() {
        let mut slashes = 0;
        while index < arg.len() && arg[index] == SLASH {
            slashes += 1;
            index += 1;
        }
        if index == arg.len() {
            out.extend(std::iter::repeat_n(SLASH, slashes * 2));
            break;
        }
        if arg[index] == QUOTE {
            out.extend(std::iter::repeat_n(SLASH, slashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n(SLASH, slashes));
        }
        out.push(arg[index]);
        index += 1;
    }
    out.push(QUOTE);
    out
}

#[cfg(test)]
mod tests {
    use super::quote_arg_wide;

    #[test]
    fn preserves_unpaired_surrogates_and_escapes_without_loss() {
        let input = [0xd800, b' ' as u16, b'"' as u16, b'\\' as u16];
        let expected = [
            b'"' as u16,
            0xd800,
            b' ' as u16,
            b'\\' as u16,
            b'"' as u16,
            b'\\' as u16,
            b'\\' as u16,
            b'"' as u16,
        ];
        assert_eq!(quote_arg_wide(&input), expected);
    }
}
