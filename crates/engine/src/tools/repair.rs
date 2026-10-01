//! Tolerant repair of tool-call arguments.
//!
//! Why this is not optional: our primary target is a *quantized* local model
//! (iq3_s). These models emit malformed tool JSON routinely, and a strict parser
//! turns "the model wanted to run `ls`" into a hard turn failure. PLAN.md §6
//! pairs this with exactly one retry — repair first, and only re-ask the model
//! when repair cannot save it.
//!
//! Deliberate limits:
//!   * repair is **conservative**. We never invent values. A truncated string is
//!     closed, not completed.
//!   * every transformation is recorded in `notes`, so the UI can show *what* we
//!     had to fix. Silent repair of a security-relevant argument (an `exec` cmd)
//!     would be worse than failing.
//!   * if the original parsed fine, `repaired == false` and we touch nothing.

use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct RepairOutcome {
    pub value: Value,
    /// true if any transformation was applied
    pub repaired: bool,
    /// human-readable record of each transformation applied
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum RepairError {
    /// Nothing we tried produced valid JSON.
    Unrepairable { input: String, notes: Vec<String> },
}

impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepairError::Unrepairable { input, notes } => {
                write!(f, "could not repair tool arguments: {input:?}")?;
                if !notes.is_empty() {
                    write!(f, " (tried: {})", notes.join("; "))?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RepairError {}

/// Parse `src` strictly; if that fails, repair and retry.
pub fn parse_or_repair(src: &str) -> Result<RepairOutcome, RepairError> {
    let trimmed = src.trim();

    // Fast path: already valid. Never touch bytes that already parse.
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(RepairOutcome { value: v, repaired: false, notes: Vec::new() });
    }

    let mut notes: Vec<String> = Vec::new();
    let mut work = trimmed.to_string();

    // 1. markdown fences — models love wrapping arguments in ```json
    if let Some(unfenced) = strip_fences(&work) {
        if unfenced != work {
            work = unfenced;
            notes.push("stripped markdown fences".into());
        }
    }

    // 2. isolate the outermost object/array, dropping prose around it
    if let Some(ex) = extract_outer_container(&work) {
        if ex != work {
            work = ex;
            notes.push("dropped surrounding prose".into());
        }
    }

    // strict again, cheap
    if let Ok(v) = serde_json::from_str::<Value>(&work) {
        return Ok(RepairOutcome { value: v, repaired: !notes.is_empty(), notes });
    }

    // 3. single-quoted strings -> double-quoted
    let (q, changed_q) = normalize_quotes(&work);
    if changed_q {
        work = q;
        notes.push("converted single-quoted strings".into());
    }

    // 4. bare keys: {cmd: "..."} -> {"cmd": "..."}
    let (k, changed_k) = quote_bare_keys(&work);
    if changed_k {
        work = k;
        notes.push("quoted bare keys".into());
    }

    // 5. python literals
    let (p, changed_p) = normalize_literals(&work);
    if changed_p {
        work = p;
        notes.push("normalized True/False/None".into());
    }

    // 6. raw control characters inside strings (a literal newline in a value)
    let (e, changed_e) = escape_raw_control_in_strings(&work);
    if changed_e {
        work = e;
        notes.push("escaped raw control chars in strings".into());
    }

    // 7. trailing commas before a closer
    let (c, changed_c) = drop_trailing_commas(&work);
    if changed_c {
        work = c;
        notes.push("dropped trailing commas".into());
    }

    // 8. close anything left open (truncation)
    let (b, changed_b) = balance_delimiters(&work);
    if changed_b {
        work = b;
        notes.push("closed unterminated strings/brackets".into());
    }

    match serde_json::from_str::<Value>(&work) {
        Ok(v) => Ok(RepairOutcome { value: v, repaired: true, notes }),
        Err(_) => Err(RepairError::Unrepairable { input: src.to_string(), notes }),
    }
}

/// Coerce an object's field to a string, tolerating the model sending a number
/// or a nested object where a string was expected.
pub fn field_as_string(v: &Value, key: &str) -> Option<String> {
    let f = v.get(key)?;
    match f {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

// ---- transformations -------------------------------------------------------

fn strip_fences(s: &str) -> Option<String> {
    let t = s.trim();
    if !t.starts_with("```") {
        return None;
    }
    let mut lines: Vec<&str> = t.lines().collect();
    if lines.first().is_some_and(|l| l.trim_start().starts_with("```")) {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.trim() == "```") {
        lines.pop();
    }
    Some(lines.join("\n").trim().to_string())
}

/// Return the substring from the first '{' or '[' through its matching closer.
/// If never closed, take everything from the opener onward.
fn extract_outer_container(s: &str) -> Option<String> {
    let bytes: Vec<char> = s.chars().collect();
    let start = bytes.iter().position(|&c| c == '{' || c == '[')?;
    let opener = bytes[start];
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(bytes[start..=i].iter().collect());
                }
            }
            _ => {}
        }
        let _ = opener;
    }
    Some(bytes[start..].iter().collect())
}

/// Rewrite 'single quoted' strings as "double quoted", escaping any embedded ".
fn normalize_quotes(s: &str) -> (String, bool) {
    let mut out = String::with_capacity(s.len() + 8);
    let mut changed = false;
    let mut chars = s.chars().peekable();
    // We only treat a quote as a string delimiter when it is in a "value
    // position", i.e. preceded by JSON punctuation. That is what stops an
    // apostrophe inside an existing double-quoted string from opening one.
    let mut prev_sig: Option<char> = None;

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push('"');
                // copy the double-quoted string verbatim
                while let Some(&n) = chars.peek() {
                    chars.next();
                    out.push(n);
                    if n == '\\' {
                        if let Some(&n2) = chars.peek() {
                            chars.next();
                            out.push(n2);
                        }
                    } else if n == '"' {
                        break;
                    }
                }
                prev_sig = Some('"');
            }
            '\'' if matches!(prev_sig, None | Some('{') | Some(',') | Some(':') | Some('[')) => {
                changed = true;
                out.push('"');
                while let Some(&n) = chars.peek() {
                    chars.next();
                    match n {
                        // the closing delimiter must become a closing double
                        // quote, not just disappear
                        '\'' => {
                            out.push('"');
                            break;
                        }
                        '"' => out.push_str("\\\""),
                        '\\' => {
                            out.push('\\');
                            if let Some(&n2) = chars.peek() {
                                chars.next();
                                out.push(n2);
                            }
                        }
                        other => out.push(other),
                    }
                }
                prev_sig = Some('\'');
            }
            _ => {
                out.push(c);
                if !c.is_whitespace() {
                    prev_sig = Some(c);
                }
            }
        }
    }
    (out, changed)
}

/// {cmd: "x"} -> {"cmd": "x"}
fn quote_bare_keys(s: &str) -> (String, bool) {
    let mut out = String::with_capacity(s.len() + 8);
    let mut changed = false;
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut in_str = false;
    let mut esc = false;

    while i < b.len() {
        let c = b[i] as char;
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            // read an identifier
            let start = i;
            while i < b.len() && (b[i] as char).is_ascii_alphanumeric() || i < b.len() && b[i] == b'_' {
                i += 1;
            }
            let ident = &s[start..i];
            // is it a key? skip whitespace, look for ':'
            let mut j = i;
            while j < b.len() && (b[j] as char).is_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b':' {
                out.push('"');
                out.push_str(ident);
                out.push('"');
                changed = true;
            } else {
                out.push_str(ident);
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    (out, changed)
}

/// Python-flavoured literals outside strings.
fn normalize_literals(s: &str) -> (String, bool) {
    let mut out = String::with_capacity(s.len());
    let mut changed = false;
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut in_str = false;
    let mut esc = false;

    while i < b.len() {
        let c = b[i] as char;
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        // Match a literal only when it is not part of a longer identifier.
        let prev_ok = i == 0 || {
            let p = b[i - 1] as char;
            !(p.is_ascii_alphanumeric() || p == '_')
        };
        if prev_ok {
            let rest = &s[i..];
            let (lit, repl) = [
                ("true", None),
                ("false", None),
                ("null", None),
                ("True", Some("true")),
                ("False", Some("false")),
                ("None", Some("null")),
            ]
            .into_iter()
            .find(|(lit, _)| {
                rest.starts_with(lit)
                    && rest[lit.len()..]
                        .chars()
                        .next()
                        .map_or(true, |n| !(n.is_ascii_alphanumeric() || n == '_'))
            })
            .map(|(lit, repl)| (lit, repl))
            .unwrap_or(("", None));

            if !lit.is_empty() {
                if let Some(r) = repl {
                    out.push_str(r);
                    changed = true;
                } else {
                    out.push_str(lit);
                }
                i += lit.len();
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    (out, changed)
}

/// A literal newline inside a JSON string is invalid; turn it into \n.
fn escape_raw_control_in_strings(s: &str) -> (String, bool) {
    let mut out = String::with_capacity(s.len() + 8);
    let mut changed = false;
    let mut in_str = false;
    let mut esc = false;
    for c in s.chars() {
        if in_str {
            if esc {
                esc = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => {
                    esc = true;
                    out.push(c);
                }
                '"' => {
                    in_str = false;
                    out.push(c);
                }
                '\n' => {
                    out.push_str("\\n");
                    changed = true;
                }
                '\r' => {
                    out.push_str("\\r");
                    changed = true;
                }
                '\t' => {
                    out.push_str("\\t");
                    changed = true;
                }
                other => out.push(other),
            }
        } else {
            if c == '"' {
                in_str = true;
            }
            out.push(c);
        }
    }
    (out, changed)
}

/// Remove a comma that is followed (modulo whitespace) by a closer.
fn drop_trailing_commas(s: &str) -> (String, bool) {
    let mut out = String::with_capacity(s.len());
    let mut changed = false;
    let chars: Vec<char> = s.chars().collect();
    let mut in_str = false;
    let mut esc = false;

    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            continue;
        }
        if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                changed = true;
                continue;
            }
        }
        out.push(c);
    }
    (out, changed)
}

/// Close anything left open. Conservative: we close, we never invent content.
fn balance_delimiters(s: &str) -> (String, bool) {
    let mut stack: Vec<char> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    let mut out = String::with_capacity(s.len() + 8);

    for c in s.chars() {
        if in_str {
            if esc {
                esc = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => {
                    esc = true;
                    out.push(c);
                }
                '"' => {
                    in_str = false;
                    out.push(c);
                }
                other => out.push(other),
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                stack.push(c);
                out.push(c);
            }
            '}' | ']' => {
                stack.pop();
                out.push(c);
            }
            _ => out.push(c),
        }
    }

    let mut changed = false;
    if in_str {
        out.push('"');
        changed = true;
    }
    while let Some(open) = stack.pop() {
        out.push(if open == '{' { '}' } else { ']' });
        changed = true;
    }
    (out, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(s: &str) -> Value {
        parse_or_repair(s).expect("should repair").value
    }

    #[test]
    fn valid_json_is_untouched_and_unflagged() {
        let o = parse_or_repair(r#"{"cmd":"ls -la"}"#).unwrap();
        assert!(!o.repaired);
        assert!(o.notes.is_empty());
        assert_eq!(o.value["cmd"], "ls -la");
    }

    #[test]
    fn trailing_comma() {
        let v = fix(r#"{"cmd":"ls","path":"/tmp",}"#);
        assert_eq!(v["cmd"], "ls");
        assert_eq!(v["path"], "/tmp");
    }

    #[test]
    fn single_quotes() {
        let v = fix(r"{'cmd': 'echo hi'}");
        assert_eq!(v["cmd"], "echo hi");
    }

    #[test]
    fn bare_keys() {
        let v = fix(r#"{cmd: "ls"}"#);
        assert_eq!(v["cmd"], "ls");
    }

    #[test]
    fn python_literals() {
        let v = fix(r#"{"cmd":"ls","background":True,"tag":None}"#);
        assert_eq!(v["background"], true);
        assert!(v["tag"].is_null());
    }

    #[test]
    fn markdown_fenced() {
        let v = fix("```json\n{\"cmd\":\"ls\"}\n```");
        assert_eq!(v["cmd"], "ls");
    }

    #[test]
    fn prose_around_the_object() {
        let v = fix("Sure! Here you go: {\"cmd\":\"ls\"} Hope that helps.");
        assert_eq!(v["cmd"], "ls");
    }

    #[test]
    fn truncated_is_closed_not_invented() {
        let v = fix(r#"{"cmd":"ls -la"#);
        assert_eq!(v["cmd"], "ls -la");
    }

    #[test]
    fn raw_newline_inside_a_string() {
        let v = fix("{\"cmd\":\"line1\nline2\"}");
        assert_eq!(v["cmd"], "line1\nline2");
    }

    #[test]
    fn nested_arrays_with_trailing_comma() {
        let v = fix(r#"{"paths":["a","b",],"n":1,}"#);
        assert_eq!(v["paths"].as_array().unwrap().len(), 2);
    }

    /// The compound case a small model actually produces.
    #[test]
    fn compound_realistic_failure() {
        let raw = "```json\n{ cmd: 'git status', 'amend': True, }\n```";
        let o = parse_or_repair(raw).unwrap();
        assert!(o.repaired);
        assert!(!o.notes.is_empty());
        assert_eq!(o.value["cmd"], "git status");
        assert_eq!(o.value["amend"], true);
    }

    #[test]
    fn repair_is_recorded_not_silent() {
        let o = parse_or_repair(r#"{"cmd":"ls",}"#).unwrap();
        assert!(o.repaired);
        assert!(o.notes.iter().any(|n| n.contains("trailing comma")));
    }

    #[test]
    fn garbage_fails_loudly() {
        let e = parse_or_repair("the model just refused to emit json at all");
        assert!(matches!(e, Err(RepairError::Unrepairable { .. })));
    }

    #[test]
    fn apostrophe_in_a_double_quoted_value_survives() {
        let v = fix(r#"{"cmd":"it's fine"}"#);
        assert_eq!(v["cmd"], "it's fine");
    }

    #[test]
    fn field_coercion_handles_number_where_string_expected() {
        let v = fix(r#"{"limit":42}"#);
        assert_eq!(field_as_string(&v, "limit").as_deref(), Some("42"));
    }

    #[test]
    fn escaped_quotes_survive_balancing() {
        let v = fix(r#"{"cmd":"echo \"hi\"""#);
        assert_eq!(v["cmd"], "echo \"hi\"");
    }
}

