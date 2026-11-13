//! Migration ratchets for SQL mutation, audit production and text-based policy.
//! Tokenization joins wrapped chains and ignores comments. Keys use the enclosing
//! function, never a line number; counts also catch an added site in that function.
//! All files are scanned to EOF. Test-only cfg items and their module files are
//! removed using balanced tokens; any(test, ...) and not(test) remain visible.
//! This is a conservative lexical ratchet, not Rust type resolution: append
//! candidates and event readers are retained and labelled in the producer table.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct Token {
    text: String,
    literal: bool,
    string: bool,
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
                            string: true,
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
                string: true,
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
                string: false,
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
                string: false,
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
        if end > 0 && sequence(&t[end - 1..], &["!"]) {
            end -= 1;
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

// Only a bare test conjunct guarantees absence from production. any/not do not.
fn test_cfg(t: &[Token], i: usize) -> Option<(usize, bool)> {
    let inner = sequence(&t[i..], &["#", "!", "["]);
    let bracket = i + if inner { 2 } else { 1 };
    if !sequence(&t[i..], &["#"]) || !sequence(t.get(bracket..)?, &["[", "cfg", "("]) {
        return None;
    }
    let end = group_end(t, bracket);
    let start = bracket + 3;
    if sequence(&t[start..], &["test", ")"]) {
        return Some((end, inner));
    }
    if sequence(&t[start..], &["all", "("]) {
        let mut j = start + 2;
        while j < end - 2 {
            if sequence(&t[j..], &["test"])
                && (sequence(&t[j + 1..], &[","]) || sequence(&t[j + 1..], &[")"]))
            {
                return Some((end, inner));
            }
            if !t[j].literal && matches!(t[j].text.as_str(), "(" | "[" | "{") {
                j = group_end(t, j);
            } else {
                j += 1;
            }
        }
    }
    None
}

// Resolve explicit module paths in test-only regions, including test-named files.
// Inline modules change the relative path base according to Rust's module layout.
fn test_includes(source: &str, base: &Path, module_dir: &Path, whole: bool) -> BTreeSet<PathBuf> {
    // Compare lexical paths without depending on the included file existing.
    fn normalized(path: PathBuf) -> PathBuf {
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => (),
                std::path::Component::ParentDir
                    if out.file_name().is_some_and(|name| name != "..") =>
                {
                    out.pop();
                }
                component => out.push(component),
            }
        }
        out
    }
    fn walk(t: &[Token], base: &Path, module_dir: &Path, whole: bool, out: &mut BTreeSet<PathBuf>) {
        let mut i = 0;
        let mut test = whole;
        let mut path = None;
        while i < t.len() {
            if let Some((end, inner)) = test_cfg(t, i) {
                if inner {
                    walk(&t[end..], base, module_dir, true, out);
                    return;
                }
                test = true;
                i = end;
                continue;
            }
            if sequence(&t[i..], &["#", "[", "path", "="])
                && t.get(i + 4).is_some_and(|x| x.literal)
            {
                path = Some(t[i + 4].text.clone());
                i = group_end(t, i + 1);
                continue;
            }
            if sequence(&t[i..], &["mod"]) && i + 2 < t.len() {
                let name = &t[i + 1].text;
                if sequence(&t[i + 2..], &[";"]) {
                    if test {
                        if let Some(ref path) = path {
                            out.insert(base.join(path));
                        } else {
                            out.insert(module_dir.join(format!("{name}.rs")));
                            out.insert(module_dir.join(name).join("mod.rs"));
                        }
                    }
                    i += 3;
                } else if sequence(&t[i + 2..], &["{"]) {
                    let end = group_end(t, i + 2);
                    let nested = path
                        .as_ref()
                        .map_or_else(|| module_dir.join(name), |p| base.join(p));
                    walk(&t[i + 3..end - 1], &nested, &nested, test, out);
                    i = end;
                } else {
                    i += 1;
                }
                test = whole;
                path = None;
                continue;
            }
            if !t[i].literal && matches!(t[i].text.as_str(), "(" | "[" | "{") {
                let block = sequence(&t[i..], &["{"]);
                let end = group_end(t, i);
                walk(&t[i + 1..end - 1], base, module_dir, test, out);
                i = end;
                if block {
                    test = whole;
                    path = None;
                }
            } else {
                if sequence(&t[i..], &[";"]) {
                    test = whole;
                    path = None;
                }
                i += 1;
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&tokens(source), base, module_dir, whole, &mut out);
    out.into_iter().map(normalized).collect()
}

fn test_files(sources: &[(PathBuf, String)]) -> BTreeSet<PathBuf> {
    let mut skipped: BTreeSet<_> = sources
        .iter()
        .filter(|(p, _)| excluded(p))
        .map(|(p, _)| p.clone())
        .collect();
    loop {
        let before = skipped.len();
        for (path, source) in sources {
            let parent = path.parent().unwrap();
            let base = if matches!(
                path.file_name().and_then(|x| x.to_str()),
                Some("lib.rs" | "main.rs" | "mod.rs")
            ) {
                parent.to_path_buf()
            } else {
                parent.join(path.file_stem().unwrap())
            };
            skipped.extend(test_includes(source, parent, &base, skipped.contains(path)));
        }
        if skipped.len() == before {
            return skipped;
        }
    }
}

fn production_tokens(source: &str) -> Vec<Token> {
    let mut t = tokens(source);
    let mut type_bodies = Vec::new();
    let mut i = 0;
    while i < t.len() {
        if let Some((end, inner)) = test_cfg(&t, i) {
            let start = i;
            if inner {
                // Inner attributes govern their enclosing module, not following siblings.
                let mut end = i;
                while end < t.len() && !sequence(&t[end..], &["}"]) {
                    if !t[end].literal && matches!(t[end].text.as_str(), "(" | "[" | "{") {
                        end = group_end(&t, end);
                    } else {
                        end += 1;
                    }
                }
                t.drain(start..end);
                continue;
            }
            i = end;
            let mut is_fn = false;
            let mut is_let = false;
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
                        "fn" => is_fn = true,
                        "let" => is_let = true,
                        "{" => {
                            i = group_end(&t, i);
                            if is_let {
                                continue;
                            }
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
                        "," if angles == 0 && !is_fn && !is_let => {
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

// Canonical audit names survive use aliases, including aliases of aliases.
fn audit_names(t: &[Token]) -> BTreeMap<String, String> {
    let mut names: BTreeMap<_, _> = ["AuditEvent", "AuditEventRecord", "AuditRecord"]
        .into_iter()
        .map(|n| (n.to_owned(), n.to_owned()))
        .collect();
    loop {
        let before = names.len();
        for w in t.windows(3) {
            if sequence(&w[1..], &["as"]) {
                if let Some(canonical) = names.get(&w[0].text).cloned() {
                    names.insert(w[2].text.clone(), canonical);
                }
            }
        }
        if names.len() == before {
            return names;
        }
    }
}

fn record_impls(t: &[Token]) -> Vec<(usize, usize)> {
    let names = audit_names(t);
    (0..t.len())
        .filter_map(|i| {
            if !sequence(&t[i..], &["impl"]) {
                return None;
            }
            let end = (i + 1..t.len()).find(|&j| sequence(&t[j..], &["{"]))?;
            t[i + 1..end]
                .iter()
                .any(|x| names.get(&x.text).is_some_and(|n| n == "AuditEventRecord"))
                .then(|| (end, group_end(t, end)))
        })
        .collect()
}

// Discover record-returning functions from signatures rather than a builder
// name list. The workspace union is used for cross-module calls (and aliases).
fn record_builders(t: &[Token]) -> BTreeSet<String> {
    let mut builders = BTreeSet::new();
    let names = audit_names(t);
    let impls = record_impls(t);
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
            && t[result..end].iter().any(|x| {
                !x.literal
                    && (names.get(&x.text).is_some_and(|n| n == "AuditEventRecord")
                        || (x.text == "Self" && impls.iter().any(|&(a, b)| a < i && i < b)))
            })
        {
            builders.insert(t[i + 1].text.clone());
        }
    }
    builders
}

// The candidate census follows syntax, never error-like receiver names.
fn string_value(t: &[Token]) -> bool {
    let mut i = 0;
    while i < t.len() && sequence(&t[i..], &["&"]) {
        i += 1;
    }
    t.get(i).is_some_and(|x| x.string) || sequence(&t[i..], &["format", "!"])
}

fn text_aliases(t: &[Token]) -> BTreeSet<String> {
    let mut aliases = legacy_text_aliases(t);
    for i in 0..t.len() {
        if !sequence(&t[i..], &["let"]) {
            continue;
        }
        let end = (i + 1..t.len())
            .find(|&j| sequence(&t[j..], &[";"]))
            .unwrap_or(t.len());
        if let Some(eq) = (i + 1..end).find(|&j| sequence(&t[j..], &["="])) {
            if string_value(&t[eq + 1..end]) {
                let name = i + if sequence(&t[i + 1..], &["mut"]) {
                    2
                } else {
                    1
                };
                if t.get(name).is_some_and(|x| {
                    !x.literal && x.text.chars().all(|c| c.is_alphanumeric() || c == '_')
                }) {
                    aliases.insert(t[name].text.clone());
                }
            }
        }
    }
    aliases
}

fn text_origin(t: &Token, aliases: &BTreeSet<String>) -> bool {
    !t.literal
        && (matches!(
            t.text.as_str(),
            "message" | "to_string" | "detail" | "error" | "reason" | "summary" | "format"
        ) || t.text.ends_with("_error")
            || t.text.ends_with("_reason")
            || t.text.ends_with("_detail")
            || t.text.ends_with("_message")
            || t.text.ends_with("_summary")
            || aliases.contains(&t.text))
}

// Conservative local dataflow: propagate error/message origins through let
// bindings, including lowercasing, borrowing, formatting and alias chains.
fn legacy_text_aliases(t: &[Token]) -> BTreeSet<String> {
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

// Inspect patterns only; arm bodies and guards have their own method/equality sites.
fn literal_pattern(t: &[Token]) -> bool {
    let end = (0..t.len())
        .find(|&i| sequence(&t[i..], &["if"]))
        .unwrap_or(t.len());
    let mut i = 0;
    while i < end {
        if sequence(&t[i..], &["#", "["]) {
            i = group_end(t, i + 1);
        } else if t[i].string {
            return true;
        } else {
            i += 1;
        }
    }
    false
}

fn string_match(t: &[Token]) -> usize {
    let mut i = 0;
    let mut start = 0;
    let mut body = false;
    let mut found = 0;
    while i < t.len() {
        if sequence(&t[i..], &["=", ">"]) {
            found += usize::from(literal_pattern(&t[start..i]));
            body = true;
            i += 2;
        } else if !t[i].literal && matches!(t[i].text.as_str(), "(" | "[" | "{") {
            let block = sequence(&t[i..], &["{"]);
            i = group_end(t, i);
            if body && block {
                start = i;
                body = false;
            }
        } else {
            if sequence(&t[i..], &[","]) {
                start = i + 1;
                body = false;
            }
            i += 1;
        }
    }
    found
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
    // Preserve B1/B3b identities for unchanged audit and policy code moved under
    // projector; the SQL and catalog capability censuses use the physical path.
    let path = if !matches!(kind, "sql-mutation" | "catalog-writer") {
        match path {
            "crates/remanence-state/src/projector/index.rs" => {
                "crates/remanence-state/src/index.rs"
            }
            "crates/remanence-state/src/projector/tape_log_projection.rs" => {
                "crates/remanence-state/src/tape_log/projection.rs"
            }
            _ => path,
        }
    } else {
        path
    };
    let t = production_tokens(source);
    let names = audit_names(&t);
    let impls = record_impls(&t);
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
        // The B4b writer preflight split did not move an audit producer.
        let name = if kind == "audit-producer"
            && path == "crates/remanence-state/src/library_log/store.rs"
            && name == "append_checked"
        {
            "append"
        } else {
            name
        };
        let mut add = |pattern: &str| {
            if kind == "text-decision" && std::env::var_os("CENSUS_SNIPPETS").is_some() {
                println!(
                    "SNIP\t{path}::{name}::{pattern}\t{}",
                    t[i.saturating_sub(12)..(i + 15).min(t.len())]
                        .iter()
                        .map(|x| if x.literal {
                            format!("{:?}", x.text)
                        } else {
                            x.text.clone()
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            *found
                .entry(format!("{path}::{name}::{pattern}"))
                .or_insert(0) += 1;
        };
        match kind {
            "catalog-writer" => {
                assert!(
                    !sequence(rest, &["CatalogIndex", ":", ":", "open"])
                        && !sequence(rest, &["CatalogIndex", ":", ":", "open_read_only"])
                        && !sequence(rest, &["CatalogIndex", ":", ":", "open_inventory"])
                        && !sequence(rest, &["CatalogIndex", ":", ":", "open_reset_source"])
                        && !sequence(rest, &["index_write"]),
                    "forbidden catalog escape in {path}::{name}"
                );
                if sequence(rest, &["fn"]) {
                    let signature_end = rest
                        .iter()
                        .position(|t| t.text == "{")
                        .unwrap_or(rest.len());
                    let signature = &rest[..signature_end];
                    // Ignore arrows inside callback parameters: only the fn's
                    // own return type is a capability escape.
                    let mut depth = 0usize;
                    let mut parameters_end = None;
                    for (i, token) in signature.iter().enumerate() {
                        if token.text == "(" {
                            depth += 1;
                        }
                        if token.text == ")" {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                parameters_end = Some(i + 1);
                                break;
                            }
                        }
                    }
                    let signature = &signature[parameters_end.unwrap_or(signature.len())..];
                    if let Some(arrow) = signature.windows(2).position(|w| sequence(w, &["-", ">"]))
                    {
                        let returned = &signature[arrow + 2..];
                        let mutable = returned.iter().any(|t| t.text == "&")
                            && returned.iter().any(|t| t.text == "mut");
                        assert!(
                            !mutable
                                || !returned.iter().any(|t| matches!(
                                    t.text.as_str(),
                                    "CatalogIndex" | "CatalogCore"
                                )),
                            "mutable catalog accessor in {path}"
                        );
                    }
                }
                if sequence(rest, &["projector", ":", ":", "open", "("]) {
                    add("projector::open");
                }
                if sequence(rest, &[".", "write_handle", "("]) {
                    add("write_handle");
                }
                if sequence(rest, &["CatalogReader", ":", ":", "empty", "("]) {
                    add("CatalogReader::empty");
                }
            }
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
                    && !token.literal
                    && sequence(&t[i + 1..], &["{"])
                    && (names
                        .get(&token.text)
                        .is_some_and(|n| matches!(n.as_str(), "AuditEventRecord" | "AuditRecord"))
                        || (token.text == "Self" && impls.iter().any(|&(a, b)| a < i && i < b)))
                    && t.get(i.wrapping_sub(1)).is_none_or(|t| t.text != "struct")
                {
                    add(names
                        .get(&token.text)
                        .map_or("AuditEventRecord", String::as_str));
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
                if sequence(rest, &[".", "audit", "(", ")"])
                    || sequence(rest, &[":", ":", "audit", "("])
                {
                    add("audit-access");
                }
                if path == "crates/remanence-state/src/state.rs" && sequence(rest, &["fn", "audit"])
                {
                    add("public-audit-sink");
                }
                if !token.literal
                    && names.get(&token.text).is_some_and(|n| n == "AuditEvent")
                    && sequence(&t[i + 1..], &[":", ":"])
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
            "library-reserved" => {
                if !path.starts_with("crates/remanence-state/src/library_log/")
                    && !token.literal
                    && matches!(token.text.as_str(), "library_log" | "LibraryLog")
                {
                    add(&token.text);
                }
                if pending.is_none()
                    && !token.literal
                    && matches!(
                        token.text.as_str(),
                        "VerificationProgress" | "AcceptUnprotectedFinalization"
                    )
                    && sequence(&t[i + 1..], &["{"])
                    && t.get(i.wrapping_sub(1))
                        .is_none_or(|x| !matches!(x.text.as_str(), "struct" | "impl"))
                {
                    add(&format!("constructor::{}", token.text));
                }
            }
            "sense-decision" => {
                if !token.literal
                    && matches!(
                        token.text.as_str(),
                        "decode_sense"
                            | "decode_usable_sense"
                            | "decode_scsi_sense"
                            | "decode_sense_key_asc"
                            | "classify"
                            | "from_scsi"
                            | "from_sense"
                    )
                    && (t.get(i + 1).is_some_and(|x| x.text == "(")
                        || (name != "<module>"
                            && sequence(&t[i.saturating_sub(2)..i], &[":", ":"])))
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
                            | "rfind"
                            | "rsplit_once"
                            | "matches"
                            | "eq"
                            | "eq_ignore_ascii_case"
                            | "trim_start_matches"
                            | "trim_end_matches"
                            | "trim_matches"
                    )
                    && i > 0
                    && sequence(&t[i - 1..], &["."])
                {
                    let begin = receiver_start(&t, i - 1);
                    if (sequence(&t[i + 1..], &["("]) && string_value(&t[i + 2..]))
                        || t[begin..i].iter().any(|x| text_origin(x, aliases))
                    {
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
                    let legacy = operands.iter().any(|x| text_origin(x, aliases))
                        && (operands.iter().any(|x| {
                            x.literal
                                || matches!(
                                    x.text.as_str(),
                                    "message" | "to_string" | "as_str" | "format"
                                )
                        }) || (operands.iter().any(|x| x.text == "reason")
                            && sequence(rest, &["!", "="])));
                    if legacy
                        || string_value(&t[begin..i])
                        || string_value(&t[i + 2..end])
                        || (i - begin == 1 && aliases.contains(&t[begin].text))
                        || (end == i + 3 && aliases.contains(&t[i + 2].text))
                    {
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
                    if literal_pattern(&t[end..close]) {
                        add("text-matches");
                    }
                }
                if sequence(rest, &["match"]) {
                    let end = (i + 1..t.len())
                        .find(|&j| sequence(&t[j..], &["{"]))
                        .unwrap_or(t.len());
                    if end < t.len() {
                        // Only string-pattern arms, not literals inside arm bodies.
                        let close = group_end(&t, end);
                        for _ in 0..string_match(&t[end + 1..close - 1]) {
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
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .canonicalize()
        .unwrap();
    let mut paths = Vec::new();
    for entry in fs::read_dir(workspace.join("crates")).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            files(&src, &mut paths);
        }
    }
    let sources: Vec<_> = paths
        .iter()
        .map(|file| (file.clone(), fs::read_to_string(file).unwrap()))
        .collect();
    let skipped = test_files(&sources);
    let sources: Vec<_> = sources
        .into_iter()
        .filter(|(p, _)| !skipped.contains(p))
        .collect();
    let builders = sources
        .iter()
        .flat_map(|(_, source)| record_builders(&production_tokens(source)))
        .collect();
    let mut actual = BTreeMap::new();
    for (file, source) in sources {
        let path = file.strip_prefix(&workspace).unwrap().to_str().unwrap();
        actual.extend(scan_with_builders(path, &source, kind, &builders));
        if path.starts_with("crates/remanence-state/src/log_frame/")
            || path.starts_with("crates/remanence-state/src/library_log/")
        {
            let tokens = tokens(&source);
            assert!(
                !tokens.iter().any(|t| !t.literal
                    && matches!(t.text.as_str(), "set_len" | "ftruncate" | "truncate")),
                "log authority must not truncate: {path}"
            );
        }
    }
    if kind == "sql-mutation" {
        for key in actual.keys() {
            assert!(key.starts_with("crates/remanence-state/src/projector/")
                || key.starts_with("crates/remanence-chaos/src/")
                || key.starts_with("crates/remanence-state/src/authority_layout.rs::validate_layout::")
                || key.starts_with("crates/remanence-cli/src/pool_ops.rs::run_archive_export_object::mutating-sql")
                || key.starts_with("crates/remanence-cli/src/pool_ops.rs::run_archive_read::mutating-sql"),
                "SQL outside projector and named exceptions: {key}");
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
        #[cfg(all(test, unix))] mod first {{ {body} }}
        #[cfg(all(target_os = "linux", test))] mod last {{ {body} }}
        #[cfg(test)] fn generic<T>() where T: Copy, {{ {body} }}
        fn attributed_statement() {{
            #[cfg(test)] let value = if condition {{ {body} }} else if other {{ {body} }} else {{ {body} }};
        }}
        #[cfg(test)] #[path = "included.rs"] mod included;
        mod inner {{ #![cfg(test)] {body} }}
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
/// Pins literal comparison discovery for closure contains.
#[test]
fn b2_text_closure_contains() {
    let found = scan(
        "example.rs",
        r#"fn f() { a.is_some_and(|m| m.contains("busy")); }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for closure prefix.
#[test]
fn b2_text_closure_prefix() {
    let found = scan(
        "example.rs",
        r#"fn f() { a.any(|m| m.starts_with("EPERM")); }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for text parameter.
#[test]
fn b2_text_text_parameter() {
    let found = scan(
        "example.rs",
        r#"fn f(text: &str) -> bool { text.contains("is busy") }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for msg parameter.
#[test]
fn b2_text_msg_parameter() {
    let found = scan(
        "example.rs",
        r#"fn f(msg: &str) -> bool { msg.contains("is busy") }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for s parameter.
#[test]
fn b2_text_s_parameter() {
    let found = scan(
        "example.rs",
        r#"fn f(s: &str) -> bool { s.contains("is busy") }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for format binding.
#[test]
fn b2_text_format_binding() {
    let found = scan(
        "example.rs",
        r#"fn f() { let s = format!("{e}"); if s == want {} }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for format method.
#[test]
fn b2_text_format_method() {
    let found = scan(
        "example.rs",
        r#"fn f() { let s = format!("{e:#}"); if s.ends_with("not ready") {} }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for case insensitive.
#[test]
fn b2_text_case_insensitive() {
    let found = scan(
        "example.rs",
        r#"fn f() { status.message().eq_ignore_ascii_case("drive bay busy"); }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for err guard.
#[test]
fn b2_text_err_guard() {
    let found = scan(
        "example.rs",
        r#"fn f() { match run() { Err(e) if e.contains("busy") => retry(), _ => () } }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for field.
#[test]
fn b2_text_field() {
    let found = scan(
        "example.rs",
        r#"fn f() { out.stderr_text.contains("Operation not permitted"); }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins literal comparison discovery for field match.
#[test]
fn b2_text_field_match() {
    let found = scan(
        "example.rs",
        r#"fn f() { match resp.status_text.as_str() { "busy" => retry(), _ => () } }"#,
        "text-decision",
    );
    assert_eq!(found.values().sum::<usize>(), 1, "{found:?}");
}
/// Pins exclusion of all test first and resumption at the sibling.
#[test]
fn b2_test_all_test_first() {
    let source = concat!(
        r#"#[cfg(all(test, unix))] mod t { fn hidden() { c.execute("x"); } }"#,
        r#" fn live() { c.execute("x"); }"#
    );
    assert_eq!(
        scan("example.rs", source, "sql-mutation"),
        BTreeMap::from([("example.rs::live::execute".into(), 1)])
    );
}
/// Pins exclusion of all test last and resumption at the sibling.
#[test]
fn b2_test_all_test_last() {
    let source = concat!(
        r#"#[cfg(all(target_os = "linux", test))] mod t { fn hidden() { c.execute("x"); } }"#,
        r#" fn live() { c.execute("x"); }"#
    );
    assert_eq!(
        scan("example.rs", source, "sql-mutation"),
        BTreeMap::from([("example.rs::live::execute".into(), 1)])
    );
}
/// Pins exclusion of where clause and resumption at the sibling.
#[test]
fn b2_test_where_clause() {
    let source = concat!(
        r#"#[cfg(test)] fn hidden<T>() where T: Copy, { c.execute("x"); }"#,
        r#" fn live() { c.execute("x"); }"#
    );
    assert_eq!(
        scan("example.rs", source, "sql-mutation"),
        BTreeMap::from([("example.rs::live::execute".into(), 1)])
    );
}
/// Pins exclusion of let else chain and resumption at the sibling.
#[test]
fn b2_test_let_else_chain() {
    let source = concat!(
        r#"fn enclosing() { #[cfg(test)] let x = if c { c.execute("x") } else if d { c.execute("x") } else { c.execute("x") }; }"#,
        r#" fn live() { c.execute("x"); }"#
    );
    assert_eq!(
        scan("example.rs", source, "sql-mutation"),
        BTreeMap::from([("example.rs::live::execute".into(), 1)])
    );
}
/// Inner test attributes govern their module; any/not still compile in production.
#[test]
fn b2_test_predicates() {
    assert!(scan(
        "example.rs",
        "#![cfg(test)] fn hidden() { c.execute(\"x\"); }",
        "sql-mutation"
    )
    .is_empty());
    for predicate in ["any(test, unix)", "not(test)"] {
        assert_eq!(
            scan(
                "example.rs",
                &format!("#[cfg({predicate})] fn live() {{ c.execute(\"x\"); }}"),
                "sql-mutation"
            )
            .len(),
            1
        );
    }
}
/// Pins audit coverage of enum alias.
#[test]
fn b2_audit_enum_alias() {
    let found = scan(
        "example.rs",
        "use a::AuditEvent as LibraryAuditEvent; fn emit() { LibraryAuditEvent::Warning {} }",
        "audit-producer",
    );
    assert_eq!(
        found.get("example.rs::emit::AuditEvent::Warning"),
        Some(&1),
        "{found:?}"
    );
}
/// Pins audit coverage of record alias.
#[test]
fn b2_audit_record_alias() {
    let found = scan(
        "example.rs",
        "use a::AuditEventRecord as Renamed; fn emit() { Renamed {} }",
        "audit-producer",
    );
    assert_eq!(
        found.get("example.rs::emit::AuditEventRecord"),
        Some(&1),
        "{found:?}"
    );
}
/// Pins audit coverage of ufcs.
#[test]
fn b2_audit_ufcs() {
    let found = scan(
        "example.rs",
        "fn emit() { StateHandle::audit(&mut h); }",
        "audit-producer",
    );
    assert_eq!(
        found.get("example.rs::emit::audit-access"),
        Some(&1),
        "{found:?}"
    );
}
/// Pins audit coverage of self literal.
#[test]
fn b2_audit_self_literal() {
    let found = scan(
        "example.rs",
        "impl AuditEventRecord { fn factory() -> Self { Self {} } }",
        "audit-producer",
    );
    assert_eq!(
        found.get("example.rs::factory::AuditEventRecord"),
        Some(&1),
        "{found:?}"
    );
}
/// Pins audit coverage of self factory.
#[test]
fn b2_audit_self_factory() {
    let found = scan("example.rs", "use a::AuditEventRecord as Renamed; impl Renamed { fn factory() -> Self { todo!() } } fn emit() { Renamed::factory(); }", "audit-producer");
    assert_eq!(
        found.get("example.rs::emit::record-builder::factory"),
        Some(&1),
        "{found:?}"
    );
}
/// An S10 helper argument remains outside the documented comparison shapes.
#[test]
fn scanner_helper_literal_is_known_residual() {
    assert!(scan(
        "example.rs",
        r#"fn f() { has(&e.to_string(), "busy"); }"#,
        "text-decision"
    )
    .is_empty());
}

/// Beside S10: crate constants require name resolution, which this lexical scanner lacks.
/// A non-origin receiver and const argument are deliberately pinned as a blind spot;
/// imports, reexports, shadowing and associated constants prevent sound lexical substitution.
#[test]
fn scanner_const_argument_is_known_residual() {
    assert!(scan(
        "example.rs",
        r#"const BUSY: &str = "busy"; fn f(s: &str) -> bool { s.contains(BUSY) }"#,
        "text-decision"
    )
    .is_empty());
}

/// Explicit path children of skipped modules and test-named files are excluded.
#[test]
fn scanner_test_path_includes() {
    let base = Path::new("/scratch/crates/example/src");
    let sources = vec![
        (base.join("lib.rs"), r#"#[cfg(test)] mod tests { #[path = "included.rs"] mod nested; } fn live() { c.execute("x"); }"#.into()),
        (base.join("tests/included.rs"), r#"fn hidden() { c.execute("x"); }"#.into()),
        (base.join("durability_tests.rs"), r#"#[path = "acceptance.rs"] mod acceptance;"#.into()),
        (base.join("acceptance.rs"), r#"fn hidden() { c.execute("x"); }"#.into()),
        (base.join("ordinary.rs"), r#"#[cfg(test)] #[path = "custom.rs"] mod t; fn live() {}"#.into()),
        (base.join("custom.rs"), r#"fn hidden() { c.execute("x"); }"#.into()),
        (base.join("paths.rs"), r#"#[cfg(test)] mod fixtures { #[path = "../relative.rs"] mod relative; } fn live() {}"#.into()),
        (base.join("paths/relative.rs"), r#"fn hidden() { c.execute("x"); }"#.into()),
    ];
    let included_in_function = test_includes(
        r#"#[cfg(test)] fn test() { #[path = "nested.rs"] mod hidden; }"#,
        base,
        base,
        false,
    );
    assert!(included_in_function.contains(&base.join("nested.rs")));
    let skipped = test_files(&sources);
    assert!(skipped.contains(&base.join("tests/included.rs")));
    assert!(skipped.contains(&base.join("acceptance.rs")));
    assert!(skipped.contains(&base.join("custom.rs")));
    assert!(skipped.contains(&base.join("paths/relative.rs")));
    let found: BTreeMap<_, _> = sources
        .iter()
        .filter(|(p, _)| !skipped.contains(p))
        .flat_map(|(_, s)| scan("example.rs", s, "sql-mutation"))
        .collect();
    assert_eq!(
        found,
        BTreeMap::from([("example.rs::live::execute".into(), 1)])
    );
}

/// The full candidate allowlist and its dispositions must agree in both directions.
#[test]
fn text_candidate_table_matches_ratchet() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/census");
    let baseline = fs::read_to_string(directory.join("text-decision.tsv")).unwrap();
    let expected: BTreeMap<_, _> = baseline
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let (k, n) = l.split_once('\t').unwrap();
            (k.to_owned(), n.to_owned())
        })
        .collect();
    let table = fs::read_to_string(directory.join("text-candidates.tsv")).unwrap();
    let mut actual = BTreeMap::new();
    let mut counts = BTreeMap::<&str, (usize, usize)>::new();
    for line in table.lines().skip(1) {
        let f: Vec<_> = line.split('\t').collect();
        assert_eq!(f.len(), 6, "{line}");
        assert!(
            matches!(
                f[4],
                "decision" | "enum-as-text" | "cli-argument" | "path" | "magic-or-unit" | "other"
            ),
            "{line}"
        );
        assert!(!f[5].trim().is_empty(), "unclassified: {line}");
        assert!(actual
            .insert(format!("{}::{}::{}", f[0], f[1], f[2]), f[3].to_owned())
            .is_none());
        let entry = counts.entry(f[4]).or_default();
        entry.0 += 1;
        entry.1 += f[3].parse::<usize>().unwrap();
    }
    println!("text labels (keys, occurrences): {counts:?}");
    assert_eq!(actual, expected);
    for (function, count) in [
        ("handle_append_finish_inner", 4),
        ("handle_abort", 1),
        ("print_setcap_hint_if_error_text_matches", 2),
    ] {
        let rows: Vec<_> = table
            .lines()
            .skip(1)
            .map(|l| l.split('\t').collect::<Vec<_>>())
            .filter(|f| f[1] == function)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][3].parse::<usize>().unwrap(), count);
        assert_eq!(rows[0][4], "decision");
        assert!(rows[0][5].contains("new"));
    }
}

/// Direct format operands, nested literal patterns and arm counts are shape based.
#[test]
fn scanner_literal_operand_and_arm_shapes() {
    let source = r#"fn f() {
        if format!("{e}") == want {}
        if want != format!("{e}") {}
        match s { Some("a") => (), Some("b") => (), _ => () }
        match e { #[cfg(target_os = "linux")] Err(e) => log!("busy"), _ => () }
        value.eq("x"); value.rfind("x"); value.rsplit_once("x");
        value.matches("x"); value.trim_start_matches("x");
        value.trim_end_matches("x"); value.trim_matches("x");
    }"#;
    let found = scan("example.rs", source, "text-decision");
    assert_eq!(found["example.rs::f::text-equality"], 2);
    assert_eq!(found["example.rs::f::text-match"], 2);
    assert_eq!(found.values().sum::<usize>(), 11);
}

/// Lists every production use of the library authority so new consumers are added deliberately.
#[test]
fn library_reserved_census() {
    census("library-reserved");
}

/// Reserved constructors remain visible even inside the future log module.
#[test]
fn scanner_library_reserved_shapes() {
    for word in ["library_log", "LibraryLog"] {
        let found = scan(
            "outside.rs",
            &format!("fn f() {{ use state::{word}; }}"),
            "library-reserved",
        );
        assert_eq!(found[&format!("outside.rs::f::{word}")], 1);
        assert!(scan(
            "crates/remanence-state/src/library_log/store.rs",
            &format!("fn f() {{ {word}; }}"),
            "library-reserved"
        )
        .is_empty());
    }
    for name in ["VerificationProgress", "AcceptUnprotectedFinalization"] {
        let path = "crates/remanence-state/src/library_log/wire.rs";
        let found = scan(
            path,
            &format!("fn decode() {{ {name} {{}} }}"),
            "library-reserved",
        );
        assert_eq!(found[&format!("{path}::decode::constructor::{name}")], 1);
        assert!(scan(
            path,
            &format!("pub struct {name} {{ a: u64 }}"),
            "library-reserved"
        )
        .is_empty());
        // Fixture feature code is production-scanned and must be explicitly allowed.
        let found = scan(
            path,
            &format!(
                "#[cfg(any(test, feature = \"fixtures\"))] fn fixture_value() {{ {name} {{}} }}"
            ),
            "library-reserved",
        );
        assert_eq!(
            found[&format!("{path}::fixture_value::constructor::{name}")],
            1
        );
    }
}

/// Inherent reader implementations are not reserved record constructors.
#[test]
fn reserved_reader_impl_is_not_a_constructor() {
    assert!(scan(
        "crates/remanence-state/src/library_log/mod.rs",
        "impl VerificationProgress { pub fn parts_done(&self)->u32 { self.parts_done } }",
        "library-reserved"
    )
    .is_empty());
}
/// Every typed destination in the producer census has an allocated library record.
#[test]
fn destinations_in_producer_table_are_records() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(root.join("src/library_log/mod.rs")).unwrap();
    let start = source.find("pub enum Tag {").unwrap();
    let body = source[start..].split_once('}').unwrap().0;
    let tags: BTreeSet<_> = tokens(body)
        .into_iter()
        .filter(|t| !t.literal)
        .map(|t| t.text)
        .collect();
    let table = fs::read_to_string(root.join("tests/census/library-producers.tsv")).unwrap();
    for row in table.lines().skip(1) {
        let destination = row.split('\t').nth(4).unwrap();
        for word in
            destination.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';'))
        {
            if word.len() > 1
                && word.chars().all(|c| c.is_ascii_alphabetic())
                && word.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && !matches!(word, "DriveFault" | "LibraryAuthority")
            {
                assert!(tags.contains(word), "unallocated destination {word}: {row}");
            }
        }
    }
}

/// Both reserved bodies are counted by the production scanner and restricted to reader/fixture sites.
#[test]
fn verification_progress_has_no_production_constructor() {
    census("library-reserved");
}

/// Error classification and refusal guards must remain in B6's decision floor.
#[test]
fn b2_review_error_text_labels_are_decisions() {
    let table = include_str!("census/text-candidates.tsv");
    for (path, function, pattern) in [
        (
            "crates/remanence-state/src/authority/resume.rs",
            "from",
            "text-equality",
        ),
        (
            "crates/remanence-api/src/readiness_wait.rs",
            "start_readiness_wait_with_index",
            "text-equality",
        ),
        (
            "crates/remanence-api/src/write_owner/robotics.rs",
            "finish_library_operation",
            "text-contains",
        ),
        (
            "crates/remanence-api/src/startup_checkpoint.rs",
            "replay_checkpoint_journal_projections_with_audit",
            "text-equality",
        ),
        (
            "crates/remanence-parity/src/terminal_inventory/one_pass.rs",
            "verify_terminal_index_full_with_route",
            "text-equality",
        ),
        (
            "crates/remanence-cli/src/standalone.rs",
            "dispatch",
            "text-equality",
        ),
    ] {
        let row = table
            .lines()
            .map(|line| line.split('\t').collect::<Vec<_>>())
            .find(|row| row.len() == 6 && row[..3] == [path, function, pattern])
            .unwrap();
        assert_eq!(row[4], "decision", "{path}::{function}::{pattern}");
    }
}

/// The pool-write classification includes its stored committed-state comparisons.
#[test]
fn pool_write_replay_reason_accounts_for_committed_state() {
    let row = include_str!("census/text-candidates.tsv").lines()
        .find(|line| line.starts_with("crates/remanence-api/src/pool_write/no_parity.rs\tmaybe_replay_pool_write\ttext-equality\t"))
        .expect("pool-write census row");
    assert!(row.contains("enum-as-text"));
    assert!(row.contains("copy.status == \"committed\""));
}

/// All production sense decisions stay within the decoder and completion evidence producers.
#[test]
fn b3b_sense_decision_census() {
    census("sense-decision");
}

/// Finishing-site checks include indirect normalizer references, while excluding test modules.
#[test]
fn b3b_finishing_site_scanner_detects_consumer_reclassification() {
    let source = r#"
        fn renderer(error: &Error) {
            let outcome = classify(input);
            let evidence = CompletionEvidence::from_scsi(error);
            optional.map(CompletionEvidence::from_sense);
            decode_usable_sense(bytes);
        }
        #[cfg(test)] mod tests { fn fixture() { classify(input); } }
    "#;
    let found = scan("crates/example/src/lib.rs", source, "sense-decision");
    assert_eq!(found.len(), 4, "{found:?}");
    for name in ["classify", "from_scsi", "from_sense", "decode_usable_sense"] {
        assert_eq!(
            found.get(&format!("crates/example/src/lib.rs::renderer::{name}")),
            Some(&1)
        );
    }
}

/// Writer creation sites are reviewed and obsolete writable escapes are absent.
#[test]
fn b4a_catalog_writer_census() {
    census("catalog-writer");
}

/// Capability scanning uses the shared tokenizer and distinguishes mutable receivers from mutable returns.
#[test]
fn b4a_catalog_writer_scanner() {
    let source = r#"
        // CatalogIndex::open("comment");
        fn issue() { let c = projector::open(p); c.write_handle(); CatalogReader::empty(); }
        fn catalog_index(&mut self) -> &CatalogIndex { &self.index }
        #[cfg(test)] fn fixture() { CatalogIndex::open(p); }
    "#;
    let found = scan("example.rs", source, "catalog-writer");
    assert_eq!(found.len(), 3);
    assert_eq!(found["example.rs::issue::projector::open"], 1);
    assert_eq!(found["example.rs::issue::write_handle"], 1);
    assert_eq!(found["example.rs::issue::CatalogReader::empty"], 1);
    for forbidden in [
        "fn leak() { CatalogIndex::open(p); }",
        "fn leak() { c.index_write(); }",
        "fn catalog_index(&mut self) -> &mut CatalogIndex { &mut self.index }",
        "fn other(&mut self) -> &mut CatalogIndex { &mut self.index }",
        "fn core(&mut self) -> &mut CatalogCore { &mut self.core }",
        "fn other(&mut self) -> &mut crate::projector::CatalogIndex { &mut self.index }",
        "fn other<'a>(&'a mut self) -> &'a mut CatalogIndex { &mut self.index }",
    ] {
        assert!(
            std::panic::catch_unwind(|| scan("example.rs", forbidden, "catalog-writer")).is_err()
        );
    }
}
