/// Interpret backslash escape sequences in a string
pub fn interpret_escapes(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some('0') => result.push('\0'),
                Some('a') => result.push('\x07'),
                Some('b') => result.push('\x08'),
                Some('f') => result.push('\x0C'),
                Some('v') => result.push('\x0B'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_newline() {
        assert_eq!(interpret_escapes(r"hello\nworld"), "hello\nworld");
    }

    #[test]
    fn test_escape_tab() {
        assert_eq!(interpret_escapes(r"hello\tworld"), "hello\tworld");
    }

    #[test]
    fn test_escape_carriage_return() {
        assert_eq!(interpret_escapes(r"hello\rworld"), "hello\rworld");
    }

    #[test]
    fn test_escape_backslash() {
        assert_eq!(interpret_escapes(r"hello\\world"), "hello\\world");
    }

    #[test]
    fn test_escape_null() {
        assert_eq!(interpret_escapes(r"hello\0world"), "hello\0world");
    }

    #[test]
    fn test_escape_bell() {
        assert_eq!(interpret_escapes(r"hello\aworld"), "hello\x07world");
    }

    #[test]
    fn test_escape_backspace() {
        assert_eq!(interpret_escapes(r"hello\bworld"), "hello\x08world");
    }

    #[test]
    fn test_escape_form_feed() {
        assert_eq!(interpret_escapes(r"hello\fworld"), "hello\x0Cworld");
    }

    #[test]
    fn test_escape_vertical_tab() {
        assert_eq!(interpret_escapes(r"hello\vworld"), "hello\x0Bworld");
    }

    #[test]
    fn test_unknown_escape_preserved() {
        assert_eq!(interpret_escapes(r"hello\xworld"), r"hello\xworld");
    }

    #[test]
    fn test_trailing_backslash() {
        assert_eq!(interpret_escapes("hello\\"), "hello\\");
    }

    #[test]
    fn test_no_escapes() {
        assert_eq!(interpret_escapes("hello world"), "hello world");
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(interpret_escapes(""), "");
    }

    #[test]
    fn test_multiple_escapes() {
        assert_eq!(interpret_escapes(r"a\nb\tc\r"), "a\nb\tc\r");
    }

    #[test]
    fn test_consecutive_escapes() {
        assert_eq!(interpret_escapes(r"\n\n\n"), "\n\n\n");
    }

    #[test]
    fn test_double_backslash_then_n() {
        // Input r"\\n" = three chars: \, \, n
        // \\ -> single \, then 'n' is literal
        let result = interpret_escapes(r"\\n");
        assert_eq!(result.len(), 2);
        assert_eq!(result.as_bytes()[0], b'\\');
        assert_eq!(result.as_bytes()[1], b'n');
    }
}
