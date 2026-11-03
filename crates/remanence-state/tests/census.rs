//! Migration ratchets for SQL mutation, audit production and text-based policy.
//! Tokenization joins wrapped chains and ignores comments. Keys use the enclosing
//! function, never a line number; counts also catch an added site in that function.
//! All files are scanned to EOF. Only cfg(test) items/blocks are removed, using
//! balanced tokens (comments and string contents never act as delimiters).
//! This is a conservative lexical ratchet, not Rust type resolution: append
//! candidates and event readers are retained and labelled in the producer table.

use std::collections::{BTreeMap, BTreeSet};
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
        if t[end - 1].literal {
            return end - 1;
        }
        if t[end - 1].text == "?" {
            end -= 1;
            continue;
        }
        if matches!(t[end - 1].text.as_str(), ")" | "]") {
            let close = t[end - 1].text.clone();
            let open = if close == ")" { "(" } else { "[" };
            let mut depth = 1;
            end -= 1;
            while end > 0 && depth > 0 {
                end -= 1;
                if !t[end].literal {
                    if t[end].text == close {
                        depth += 1;
                    }
                    if t[end].text == open {
                        depth -= 1;
                    }
                }
            }
        }
        if end > 0
            && !t[end - 1].literal
            && (t[end - 1].text.as_bytes()[0].is_ascii_alphabetic()
                || t[end - 1].text.starts_with('_'))
        {
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

// Find a balanced token group, without treating string contents as syntax.
fn group_end(t: &[Token], start: usize) -> usize {
    let close = match t[start].text.as_str() {
        "(" => ")",
        "[" => "]",
        "{" => "}",
        _ => panic!("not a group"),
    };
    let mut i = start + 1;
    while i < t.len() {
        if !t[i].literal {
            if t[i].text == close {
                return i + 1;
            }
            if matches!(t[i].text.as_str(), "(" | "[" | "{") {
                i = group_end(t, i);
                continue;
            }
        }
        i += 1;
    }
    panic!("unclosed token group");
}

fn production_tokens(source: &str) -> Vec<Token> {
    let mut t = tokens(source);
    let mut type_bodies = Vec::new();
    let mut i = 0;
    while i < t.len() {
        if sequence(&t[i..], &["#", "[", "cfg", "(", "test", ")", "]"]) {
            let start = i;
            i += 7;
            let mut angles = 0usize;
            let mut initializer = false;
            let declaration = type_bodies.last() == Some(&true)
                || t[i..]
                    .iter()
                    .take_while(|x| x.literal || !matches!(x.text.as_str(), "=" | "{" | ";"))
                    .any(|x| {
                        !x.literal
                            && matches!(
                                x.text.as_str(),
                                "fn" | "impl" | "struct" | "enum" | "trait" | "type" | "use"
                            )
                    });
            // Skip the complete attributed item/block, including any subsequent
            // attributes and signature groups. Continue at its sibling, not EOF.
            while i < t.len() {
                if !t[i].literal {
                    match t[i].text.as_str() {
                        "{" => {
                            i = group_end(&t, i);
                            break;
                        }
                        "=" if angles == 0 => initializer = true,
                        "<" if angles > 0
                            || (!initializer && declaration)
                            || (i >= 2 && sequence(&t[i - 2..], &[":", ":"])) =>
                        {
                            angles += 1
                        }
                        ">" if i == 0 || t[i - 1].text != "-" => angles = angles.saturating_sub(1),
                        ";" => {
                            i += 1;
                            break;
                        }
                        "," if angles == 0 => {
                            i += 1;
                            break;
                        }
                        "}" => break,
                        "(" | "[" => {
                            i = group_end(&t, i);
                            continue;
                        }
                        _ => (),
                    }
                }
                i += 1;
            }
            t.drain(start..i);
            i = start;
        } else {
            if sequence(&t[i..], &["{"]) {
                let begin = (0..i)
                    .rev()
                    .find(|&j| !t[j].literal && matches!(t[j].text.as_str(), ";" | "{" | "}"))
                    .map_or(0, |j| j + 1);
                type_bodies.push(
                    t[begin..i].iter().any(|x| {
                        !x.literal && matches!(x.text.as_str(), "struct" | "enum" | "union")
                    }),
                );
            } else if sequence(&t[i..], &["}"]) {
                type_bodies.pop();
            }
            i += 1;
        }
    }
    t
}

// Discover record-returning functions from signatures rather than a builder
// name list. The workspace union is used for cross-module calls (and aliases).
fn record_builders(t: &[Token]) -> BTreeSet<String> {
    let mut builders = BTreeSet::new();
    for i in 0..t.len() {
        if !sequence(&t[i..], &["fn"]) {
            continue;
        }
        let mut angles = 0usize;
        let parameters = (i + 2..t.len()).find(|&j| {
            if !t[j].literal {
                match t[j].text.as_str() {
                    "<" => angles += 1,
                    ">" if j == 0 || t[j - 1].text != "-" => angles = angles.saturating_sub(1),
                    "(" if angles == 0 => return true,
                    _ => (),
                }
            }
            false
        });
        let Some(parameters) = parameters else {
            continue;
        };
        let result = group_end(t, parameters);
        let end = (result..t.len())
            .find(|&j| sequence(&t[j..], &["{"]) || sequence(&t[j..], &[";"]))
            .unwrap_or(t.len());
        if sequence(&t[result..], &["-", ">"])
            && t[result..end]
                .iter()
                .any(|x| !x.literal && x.text == "AuditEventRecord")
        {
            builders.insert(t[i + 1].text.clone());
        }
    }
    builders
}

fn text_origin(t: &Token, aliases: &BTreeSet<String>) -> bool {
    !t.literal
        && (matches!(
            t.text.as_str(),
            "message" | "to_string" | "detail" | "error" | "reason" | "summary"
        ) || t.text.ends_with("_error")
            || t.text.ends_with("_reason")
            || t.text.ends_with("_detail")
            || t.text.ends_with("_message")
            || t.text.ends_with("_summary")
            || aliases.contains(&t.text))
}

// Conservative local dataflow: propagate error/message origins through let
// bindings, including lowercasing, borrowing, formatting and alias chains.
fn text_aliases(t: &[Token]) -> BTreeSet<String> {
    let mut aliases = BTreeSet::new();
    loop {
        let before = aliases.len();
        for i in 0..t.len() {
            if !sequence(&t[i..], &["let"]) {
                continue;
            }
            let end = (i + 1..t.len())
                .find(|&j| sequence(&t[j..], &[";"]))
                .unwrap_or(t.len());
            if let Some(eq) = (i + 1..end).find(|&j| sequence(&t[j..], &["="])) {
                // Error decorators describe failure, not the successful value
                // bound by let. Do not taint that value through their closures.
                let value_end = (eq + 1..end)
                    .find(|&j| {
                        !t[j].literal
                            && matches!(
                                t[j].text.as_str(),
                                "map_err" | "ok_or" | "ok_or_else" | "unwrap_or_else" | "{" | "|"
                            )
                    })
                    .unwrap_or(end);
                if t[eq + 1..value_end]
                    .iter()
                    .any(|x| text_origin(x, &aliases))
                {
                    let binding_end = (i + 1..eq)
                        .find(|&j| sequence(&t[j..], &[":"]))
                        .unwrap_or(eq);
                    for x in &t[i + 1..binding_end] {
                        if !x.literal
                            && x.text != "mut"
                            && x.text != "ref"
                            && x.text
                                .chars()
                                .next()
                                .is_some_and(|c| c.is_alphabetic() || c == '_')
                        {
                            aliases.insert(x.text.clone());
                        }
                    }
                }
            }
        }
        if before == aliases.len() {
            return aliases;
        }
    }
}

fn function_end(t: &[Token], start: usize) -> usize {
    let mut i = start;
    while i < t.len() {
        if !t[i].literal {
            match t[i].text.as_str() {
                "{" => return group_end(t, i),
                ";" => return i,
                "(" | "[" => {
                    i = group_end(t, i);
                    continue;
                }
                _ => (),
            }
        }
        i += 1;
    }
    i
}

fn scan(path: &str, source: &str, kind: &str) -> BTreeMap<String, usize> {
    scan_with_builders(path, source, kind, &BTreeSet::new())
}

fn scan_with_builders(
    path: &str,
    source: &str,
    kind: &str,
    builders: &BTreeSet<String>,
) -> BTreeMap<String, usize> {
    let t = production_tokens(source);
    let mut builders = builders
        .union(&record_builders(&t))
        .cloned()
        .collect::<BTreeSet<_>>();
    // Import aliases of builders retain the same coverage.
    for w in t.windows(3) {
        if builders.contains(&w[0].text) && sequence(&w[1..], &["as"]) {
            builders.insert(w[2].text.clone());
        }
    }
    let mut alias_scopes: Vec<(usize, BTreeSet<String>)> = Vec::new();
    let mut found = BTreeMap::new();
    let mut scopes: Vec<Option<String>> = Vec::new();
    let mut pending = None;
    let mut signature_brackets = 0usize;
    for i in 0..t.len() {
        while alias_scopes.last().is_some_and(|(end, _)| i >= *end) {
            alias_scopes.pop();
        }
        let rest = &t[i..];
        let token = &t[i];
        if sequence(rest, &["fn"])
            && t.get(i + 1).is_some_and(|t| {
                t.text.as_bytes()[0].is_ascii_alphabetic() || t.text.starts_with('_')
            })
        {
            let end = function_end(&t, i);
            alias_scopes.push((end, text_aliases(&t[i..end])));
            pending = Some(t[i + 1].text.clone());
            signature_brackets = 0;
        }
        let empty_aliases = BTreeSet::new();
        let aliases = alias_scopes
            .last()
            .map_or(&empty_aliases, |(_, aliases)| aliases);
        if !token.literal {
            match token.text.as_str() {
                "{" => {
                    scopes.push(pending.take());
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
                            "insert"
                                | "update"
                                | "delete"
                                | "create"
                                | "drop"
                                | "alter"
                                | "replace"
                        ) && matches!(
                            w[1].as_str(),
                            "into"
                                | "or"
                                | "from"
                                | "table"
                                | "index"
                                | "trigger"
                                | "unique"
                                | "temp"
                                | "temporary"
                                | "view"
                        )
                    }) || (words.iter().any(|w| w == "update")
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
                    && (sequence(rest, &["AuditEventRecord", "{"])
                        || sequence(rest, &["AuditRecord", "{"]))
                    && t.get(i.wrapping_sub(1)).is_none_or(|t| t.text != "struct")
                {
                    add(&token.text);
                }
                // This also catches qualified traits and generic impl headers.
                if sequence(rest, &["AuditSink", "for"]) {
                    add("impl-AuditSink");
                }
                if !token.literal
                    && matches!(token.text.as_str(), "append" | "append_and_return_record")
                    && sequence(&t[i + 1..], &["("])
                    && i > 0
                    && !sequence(&t[i - 1..], &["fn"])
                {
                    // Without type resolution, count every append candidate. The
                    // table labels non-audit calls; receiver spelling is irrelevant.
                    add("audit-append");
                }
                if sequence(rest, &[".", "audit", "(", ")"]) {
                    add("audit-access");
                }
                if path == "crates/remanence-state/src/state.rs" && sequence(rest, &["fn", "audit"])
                {
                    add("public-audit-sink");
                }
                if sequence(rest, &["AuditEvent", ":", ":"])
                    && t.get(i + 3)
                        .is_some_and(|x| x.text.starts_with(char::is_uppercase))
                {
                    add(&format!("AuditEvent::{}", t[i + 3].text));
                }
                if !token.literal
                    && builders.contains(&token.text)
                    && sequence(&t[i + 1..], &["("])
                    && i > 0
                    && !sequence(&t[i - 1..], &["fn"])
                {
                    add(&format!("record-builder::{}", token.text));
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
                        "contains"
                            | "starts_with"
                            | "ends_with"
                            | "split_once"
                            | "strip_prefix"
                            | "strip_suffix"
                            | "find"
                    )
                    && i > 0
                    && sequence(&t[i - 1..], &["."])
                {
                    let begin = receiver_start(&t, i - 1);
                    if t[begin..i].iter().any(|x| text_origin(x, aliases)) {
                        add(&format!("text-{}", token.text));
                    }
                }
                if sequence(rest, &["=", "="]) || sequence(rest, &["!", "="]) {
                    let begin = receiver_start(&t, i);
                    // Include both operands: a literal, variable or format! may
                    // appear on either side of the comparison.
                    let mut end = i + 2;
                    while end < t.len() {
                        if !t[end].literal {
                            if matches!(
                                t[end].text.as_str(),
                                ";" | "{" | "}" | "&" | "|" | "," | ")"
                            ) {
                                break;
                            }
                            if matches!(t[end].text.as_str(), "(" | "[") {
                                end = group_end(&t, end);
                                continue;
                            }
                        }
                        end += 1;
                    }
                    let operands = &t[begin..end];
                    let string_evidence = operands.iter().any(|x| {
                        x.literal
                            || matches!(
                                x.text.as_str(),
                                "message" | "to_string" | "as_str" | "format"
                            )
                    }) || ((i - begin == 1
                        && aliases.contains(&t[begin].text))
                        || (end == i + 3 && aliases.contains(&t[i + 2].text)))
                        || (operands.iter().any(|x| x.text == "reason")
                            && sequence(rest, &["!", "="]));
                    if string_evidence && operands.iter().any(|x| text_origin(x, aliases)) {
                        add("text-equality");
                    }
                }
                if sequence(rest, &["matches", "!", "("]) {
                    let mut end = i + 3;
                    while end < t.len() && !sequence(&t[end..], &[","]) {
                        if !t[end].literal && matches!(t[end].text.as_str(), "(" | "[") {
                            end = group_end(&t, end);
                        } else {
                            end += 1;
                        }
                    }
                    let close = group_end(&t, i + 2);
                    if t[i + 3..end].iter().any(|x| text_origin(x, aliases))
                        && t[end..close].iter().any(|x| x.literal)
                    {
                        add("text-matches");
                    }
                }
                if sequence(rest, &["match"]) {
                    let end = (i + 1..t.len())
                        .find(|&j| sequence(&t[j..], &["{"]))
                        .unwrap_or(t.len());
                    if end < t.len() && t[i + 1..end].iter().any(|x| text_origin(x, aliases)) {
                        // Only string-pattern arms, not literals inside arm bodies.
                        let close = group_end(&t, end);
                        if (end + 1..close).any(|j| {
                            t[j].literal
                                && (sequence(&t[j + 1..], &["=", ">"])
                                    || sequence(&t[j + 1..], &["|"]))
                        }) {
                            add("text-match");
                        }
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
    let sources: Vec<_> = paths
        .iter()
        .map(|file| (file, fs::read_to_string(file).unwrap()))
        .collect();
    let builders = sources
        .iter()
        .flat_map(|(_, source)| record_builders(&production_tokens(source)))
        .collect();
    let mut actual = BTreeMap::new();
    for (file, source) in sources {
        let path = file.strip_prefix(&workspace).unwrap().to_str().unwrap();
        actual.extend(scan_with_builders(path, &source, kind, &builders));
        if path.starts_with("crates/remanence-state/src/log_frame/") {
            let tokens = tokens(&source);
            assert!(
                !tokens.iter().any(|t| !t.literal
                    && matches!(t.text.as_str(), "set_len" | "ftruncate" | "truncate")),
                "shared log frame must not truncate: {path}"
            );
        }
    }
    println!(
        "{kind}: {} keys / {} occurrences",
        actual.len(),
        actual.values().sum::<usize>()
    );
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

/// The review's fifteen missing occurrences plus aliases, formatted RHS and
/// reversed comparisons must be discovered without a function-name allowlist.
#[test]
fn scanner_text_decisions_follow_values() {
    let source = r#"
        fn arbitrary(error: &str, summary: &str, status: Status) {
            let lower = error.to_ascii_lowercase();
            let renamed = lower;
            renamed.contains("transport error"); lower.contains("b");
            lower.contains("c"); lower.contains("d"); lower.contains("e");
            lower.contains("f"); lower.contains("g"); lower.contains("h");
            summary.contains("EPERM"); summary.contains("permission denied");
            status.message() == format!("drive bay {bay} is busy");
            status.message() != format!("drive bay {bay} is busy");
            let further = status.message();
            further.strip_prefix("busy"); further.strip_suffix("busy");
            matches!(further.as_str(), "busy" | "idle");
            "busy" == further;
            "busy" == summary;
            further == summary;
            match further { "busy" => retry(), _ => stop() }
        }
    "#;
    let found = scan("example.rs", source, "text-decision");
    assert_eq!(found.values().sum::<usize>(), 19, "{found:#?}");
}

/// Each census must resume after test items, including nested modules and a
/// cfg(test) function/impl; strings and comments cannot affect brace matching.
fn assert_scan_resumes_after_test_items(kind: &str) {
    let body = r#"fn production(error: &str) {
        c.execute("create table t (n int)");
        let r = AuditEventRecord { event: AuditEvent::DriveFenced };
        sink.append(r);
        error.contains("busy");
    }"#;
    let source = format!(
        r#"
        #[cfg(test)] mod tests {{ mod nested {{ {body} }} const S: &str = "}}"; }}
        #[cfg(test)] fn helper() {{ {body} }}
        #[cfg(test)] impl Fake {{ {body} }}
        #[cfg(not(test))] mod runtime {{ {body} }}
        {body}
    "#
    );
    let expected = scan("example.rs", body, kind)
        .into_iter()
        .map(|(key, n)| (key, n * 2))
        .collect();
    assert_eq!(scan("example.rs", &source, kind), expected, "{kind}");
}

#[test]
fn scanner_sql_resumes_after_test_items() {
    assert_scan_resumes_after_test_items("sql-mutation");
}
#[test]
fn scanner_audit_resumes_after_test_items() {
    assert_scan_resumes_after_test_items("audit-producer");
}
#[test]
fn scanner_text_resumes_after_test_items() {
    assert_scan_resumes_after_test_items("text-decision");
}

/// Renaming an audit receiver cannot remove an append from the ratchet; each
/// event-selected caller and record builder is independently visible.
#[test]
fn scanner_audit_events_builders_and_renamed_receivers() {
    let source = r#"
        fn build() -> AuditEventRecord { AuditEventRecord { event: AuditEvent::DriveFenced } }
        fn emit(handle: Handle) {
            let w = handle.audit(); w.append(build());
            let z = w; z.append_and_return_record(build());
            record_session_event(AuditEvent::SessionOpened);
            progress.audit(AuditEvent::OperationFinished);
        }
    "#;
    let found = scan("example.rs", source, "audit-producer");
    for (key, count) in [
        ("build::AuditEventRecord", 1),
        ("build::AuditEvent::DriveFenced", 1),
        ("emit::audit-append", 2),
        ("emit::audit-access", 1),
        ("emit::record-builder::build", 2),
        ("emit::AuditEvent::SessionOpened", 1),
        ("emit::AuditEvent::OperationFinished", 1),
    ] {
        assert_eq!(
            found.get(&format!("example.rs::{key}")),
            Some(&count),
            "{found:#?}"
        );
    }
}

/// Mutating prepared statements need literal coverage even without execute().
#[test]
fn scanner_sql_statement_shapes() {
    for statement in [
        "create temp table t (n int)",
        "CREATE TEMPORARY TABLE t (n int)",
        "create view v as select 1",
        "replace into t values (1)",
        "with v as (select 1) update t set n = 2",
    ] {
        let source = format!("fn mutate() {{ db.prepare({statement:?}).query([]); }}");
        assert_eq!(
            scan("example.rs", &source, "sql-mutation")
                .values()
                .sum::<usize>(),
            1,
            "{statement}"
        );
    }
}

/// Record factories are discovered across modules and import renames. Receiving
/// a record as an argument does not make a function a record factory.
#[test]
fn scanner_cross_module_record_factory_and_alias() {
    let definitions = production_tokens(
        "fn new_record<F: Fn() -> bool>() -> AuditEventRecord {} fn consume(r: AuditEventRecord) -> bool {}",
    );
    let builders = record_builders(&definitions);
    assert_eq!(builders, BTreeSet::from(["new_record".into()]));
    let found = scan_with_builders(
        "caller.rs",
        "use module::new_record as renamed; fn caller() { w.append(renamed()); }",
        "audit-producer",
        &builders,
    );
    assert_eq!(
        found.get("caller.rs::caller::record-builder::renamed"),
        Some(&1)
    );
    assert_eq!(found.get("caller.rs::caller::audit-append"), Some(&1));
}

/// Fields and generic test functions cannot consume their production siblings.
#[test]
fn scanner_cfg_fields_and_generic_items() {
    let source = r#"
        #[cfg(test)] fn fixture<A, B>() { error.contains("test"); }
        struct S { #[cfg(test)] fixture: Pair<A, B>, live: usize }
        fn after(error: &str) {
            #[cfg(test)] let comparison = 1 < 2;
            let value = S { #[cfg(test)] fixture: 1 < 2, live: error.contains("field") };
            error.contains("busy");
        }
    "#;
    assert_eq!(
        scan("example.rs", source, "text-decision"),
        BTreeMap::from([("example.rs::after::text-contains".into(), 2)])
    );
}

/// The checked-in producer dispositions must cover every census key and count,
/// including readers/candidates and the public sink with all its accessors.
#[test]
fn audit_producer_table_matches_ratchet_and_sink_callers() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/census");
    let baseline = fs::read_to_string(directory.join("audit-producer.tsv")).unwrap();
    let expected: BTreeMap<_, _> = baseline
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let (key, n) = line.split_once('\t').unwrap();
            (key.to_owned(), n.to_owned())
        })
        .collect();
    let table = fs::read_to_string(directory.join("library-producers.tsv")).unwrap();
    let mut actual = BTreeMap::new();
    let mut public_sink = None;
    let mut callers = Vec::new();
    for line in table.lines().skip(1) {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 10, "{line}");
        let key = format!("{}::{}::{}", fields[0], fields[1], fields[6]);
        assert!(
            actual.insert(key, fields[7].to_owned()).is_none(),
            "duplicate table key"
        );
        assert!(!fields[8].is_empty(), "unclassified row: {line}");
        if fields[6] == "public-audit-sink" {
            assert!(
                public_sink.replace(fields[9]).is_none(),
                "multiple public sink entries"
            );
        }
        if fields[6] == "audit-access" {
            callers.push(format!("{}::{} ({} call)", fields[0], fields[1], fields[7]));
        }
    }
    assert_eq!(actual, expected);
    assert_eq!(public_sink.unwrap(), callers.join("; "));
}
