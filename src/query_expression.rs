//! The query language front end for `query`: Bazel's query `Lexer` and the
//! `QueryParser` decisions that separate a single target literal from every
//! other expression (query2/engine/Lexer.java:119-318,
//! query2/engine/QueryParser.java:60-80, 261-357). Only a lone target literal
//! is evaluated natively; functions, operators, `let`, `set` and parenthesized
//! expressions are explicit gaps. Syntax errors use Bazel's texts.

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Eof,
    Comma,
    Equals,
    LParen,
    Minus,
    Plus,
    RParen,
    Caret,
    In,
    Let,
    Set,
    Intersect,
    Except,
    Union,
}

impl Token {
    /// `Lexer.Token.toString`: a word itself, otherwise the pretty name.
    fn display(&self) -> &str {
        match self {
            Token::Word(word) => word,
            Token::Eof => "EOF",
            Token::Comma => ",",
            Token::Equals => "=",
            Token::LParen => "(",
            Token::Minus => "-",
            Token::Plus => "+",
            Token::RParen => ")",
            Token::Caret => "^",
            Token::In => "in",
            Token::Let => "let",
            Token::Set => "set",
            Token::Intersect => "intersect",
            Token::Except => "except",
            Token::Union => "union",
        }
    }

    fn is_binary_operator(&self) -> bool {
        matches!(
            self,
            Token::Intersect
                | Token::Caret
                | Token::Union
                | Token::Plus
                | Token::Except
                | Token::Minus
        )
    }
}

/// Lexer.java keywordMap: the token kinds after `__ALL_IDENTIFIERS_FOLLOW`.
fn keyword(word: &str) -> Option<Token> {
    Some(match word {
        "in" => Token::In,
        "let" => Token::Let,
        "set" => Token::Set,
        "intersect" => Token::Intersect,
        "except" => Token::Except,
        "union" => Token::Union,
        _ => return None,
    })
}

/// `Lexer.scan` (Lexer.java:119-123, 290-312).
fn scan(input: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
        let c = chars[pos];
        pos += 1;
        match c {
            '(' => tokens.push(Token::LParen),
            ')' => tokens.push(Token::RParen),
            ',' => tokens.push(Token::Comma),
            '+' => tokens.push(Token::Plus),
            '-' => tokens.push(Token::Minus),
            '=' => tokens.push(Token::Equals),
            '^' => tokens.push(Token::Caret),
            '\n' | ' ' | '\t' | '\r' => {}
            '\'' | '"' => {
                // quotedWord (Lexer.java:150-166): up to the same quote.
                let start = pos;
                loop {
                    let Some(&next) = chars.get(pos) else {
                        return Err("unclosed quotation".to_owned());
                    };
                    pos += 1;
                    if next == c {
                        break;
                    }
                }
                tokens.push(Token::Word(chars[start..pos - 1].iter().collect()));
            }
            _ => {
                // scanWord (Lexer.java:173-275).
                let start = pos - 1;
                let double_at = c == '@' && chars.get(pos) == Some(&'@');
                while let Some(&next) = chars.get(pos) {
                    let word_char = next.is_ascii_alphanumeric()
                        || "*/@.-_:$~[]".contains(next)
                        || (next == '+' && double_at)
                        || !next.is_ascii();
                    if !word_char {
                        break;
                    }
                    pos += 1;
                }
                let word: String = chars[start..pos].iter().collect();
                tokens.push(keyword(&word).unwrap_or(Token::Word(word)));
            }
        }
    }
    tokens.push(Token::Eof);
    Ok(tokens)
}

/// The query functions of Bazel 9.3's default `query` environment, as listed
/// by its unknown-function error (QueryParser.java:110-122).
const FUNCTIONS: [&str; 16] = [
    "allpaths",
    "attr",
    "buildfiles",
    "deps",
    "executables",
    "filter",
    "kind",
    "labels",
    "loadfiles",
    "rdeps",
    "same_pkg_direct_rdeps",
    "siblings",
    "some",
    "somepath",
    "tests",
    "visible",
];

/// QueryParser.appendInputContext (:159-168): up to 3 tokens, not EOF.
fn context(tokens: &[Token], index: usize) -> String {
    let max = (tokens.len() - 1).min(index + 3);
    tokens[index..max.max(index)]
        .iter()
        .map(Token::display)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `TargetLiteral.toString` (query2/engine/TargetLiteral.java:89-115).
fn literal_display(word: &str) -> String {
    let quoting = keyword(word).is_some()
        || word.is_empty()
        || word.starts_with('-')
        || word.starts_with('*')
        || word.chars().any(|c| " \"#&'()+,;<=>?[]{|}".contains(c));
    if !quoting {
        return word.to_owned();
    }
    let quote = if word.contains('"') { '\'' } else { '"' };
    format!("{quote}{word}{quote}")
}

/// `QueryExpression.truncate` (QueryExpression.java:102-108).
fn truncate(text: &str) -> String {
    const MAX: usize = 1000;
    const MARKER: &str = "[truncated]";
    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX - MARKER.len()).collect();
    format!("{kept}{MARKER}")
}

/// The single target literal of a `query` expression (the command's residue
/// joined by spaces, QueryOptionHelper.java:41-48), or Bazel's syntax error /
/// the native gap for any other expression.
pub fn target_literal(query: &str) -> Result<String, String> {
    let parse_error =
        |message: String| format!("Error while parsing '{}': {message}", truncate(query));
    let tokens = scan(query).map_err(parse_error)?;
    let not_implemented = || {
        format!(
            "query expression '{}' is not implemented in the native profile: only a single target pattern is evaluated",
            truncate(query)
        )
    };
    match &tokens[0] {
        Token::Word(word) => match &tokens[1] {
            Token::LParen if !FUNCTIONS.contains(&word.as_str()) => Err(parse_error(format!(
                "unknown function '{word}' at '{}'; expected one of ['{}']",
                context(&tokens, 0),
                FUNCTIONS.join("', '")
            ))),
            Token::LParen => Err(not_implemented()),
            next => {
                // validateTargetLiteral (QueryParser.java:347-357).
                if word.starts_with('-') || word.starts_with('*') {
                    return Err(parse_error(format!(
                        "target literal must not begin with ({}): {word}",
                        &word[..1]
                    )));
                }
                if next.is_binary_operator() {
                    Err(not_implemented())
                } else if *next != Token::Eof {
                    Err(parse_error(format!(
                        "unexpected token '{}' after query expression '{}'",
                        next.display(),
                        truncate(&literal_display(word))
                    )))
                } else {
                    Ok(word.clone())
                }
            }
        },
        Token::Let | Token::LParen | Token::Set => Err(not_implemented()),
        // QueryParser.syntaxError (:89-107).
        Token::Eof => Err(parse_error("premature end of input".to_owned())),
        _ => Err(parse_error(format!(
            "syntax error at '{}'",
            context(&tokens, 0)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_literals_and_quoted_words() {
        for (query, literal) in [
            ("//...", "//..."),
            ("//p:*", "//p:*"),
            ("@@rules++ext+repo//:x", "@@rules++ext+repo//:x"),
            ("'//sp ace:a b'", "//sp ace:a b"),
            ("\"//(group)/...\"", "//(group)/..."),
            ("//caf\u{e9}:x", "//caf\u{e9}:x"),
            ("  //a:b  ", "//a:b"),
        ] {
            assert_eq!(target_literal(query).unwrap(), literal, "{query:?}");
        }
    }

    #[test]
    fn bazel_query_syntax_errors() {
        for (query, error) in [
            (
                "//sp ace:all",
                "Error while parsing '//sp ace:all': unexpected token 'ace:all' after query expression '//sp'",
            ),
            (
                "@a b//:x",
                "Error while parsing '@a b//:x': unexpected token 'b//:x' after query expression '@a'",
            ),
            (
                "//(group)/...",
                "Error while parsing '//(group)/...': unknown function '//' at '// ( group'; expected one of ['allpaths', 'attr', 'buildfiles', 'deps', 'executables', 'filter', 'kind', 'labels', 'loadfiles', 'rdeps', 'same_pkg_direct_rdeps', 'siblings', 'some', 'somepath', 'tests', 'visible']",
            ),
            ("", "Error while parsing '': premature end of input"),
            ("'//x", "Error while parsing ''//x': unclosed quotation"),
            (
                "- //x",
                "Error while parsing '- //x': syntax error at '- //x'",
            ),
            (
                "'-x'",
                "Error while parsing ''-x'': target literal must not begin with (-): -x",
            ),
            (
                "'a b' c",
                "Error while parsing ''a b' c': unexpected token 'c' after query expression '\"a b\"'",
            ),
        ] {
            assert_eq!(target_literal(query).unwrap_err(), error, "{query:?}");
        }
        for query in [
            "deps(//x)",
            "//a + //b",
            "//a - //b",
            "let x = //a in $x",
            "set(//a)",
            "(//a)",
        ] {
            assert!(
                target_literal(query)
                    .unwrap_err()
                    .contains("is not implemented in the native profile"),
                "{query:?}"
            );
        }
    }
}
