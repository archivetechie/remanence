//! Migration ratchets for SQL mutation, audit production and text-based policy.
//! Tokenization joins wrapped chains and ignores comments. Keys use the enclosing
//! function, never a line number; counts also catch an added site in that function.
//! The deliberately lexical scan includes cfg(test) blocks inside production
//! functions, but stops before the first inline cfg(test) module in each source file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Token {
    text: String,
    literal: bool,
}

fn tokens(source: &str) -> Vec<Token> {
    let b = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"//") {
            i += b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .unwrap_or(b.len() - i);
            continue;
        }
        if b[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        let start = i;
        let raw_start = if b[i] == b'r' || b[i..].starts_with(b"br") {
            Some(i + if b[i] == b'b' { 2 } else { 1 })
        } else {
            None
        };
        if let Some(mut quote) = raw_start {
            while quote < b.len() && b[quote] == b'#' {
                quote += 1;
            }
            if b.get(quote) == Some(&b'"') {
                let hashes = quote - raw_start.unwrap();
                i = quote + 1;
                loop {
                    assert!(i < b.len(), "unterminated raw string");
                    if b[i] == b'"'
                        && b.get(i + 1..i + 1 + hashes) == Some(&b[quote - hashes..quote])
                    {
                        out.push(Token {
                            text: source[quote + 1..i].into(),
                            literal: true,
                        });
                        i += 1 + hashes;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }
        if b[i] == b'"' || b[i..].starts_with(b"b\"") {
            if b[i] == b'b' {
                i += 1;
            }
            let quote = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                } else if b[i] == b'"' {
                    break;
                } else {
                    i += 1;
                }
            }
            out.push(Token {
                text: source[quote + 1..i].into(),
                literal: true,
            });
            i += 1;
        } else if b[i] == b'\'' && (b.get(i + 2) == Some(&b'\'') || b.get(i + 1) == Some(&b'\\')) {
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                } else if b[i] == b'\'' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            out.push(Token {
                text: source[start..i].into(),
                literal: true,
            });
        } else {
            i += 1;
            if b[start].is_ascii_alphanumeric() || b[start] == b'_' {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
            } else {
                while !source.is_char_boundary(i) {
                    i += 1;
                }
            }
            out.push(Token {
                text: source[start..i].into(),
                literal: false,
            });
        }
    }
    out
}

fn sequence(t: &[Token], words: &[&str]) -> bool {
    t.len() >= words.len() && t.iter().zip(words).all(|(t, w)| !t.literal && t.text == *w)
}

// Walk one receiver/expression backwards, respecting call parentheses. This
// avoids attributing a later comparison to an unrelated error earlier in a match.
fn receiver_start(t: &[Token], mut end: usize) -> usize {
    loop {
        if end == 0 {
            return 0;
        }
        if t[end - 1].text == ")" {
            let mut depth = 1;
            end -= 1;
            while end > 0 && depth > 0 {
                end -= 1;
                if !t[end].literal {
                    match t[end].text.as_str() {
                        ")" => depth += 1,
                        "(" => depth -= 1,
                        _ => (),
                    }
                }
            }
        }
        if end > 0 && !t[end - 1].literal && t[end - 1].text.as_bytes()[0].is_ascii_alphabetic() {
            end -= 1;
        }
        if end > 0 && t[end - 1].text == "." {
            end -= 1;
        } else if end >= 2 && sequence(&t[end - 2..], &[":", ":"]) {
            end -= 2;
        } else {
            return end;
        }
    }
}

fn excluded(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        name == "tests"
            || name == "test"
            || name == "tests.rs"
            || name.ends_with("_tests.rs")
            || name.starts_with("test_")
    })
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if excluded(&path) {
            continue;
        }
        if path.is_dir() {
            files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

fn scan(path: &str, source: &str, kind: &str) -> BTreeMap<String, usize> {
    let t = tokens(source);
    let mut found = BTreeMap::new();
    let mut scopes: Vec<Option<String>> = Vec::new();
    let mut pending = None;
    let mut test_attribute = false;
    let mut signature_brackets = 0usize;
    for i in 0..t.len() {
        let rest = &t[i..];
        let token = &t[i];
        if sequence(rest, &["#", "[", "cfg", "(", "test", ")", "]"]) {
            test_attribute = true;
        }
        if test_attribute && sequence(rest, &["mod"]) {
            let terminator = t[i..]
                .iter()
                .find(|t| !t.literal && matches!(t.text.as_str(), "{" | ";"));
            if terminator.is_some_and(|t| t.text == "{") {
                break;
            }
        }
        if sequence(rest, &["fn"])
            && t.get(i + 1).is_some_and(|t| {
                t.text.as_bytes()[0].is_ascii_alphabetic() || t.text.starts_with('_')
            })
        {
            test_attribute = false;
            pending = Some(t[i + 1].text.clone());
            signature_brackets = 0;
        }
        if !token.literal {
            match token.text.as_str() {
                "{" => {
                    scopes.push(pending.take());
                    test_attribute = false;
                }
                "}" => {
                    scopes.pop();
                }
                "[" if pending.is_some() => signature_brackets += 1,
                "]" if pending.is_some() => {
                    signature_brackets = signature_brackets.saturating_sub(1)
                }
                ";" if signature_brackets == 0 => {
                    pending = None;
                    test_attribute = false;
                }
                _ => (),
            }
        }
        let name = scopes
            .iter()
            .rev()
            .find_map(|s| s.as_deref())
            .or(pending.as_deref())
            .unwrap_or("<module>");
        let mut add = |pattern: &str| {
            *found
                .entry(format!("{path}::{name}::{pattern}"))
                .or_insert(0) += 1;
        };
        match kind {
            "sql-mutation" => {
                if sequence(rest, &["Connection", ":", ":"])
                    && t.get(i + 3).is_some_and(|x| x.text.starts_with("open"))
                {
                    add("Connection::open");
                }
                if sequence(rest, &["."])
                    && t.get(i + 1).is_some_and(|x| {
                        matches!(
                            x.text.as_str(),
                            "execute"
                                | "execute_batch"
                                | "transaction"
                                | "transaction_with_behavior"
                                | "unchecked_transaction"
                        )
                    })
                {
                    add(&t[i + 1].text);
                }
                if token.literal {
                    let words: Vec<_> = token
                        .text
                        .split_whitespace()
                        .map(str::to_ascii_lowercase)
                        .collect();
                    if words.windows(2).any(|w| {
                        matches!(
                            w[0].as_str(),
                            "insert" | "update" | "delete" | "create" | "drop" | "alter"
                        ) && matches!(
                            w[1].as_str(),
                            "into" | "or" | "from" | "table" | "index" | "trigger" | "unique"
                        )
                    }) || (words.first().is_some_and(|w| w == "update")
                        && words.iter().any(|w| w == "set"))
                        || (token.text.to_ascii_lowercase().contains("user_version")
                            && token.text.contains('='))
                    {
                        add("mutating-sql");
                    }
                }
                if sequence(rest, &[".", "pragma_update", "("])
                    && t[i + 3..]
                        .iter()
                        .take_while(|t| t.text != ")")
                        .any(|t| t.literal && t.text.eq_ignore_ascii_case("user_version"))
                {
                    add("user_version-assignment");
                }
            }
            "audit-producer" => {
                if path == "crates/remanence-api/src/audit_projection.rs" && sequence(rest, &["fn"])
                {
                    add("audit-hub");
                }
                if pending.is_none()
                    && sequence(rest, &["AuditEventRecord", "{"])
                    && t.get(i.wrapping_sub(1)).is_none_or(|t| t.text != "struct")
                {
                    add("AuditEventRecord");
                }
                // This also catches qualified traits and generic impl headers.
                if sequence(rest, &["AuditSink", "for"]) {
                    add("impl-AuditSink");
                }
                if sequence(rest, &[".", "append"])
                    || sequence(rest, &[".", "append_and_return_record"])
                {
                    let begin = (0..i)
                        .rev()
                        .find(|&j| !t[j].literal && matches!(t[j].text.as_str(), ";" | "{" | "}"))
                        .map_or(0, |j| j + 1);
                    if t[begin..i].iter().any(|x| {
                        !x.literal
                            && (x.text.contains("audit")
                                || matches!(x.text.as_str(), "sink" | "log"))
                    }) {
                        add("audit-append");
                    }
                }
                if !token.literal
                    && matches!(
                        token.text.as_str(),
                        "append_and_project_audit"
                            | "append_and_project_audit_locked"
                            | "append_operation_audit"
                            | "fire_audit"
                    )
                    && t.get(i + 1).is_some_and(|x| x.text == "(")
                    && t.get(i.wrapping_sub(1)).is_none_or(|x| x.text != "fn")
                {
                    add(&token.text);
                }
            }
            "text-decision" => {
                if path.starts_with("crates/remanence-cli/src/dev/") {
                    continue;
                }
                if !token.literal
                    && matches!(
                        token.text.as_str(),
                        "contains" | "starts_with" | "ends_with" | "split_once"
                    )
                    && i > 0
                    && t[i - 1].text == "."
                {
                    let begin = receiver_start(&t, i - 1);
                    if t[begin..i].iter().any(|x| {
                        !x.literal
                            && (matches!(
                                x.text.as_str(),
                                "message" | "to_string" | "detail" | "error" | "reason"
                            ) || x.text.ends_with("_error")
                                || x.text.ends_with("_reason")
                                || x.text.ends_with("_detail"))
                    }) {
                        add(&format!("text-{}", token.text));
                    }
                }
                if sequence(rest, &["=", "="]) || sequence(rest, &["!", "="]) {
                    let begin = receiver_start(&t, i);
                    if (t.get(i + 2).is_some_and(|x| x.literal)
                        || (sequence(rest, &["!", "="])
                            && t[begin..i].iter().any(|x| x.text == "reason")))
                        && t[begin..i].iter().any(|x| {
                            !x.literal
                                && matches!(
                                    x.text.as_str(),
                                    "message" | "to_string" | "detail" | "error" | "reason"
                                )
                        })
                    {
                        add("text-equality");
                    }
                }
                if sequence(rest, &["matches", "!", "("]) {
                    let end = (i + 3..t.len())
                        .find(|&j| t[j].text == ",")
                        .unwrap_or(t.len());
                    if t[i + 3..end].iter().any(|x| x.text == "reason")
                        && t[i + 3..end].iter().any(|x| x.text == "as_str")
                    {
                        add("matches-reason");
                    }
                }
            }
            _ => panic!("unknown census"),
        }
    }
    found
}

fn census(kind: &str) {
    let workspace = std::env::var_os("CENSUS_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    let mut paths = Vec::new();
    for entry in fs::read_dir(workspace.join("crates")).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            files(&src, &mut paths);
        }
    }
    let mut actual = BTreeMap::new();
    for file in paths {
        let path = file.strip_prefix(&workspace).unwrap().to_str().unwrap();
        let source = fs::read_to_string(&file).unwrap();
        actual.extend(scan(path, &source, kind));
        if path.starts_with("crates/remanence-state/src/log_frame/") {
            let tokens = tokens(&source);
            assert!(
                !tokens.iter().any(|t| !t.literal
                    && matches!(t.text.as_str(), "set_len" | "ftruncate" | "truncate")),
                "shared log frame must not truncate: {path}"
            );
        }
    }
    if let Some(dir) = std::env::var_os("CENSUS_EXPORT") {
        let output: String = actual
            .iter()
            .map(|(key, count)| format!("{key}\t{count}\n"))
            .collect();
        fs::write(Path::new(&dir).join(format!("{kind}.tsv")), output).unwrap();
    }
    let list = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/census/{kind}.tsv")),
    )
    .unwrap();
    let expected: BTreeMap<String, usize> = list
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|line| {
            let mut fields = line.split('\t');
            (
                fields.next().unwrap().to_string(),
                fields.next().unwrap().parse().unwrap(),
            )
        })
        .collect();
    let added: Vec<_> = actual
        .iter()
        .filter(|(k, v)| expected.get(*k) != Some(*v))
        .collect();
    let removed: Vec<_> = expected
        .iter()
        .filter(|(k, v)| actual.get(*k) != Some(*v))
        .collect();
    assert!(added.is_empty() && removed.is_empty(), "{kind} census: new/changed sites (file::enclosing_fn::pattern, count): {added:#?}\nDisappeared/changed sites; shrink the allowlist: {removed:#?}");
}

#[test]
fn sql_mutation_census() {
    census("sql-mutation");
}
#[test]
fn audit_producer_census() {
    census("audit-producer");
}
#[test]
fn text_decision_census() {
    census("text-decision");
}

#[test]
fn scanner_joins_chains_and_excludes_comments_and_test_modules() {
    let source = r#"
        // error.contains("ignored")
        fn decide() { if status.message()
            .contains("busy") { refuse(); } }
        #[cfg(test)] mod tests { fn ignored() { error.contains("test"); } }
    "#;
    assert_eq!(
        scan("example.rs", source, "text-decision"),
        BTreeMap::from([("example.rs::decide::text-contains".into(), 1)])
    );
    assert_eq!(
        scan(
            "example.rs",
            r#"fn _schema() { connection
                .pragma_update(None, "user_version", 9); }"#,
            "sql-mutation",
        ),
        BTreeMap::from([("example.rs::_schema::user_version-assignment".into(), 1)]),
    );
    assert_eq!(
        scan(
            "example.rs",
            "impl<T> remanence_state::AuditSink for Sink<T> {}",
            "audit-producer",
        ),
        BTreeMap::from([("example.rs::<module>::impl-AuditSink".into(), 1)]),
    );
}
