//! Egg configuration file rewriting — a faithful port of wings
//! `parser/parser.go` + `parser/helpers.go`.
//!
//! The panel ships a list of configuration files per egg
//! (`process_configuration.configs`), each with a parser type and a list
//! of find/replace rules. Before a server boots, every rule is applied to
//! the file on disk so game-specific settings (ports, IPs, passwords...)
//! always match the allocation and egg variables managed by the panel.
//!
//! Values may reference daemon configuration with `{{ config.docker.interface }}`
//! templating. Like wings since v1.12.3, only a strictly limited subset of
//! the daemon configuration is exposed to egg templating: the docker
//! network interface (both `docker.interface` and `docker.network.interface`).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use regex::Regex;
use serde_json::{Map, Value};

use crate::error::{AppError, AppResult};
use crate::models::configuration::{PatternConfig, PatternReplace, ReplaceValue};

/// Refuse to parse (or allocate for) configuration files larger than this.
/// Every parser buffers the whole file in memory and the contents are
/// untrusted server-owned input, so this guards the daemon against being
/// OOM'd by an oversized config (wings `maxConfigFileSize`).
const MAX_CONFIG_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// The limited daemon configuration exposed to `{{ config.* }}` templating.
/// Wings restricts this to the docker network interface.
pub struct TemplatableConfig {
    /// `docker.network.interface`
    pub interface: String,
}

impl TemplatableConfig {
    pub fn from_daemon(interface: &str) -> Self {
        Self { interface: interface.to_string() }
    }

    /// JSON view used for `{{ config.<path> }}` lookups. Paths are
    /// snake_cased per segment by the caller, so `docker.network.interface`
    /// becomes `docker.network.interface` here.
    fn to_json(&self) -> Value {
        serde_json::json!({
            "docker": {
                "interface": self.interface,
                "network": { "interface": self.interface },
            }
        })
    }
}

// wings `configMatchRegex`: `{{\s?config\.([\w.-]+)\s?}}`
fn config_match_regex() -> Regex {
    Regex::new(r"\{\{\s?config\.([\w.-]+)\s?\}\}").expect("valid regex")
}

// wings `xmlValueMatchRegex`: `^\[([\w]+)='(.*)'\]$`
fn xml_value_match_regex() -> Regex {
    Regex::new(r"^\[([\w]+)='(.*)'\]$").expect("valid regex")
}

// wings `checkForArrayElement`: `^([^\[\]]+)\[([\d]+)](\..+)?$`
fn array_element_regex() -> Regex {
    Regex::new(r"^([^\[\]]+)\[([\d]+)](\..+)?$").expect("valid regex")
}

/// Convert a single identifier to snake_case (wings uses
/// strcase.ToSnake on every path segment). "Network" -> "network",
/// "MyKey" -> "my_key".
fn to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower = i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_ascii_lowercase();
            if i > 0 && (prev_lower || (chars[i - 1].is_ascii_uppercase() && next_lower)) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Rewrite the file at `path` according to the egg rule. Errors are
/// returned to the caller, which logs and continues with the next file
/// (wings behavior: a broken config file never prevents a boot).
pub fn apply(path: &Path, file: &PatternConfig, daemon: &TemplatableConfig) -> AppResult<()> {
    let mut handle = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("parser: cannot open {}: {e}", path.display())))?;

    let meta = handle
        .metadata()
        .map_err(|e| AppError::Io(std::io::Error::from(e)))?;
    if meta.len() > MAX_CONFIG_FILE_SIZE {
        return Err(AppError::BadRequest(format!(
            "parser: refusing to parse configuration file {}: size {} exceeds limit of {MAX_CONFIG_FILE_SIZE} bytes",
            path.display(),
            meta.len()
        )));
    }

    let parser = file.parser.as_str();
    match parser {
        "properties" => parse_properties_file(&mut handle, file, daemon),
        "file" => parse_text_file(&mut handle, file, daemon),
        "yaml" | "yml" => parse_yaml_file(&mut handle, file, daemon),
        "json" => parse_json_file(&mut handle, file, daemon),
        "ini" => parse_ini_file(&mut handle, file, daemon),
        "xml" => parse_xml_file(&mut handle, file, daemon),
        other => {
            tracing::warn!(parser = other, file = %path.display(), "unknown configuration file parser, skipping");
            Ok(())
        }
    }
}

/// Truncate the file and write the new content (wings Seek+Truncate+Write).
fn rewrite(file: &mut std::fs::File, data: &[u8]) -> AppResult<()> {
    file.seek(SeekFrom::Start(0))
        .map_err(AppError::Io)?;
    file.set_len(0).map_err(AppError::Io)?;
    file.write_all(data).map_err(AppError::Io)?;
    file.flush().map_err(AppError::Io)
}

/// Read the whole file (bounded by MAX_CONFIG_FILE_SIZE).
fn read_all(file: &mut std::fs::File) -> AppResult<Vec<u8>> {
    let mut buf = Vec::new();
    file.take(MAX_CONFIG_FILE_SIZE)
        .read_to_end(&mut buf)
        .map_err(AppError::Io)?;
    Ok(buf)
}

/// wings `LookupConfigurationValue`: resolve `{{ config.* }}` templates in
/// the replacement value against the (restricted) daemon configuration.
/// Non-string values and missing keys are returned untouched.
fn lookup_configuration_value(replace: &PatternReplace, daemon: &TemplatableConfig) -> AppResult<String> {
    let raw = match &replace.replace_with {
        ReplaceValue::Str(s) => s.clone(),
        _ => return Ok(replace.replace_with.as_string()),
    };

    let re = config_match_regex();
    if !re.is_match(&raw) {
        return Ok(replace.replace_with.as_string());
    }

    let hunt = re
        .captures(&raw)
        .map(|c| c[1].to_string())
        .unwrap_or_default();

    let path: Vec<String> = hunt.split('.').map(to_snake).collect();

    let json = daemon.to_json();
    let mut current = &json;
    for segment in &path {
        match current.get(segment) {
            Some(v) => current = v,
            // Key does not exist in the daemon config: keep the original
            // value intact so the misconfiguration is obvious to the user.
            None => {
                tracing::debug!(path = ?path, "attempted to load a configuration value that does not exist");
                return Ok(raw);
            }
        }
    }

    // Only substitute scalar values, never whole objects or arrays.
    match current {
        Value::Object(_) | Value::Array(_) => Ok(raw),
        scalar => {
            let value = match scalar {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Ok(re.replace_all(&raw, value.as_str()).into_owned())
        }
    }
}

/// Compute the replacement string for a rule (after template resolution)
/// and the typed JSON value used by the json/yaml parsers.
fn resolved(replace: &PatternReplace, daemon: &TemplatableConfig) -> AppResult<(String, Value)> {
    let s = lookup_configuration_value(replace, daemon)?;
    // wings getKeyValue: the typed value is derived from the *resolved
    // string*: booleans from boolean-typed values, integers parsed from the
    // string representation, everything else stays a string.
    let typed = match &replace.replace_with {
        ReplaceValue::Bool(b) => Value::Bool(*b),
        _ => {
            if let Ok(i) = s.parse::<i64>() {
                Value::Number(i.into())
            } else {
                Value::String(s.clone())
            }
        }
    };
    Ok((s, typed))
}

// ---------------------------------------------------------------------------
// json + yaml iteration (wings IterateOverJson / setValueAtPath)
// ---------------------------------------------------------------------------

/// Apply all replacements to an unstructured JSON/YAML document. Supports
/// wildcard children (`servers.*.port`) and explicit array indexes
/// (`something[1].nested`). Bug-for-bug port of wings `SetAtPathway`
/// including its IfValue comparison quirks.
fn iterate_over_json(root: &mut Value, file: &PatternConfig, daemon: &TemplatableConfig) -> AppResult<()> {
    for replace in &file.replace {
        let (value, typed) = resolved(replace, daemon)?;

        // Wildcard: split on the first ".*" and apply the remaining path to
        // every child below the prefix. Children are mutated in place (gabs
        // hands out live containers, not copies).
        if let Some(pos) = replace.match_.find(".*") {
            let prefix = replace.match_[..pos].trim_matches('.');
            let suffix = replace.match_[pos + 2..].trim_matches('.');

            match get_at_path_mut(root, prefix) {
                Some(Value::Array(items)) => {
                    for child in items.iter_mut() {
                        if child.is_null() {
                            continue;
                        }
                        if let Err(e) = set_value_at_path(child, suffix, typed.clone()) {
                            tracing::warn!(error = %e, "failed to set config value of array child");
                        }
                    }
                }
                Some(Value::Object(map)) => {
                    for child in map.values_mut() {
                        if child.is_null() {
                            continue;
                        }
                        if let Err(e) = set_value_at_path(child, suffix, typed.clone()) {
                            tracing::warn!(error = %e, "failed to set config value of array child");
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        if let Err(e) = set_replacement_at_path(root, replace, &replace.match_, value.clone(), typed.clone()) {
            return Err(AppError::BadRequest(format!(
                "unable to set config value at pathway: {} ({e})",
                replace.match_
            )));
        }
    }
    Ok(())
}

/// Read the value at a dot-separated path (read-only helper for wildcards).
fn get_at_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    if path.is_empty() {
        return Some(current);
    }
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// wings `SetAtPathway`: honors `if_value` (exact or `regex:`) before
/// delegating to `set_value_at_path`. The regex form substitutes into the
/// *string* value (wings passes the string, not the typed value); the
/// plain form uses the typed value.
fn set_replacement_at_path(
    root: &mut Value,
    replace: &PatternReplace,
    path: &str,
    value_str: String,
    value: Value,
) -> AppResult<()> {
    if replace.if_value.is_empty() {
        return set_value_at_path(root, path, value);
    }

    // Regex replacement requires an existing value.
    if let Some(pattern) = replace.if_value.strip_prefix("regex:") {
        let current = match get_at_path(root, path) {
            Some(v) => v.clone(),
            None => return Ok(()),
        };
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(if_value = pattern, error = %e, "configuration if_value using invalid regexp, cannot perform replacement");
                return Ok(());
            }
        };
        let current_str = match &current {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if re.is_match(&current_str) {
            let replaced = re.replace_all(&current_str, value_str.as_str()).into_owned();
            return set_value_at_path(root, path, Value::String(replaced));
        }
        return Ok(());
    }

    // Exact-match IfValue. NOTE: this replicates wings exactly, including
    // comparing against the whole document (`c.Bytes()`), not the value at
    // the path. Keeping this identical is required for panel parity.
    if get_at_path(root, path).is_some() {
        let doc = serde_json::to_string(root).unwrap_or_default();
        if doc != replace.if_value {
            return Ok(());
        }
    }

    set_value_at_path(root, path, value)
}

/// wings `setValueAtPath`: sets a value at a dot path, handling explicit
/// array indexes (`something[1]` / `something[0].nested`).
fn set_value_at_path(root: &mut Value, path: &str, value: Value) -> AppResult<()> {
    let captures = match array_element_regex().captures(path) {
        Some(c) => c,
        None => return set_p(root, path, value),
    };

    let base = captures[1].to_string();
    let index: usize = captures[2].parse().unwrap_or(0);
    let trailing = captures.get(3).map(|m| m.as_str().to_string());

    // Resolve (or create) the array at `base`. A missing, null, or empty
    // array is materialized when index == 0 (wings creates the array with
    // an object element when a trailing path follows); anything else at
    // the base path, or a non-zero index into a missing array, is an error.
    let needs_create = match get_at_path_mut(root, &base) {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => items.is_empty(),
        Some(_) => {
            return Err(AppError::BadRequest(format!(
                "error while parsing array element at path: {path}"
            )))
        }
    };
    if needs_create {
        if index != 0 {
            return Err(AppError::BadRequest(format!(
                "error while parsing array element at path: {path}"
            )));
        }
        let first = if trailing.is_some() {
            Value::Object(Map::new())
        } else {
            Value::Null
        };
        // wings: SetP replaces whatever was there (including an empty array).
        set_p(root, &base, Value::Array(vec![first]))?;
    }

    let array = get_at_path_mut(root, &base).expect("array materialized above");

    // Additional indexes require the element to already exist; only a
    // fresh element 0 can be appended implicitly.
    if index >= array.as_array().map(|a| a.len()).unwrap_or(0) {
        if index != 0 {
            return Err(AppError::BadRequest(format!(
                "failed to find array element at path: {path}"
            )));
        }
        array
            .as_array_mut()
            .expect("is array")
            .push(Value::Null);
    }

    let element = array
        .as_array_mut()
        .expect("is array")
        .get_mut(index)
        .expect("index checked");

    match &trailing {
        Some(t) => {
            let t = t.trim_start_matches('.').to_string();
            set_p(element, &t, value)
        }
        None => {
            *element = value;
            Ok(())
        }
    }
}

/// gabs `SetP`: create intermediate objects along the path, then set.
fn set_p(target: &mut Value, path: &str, value: Value) -> AppResult<()> {
    if path.is_empty() {
        *target = value;
        return Ok(());
    }
    let segments: Vec<&str> = path.split('.').collect();
    let mut current = target;
    for (i, segment) in segments.iter().enumerate() {
        if i == segments.len() - 1 {
            match current {
                Value::Object(map) => {
                    map.insert((*segment).to_string(), value);
                    return Ok(());
                }
                Value::Array(items) => {
                    let idx: usize = (*segment)
                        .parse()
                        .map_err(|_| AppError::BadRequest(format!("invalid array index in path: {path}")))?;
                    if idx >= items.len() {
                        return Err(AppError::BadRequest(format!(
                            "array index {idx} out of bounds while setting path: {path}"
                        )));
                    }
                    items[idx] = value;
                    return Ok(());
                }
                // Overwrite a scalar on the way down? gabs errors; we do too.
                _ => {
                    return Err(AppError::BadRequest(format!(
                        "cannot set path through scalar value: {path}"
                    )))
                }
            }
        }
        current = match current {
            Value::Object(map) => {
                let entry = map
                    .entry((*segment).to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !entry.is_object() && !entry.is_array() {
                    *entry = Value::Object(Map::new());
                }
                entry
            }
            Value::Array(items) => {
                let idx: usize = (*segment)
                    .parse()
                    .map_err(|_| AppError::BadRequest(format!("invalid array index in path: {path}")))?;
                if idx >= items.len() {
                    return Err(AppError::BadRequest(format!(
                        "array index {idx} out of bounds while setting path: {path}"
                    )));
                }
                &mut items[idx]
            }
            _ => {
                return Err(AppError::BadRequest(format!(
                    "cannot set path through scalar value: {path}"
                )))
            }
        };
    }
    Ok(())
}

/// Mutable lookup for a dot path (arrays + objects only).
fn get_at_path_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut current = root;
    if path.is_empty() {
        return Some(current);
    }
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get_mut(segment)?,
            Value::Array(items) => items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

// ---------------------------------------------------------------------------
// per-format parsers
// ---------------------------------------------------------------------------

/// json: parse, iterate, write back with 4-space indent (wings parity).
fn parse_json_file(
    file: &mut std::fs::File,
    config: &PatternConfig,
    daemon: &TemplatableConfig,
) -> AppResult<()> {
    let raw = read_all(file)?;
    let mut data: Value = serde_json::from_slice(&raw)
        .map_err(|e| AppError::BadRequest(format!("parser: invalid json config: {e}")))?;

    iterate_over_json(&mut data, config, daemon)?;

    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(&data, &mut ser)
        .map_err(|e| AppError::BadRequest(format!("parser: cannot serialize json: {e}")))?;
    rewrite(file, &out)
}

/// yaml: unmarshal into JSON, iterate, re-marshal as YAML (wings converts
/// the YAML to JSON via dyno, applies the same iteration, then yaml.Marshal).
fn parse_yaml_file(
    file: &mut std::fs::File,
    config: &PatternConfig,
    daemon: &TemplatableConfig,
) -> AppResult<()> {
    let raw = read_all(file)?;
    let data: Value = serde_yaml::from_slice(&raw)
        .map_err(|e| AppError::BadRequest(format!("parser: invalid yaml config: {e}")))?;

    let mut data = data;
    iterate_over_json(&mut data, config, daemon)?;

    let out = serde_yaml::to_string(&data)
        .map_err(|e| AppError::BadRequest(format!("parser: cannot serialize yaml: {e}")))?;
    rewrite(file, out.as_bytes())
}

/// file: plain-text line-prefix replacement. A line whose start matches a
/// rule is replaced by the rule's raw value; the last matching rule wins
/// (wings loops all rules per line).
fn parse_text_file(
    file: &mut std::fs::File,
    config: &PatternConfig,
    daemon: &TemplatableConfig,
) -> AppResult<()> {
    let raw = read_all(file)?;

    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let segments: Vec<&[u8]> = raw.split(|&b| b == b'\n').collect();
    let last = segments.len().saturating_sub(1);

    for (i, line) in segments.iter().enumerate() {
        let is_last = i == last;
        // A trailing empty segment is the artifact of a final newline in
        // the source; the previous iteration already emitted it.
        if is_last && line.is_empty() {
            break;
        }
        let line_str = String::from_utf8_lossy(line);
        // Trim a trailing \r (CRLF files).
        let trimmed = line_str.strip_suffix('\r').unwrap_or(&line_str);

        let mut replacement: Option<String> = None;
        for rule in &config.replace {
            if !trimmed.starts_with(&rule.match_) {
                continue;
            }
            replacement = Some(lookup_configuration_value(rule, daemon)?);
        }

        match replacement {
            Some(value) => out.extend_from_slice(value.as_bytes()),
            None => out.extend_from_slice(line),
        }
        if !is_last {
            out.push(b'\n');
        }
    }

    rewrite(file, &out)
}

/// properties: hand-rolled to match wings byte-for-byte where practical:
/// the leading comment block is preserved, all keys are re-emitted in load
/// order as `key=value`, and values are ASCII-escaped (wings
/// `strconv.QuoteToASCII` semantics; see the wings docblock about UTF-8).
fn parse_properties_file(
    file: &mut std::fs::File,
    config: &PatternConfig,
    daemon: &TemplatableConfig,
) -> AppResult<()> {
    let raw = read_all(file)?;
    let text = String::from_utf8_lossy(&raw).into_owned();

    // 1. Preserve the leading comment block (wings scans until the first
    //    non-comment line with content).
    let mut header = String::new();
    {
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let t = line.trim_start();
            if t.is_empty() || t.starts_with('#') {
                header.push_str(line);
                header.push('\n');
                continue;
            }
            break;
        }
    }

    // 2. Parse into insertion-ordered key/value pairs.
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut pending: Option<(String, String)> = None; // (key, partial value) across line continuations
    for line in text.lines() {
        if let Some((key, mut value)) = pending.take() {
            // Continuation line: if it ends with a single backslash, keep
            // consuming; otherwise this is the last chunk.
            let escaped = value.ends_with('\\') && !value.ends_with("\\\\");
            if escaped {
                value.pop();
                pending = Some((key, value));
                continue;
            }
            pending = None;
            // The final chunk was already appended; fall through.
            let _ = &mut value;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
            continue;
        }

        // Find the separator: first unescaped '=', ':' or whitespace.
        let bytes = line.as_bytes();
        let mut sep: Option<usize> = None;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 1, // skip escaped char
                b'=' | b':' => {
                    sep = Some(i);
                    break;
                }
                b' ' | b'\t' | b'\x0c' => {
                    sep = Some(i);
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        let (key_raw, value_raw) = match sep {
            Some(pos) => (&line[..pos], line[pos..].to_string()),
            None => (line, String::new()),
        };
        let key = unescape_properties(key_raw.trim());
        // Value: strip the separator char and leading whitespace.
        let mut value_part = value_raw.as_str();
        if !value_part.is_empty() {
            let first = value_part.chars().next().unwrap();
            if first == '=' || first == ':' {
                value_part = &value_part[1..];
            }
        }
        let value_part = value_part.trim_start();
        let mut value = unescape_properties(value_part);

        if value.ends_with('\\') && !value.ends_with("\\\\") {
            value.pop();
            pending = Some((key, value));
            continue;
        }
        entries.push((key, value));
    }
    // Flush a dangling continuation (file ended with a backslash).
    if let Some((key, value)) = pending.take() {
        entries.push((key, value));
    }

    // 3. Apply replacements (wings properties semantics: IfValue compares
    //    against the current value of the key).
    for replace in &config.replace {
        let value = lookup_configuration_value(replace, daemon)?;
        let current = entries.iter().find(|(k, _)| *k == replace.match_).map(|(_, v)| v.clone());
        if !replace.if_value.is_empty() {
            match &current {
                // Key missing or value mismatch: skip.
                Some(v) if *v == replace.if_value => {}
                _ => continue,
            }
        }
        match entries.iter_mut().find(|(k, _)| *k == replace.match_) {
            Some(entry) => entry.1 = value,
            None => entries.push((replace.match_.clone(), value)),
        }
    }

    // 4. Emit: header + `key=<ascii-escaped value>` lines.
    let mut out = String::from(&header);
    for (key, value) in &entries {
        out.push_str(key);
        out.push('=');
        out.push_str(&quote_to_ascii(value));
        out.push('\n');
    }

    rewrite(file, out.as_bytes())
}

/// Go `strconv.QuoteToASCII` minus the surrounding quotes: escape all
/// non-ASCII runes and control characters as `\uXXXX` (or `\UXXXXXXXX`
/// beyond the BMP, which Go uses for runes > 0xFFFF... Go actually uses
/// `\u` for < 0x10000 and `\U` for larger;QuoteToASCII on a string uses
/// \u/\U respectively).
fn quote_to_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x80 && !c.is_control() => out.push(c),
            c => {
                let cp = c as u32;
                if cp < 0x10000 {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    out.push_str(&format!("\\U{:08x}", cp));
                }
            }
        }
    }
    out
}

/// Unescape a properties token (standard java properties unescaping,
/// including \uXXXX).
fn unescape_properties(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('f') => out.push('\x0c'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch);
                    }
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// ini: hand-rolled parser with wings' bracket-aware dot path splitting.
/// `section.key` addresses key `key` in the `[section]` block; dots inside
/// brackets or beyond the first split are kept verbatim in the key.
fn parse_ini_file(
    file: &mut std::fs::File,
    config: &PatternConfig,
    daemon: &TemplatableConfig,
) -> AppResult<()> {
    let raw = read_all(file)?;
    let text = String::from_utf8_lossy(&raw).into_owned();

    // Parse into ordered sections: (section name, Vec<(key, value)>).
    // Comments and formatting are dropped (wings' ini.Load + WriteTo also
    // re-emits a canonical form).
    let mut sections: Vec<(String, Vec<(String, String)>)> = vec![(String::new(), Vec::new())];
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            sections.push((trimmed[1..trimmed.len() - 1].to_string(), Vec::new()));
            continue;
        }
        if let Some(pos) = trimmed.find('=') {
            let key = trimmed[..pos].trim().to_string();
            let value = trimmed[pos + 1..].trim().to_string();
            sections.last_mut().expect("always present").1.push((key, value));
        }
    }

    let section_index = |sections: &mut Vec<(String, Vec<(String, String)>)>, name: &str| -> usize {
        if let Some(idx) = sections.iter().position(|(s, _)| s == name) {
            return idx;
        }
        sections.push((name.to_string(), Vec::new()));
        sections.len() - 1
    };

    for replace in &config.replace {
        // Bracket-aware path split (wings walks runes tracking depth).
        let mut path: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut bracket_depth = 0i32;
        for c in replace.match_.chars() {
            match c {
                '[' => {
                    bracket_depth += 1;
                    current.push(c);
                }
                ']' => {
                    bracket_depth -= 1;
                    current.push(c);
                }
                '.' => {
                    if bracket_depth > 0 || path.len() == 1 {
                        current.push(c);
                    } else {
                        path.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(c),
            }
        }
        path.push(current);

        let value = lookup_configuration_value(replace, daemon)?;

        let (section, key) = if path.len() == 2 {
            (path[0].clone(), path[1].clone())
        } else {
            (String::new(), path.join("."))
        };

        let idx = section_index(&mut sections, &section);
        let entries = &mut sections[idx].1;
        match entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => entries.push((key, value)),
        }
    }

    // Emit canonical form: default section first (without header), then
    // named sections.
    let mut out = String::new();
    for (i, (name, entries)) in sections.iter().enumerate() {
        if entries.is_empty() {
            continue;
        }
        if i > 0 || !name.is_empty() {
            out.push('[');
            out.push_str(name);
            out.push_str("]\n");
        }
        for (key, value) in entries {
            out.push_str(key);
            out.push('=');
            out.push_str(value);
            out.push('\n');
        }
        out.push('\n');
    }

    rewrite(file, out.as_bytes())
}

/// xml: parse into a tree, create missing elements along each rule's path,
/// then set text content or attributes. Paths use dots; `*` matches any
/// child element name. `Root.Prop='[name="value"]'`-style rules (wings
/// `xmlValueMatchRegex`) set attributes instead of text.
fn parse_xml_file(
    file: &mut std::fs::File,
    config: &PatternConfig,
    daemon: &TemplatableConfig,
) -> AppResult<()> {
    let raw = read_all(file)?;
    let had_prolog = raw.starts_with(b"<?xml");
    let mut cursor = std::io::Cursor::new(raw);
    let mut root: Option<xmltree::Element> = match xmltree::Element::parse(&mut cursor) {
        Ok(el) => Some(el),
        // Empty or unparsable file: start from scratch like wings creates
        // a root when the document has none.
        Err(e) => {
            if config.replace.is_empty() {
                return Err(AppError::BadRequest(format!("parser: invalid xml config: {e}")));
            }
            None
        }
    };

    let value_regex = xml_value_match_regex();

    let mut created_root = false;
    if root.is_none() {
        // wings: the first replacement's first segment becomes the root.
        let first = config
            .replace
            .first()
            .map(|r| r.match_.split('.').next().unwrap_or("").to_string())
            .unwrap_or_default();
        root = Some(xmltree::Element {
            prefix: None,
            namespace: None,
            namespaces: None,
            name: first,
            attributes: Default::default(),
            children: Vec::new(),
        });
        created_root = true;
    }
    let mut root = root.expect("root created above");

    for replace in config.replace.iter() {
        let value = lookup_configuration_value(replace, daemon)?;

        let segments: Vec<&str> = replace.match_.split('.').collect();
        if segments.is_empty() {
            continue;
        }

        let has_wildcard = segments.iter().any(|s| *s == "*");
        if !has_wildcard {
            // Create the missing structure starting at the root (segments
            // after the root element). Each step re-borrows from the root
            // via the collected index path to keep the borrow checker happy.
            let mut idx_path: Vec<usize> = Vec::new();
            for tag in &segments[1..] {
                let element = element_at_mut(&mut root, &idx_path);
                let found = element
                    .children
                    .iter()
                    .position(|c| matches!(c, xmltree::XMLNode::Element(e) if &e.name == tag));
                let idx = match found {
                    Some(idx) => idx,
                    None => {
                        element.children.push(xmltree::XMLNode::Element(xmltree::Element {
                            prefix: None,
                            namespace: None,
                            namespaces: None,
                            name: (*tag).to_string(),
                            attributes: Default::default(),
                            children: Vec::new(),
                        }));
                        element.children.len() - 1
                    }
                };
                idx_path.push(idx);
            }
        }

        // Resolve the path to every matching element as a list of index
        // paths (collected immutably), then mutate each target in turn.
        let mut index_paths: Vec<Vec<usize>> = Vec::new();
        collect_index_paths(&root, &segments[1..], &mut Vec::new(), &mut index_paths);

        for path in index_paths {
            let element = element_at_mut(&mut root, &path);
            if let Some(caps) = value_regex.captures(&value) {
                // Attribute form: `[name='value']`.
                element
                    .attributes
                    .insert(caps[1].to_string(), caps[2].to_string());
            } else {
                // Text form: replace content.
                element.children = vec![xmltree::XMLNode::Text(value.clone())];
            }
        }
    }

    // Serialize with 2-space indentation (wings doc.Indent(2)).
    let mut out = Vec::new();
    if had_prolog || created_root {
        out.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    }
    write_element(&mut out, &root, 0)?;
    rewrite(file, &out)
}

/// Descend into the element tree following a list of child indexes.
fn element_at_mut<'a>(root: &'a mut xmltree::Element, path: &[usize]) -> &'a mut xmltree::Element {
    let mut current = root;
    for &idx in path {
        current = match &mut current.children[idx] {
            xmltree::XMLNode::Element(e) => e,
            _ => unreachable!("index path only ever points at elements"),
        };
    }
    current
}

/// Collect the index path of every element matching the segment sequence
/// (segments collected immutably so mutation can happen afterwards).
fn collect_index_paths(
    element: &xmltree::Element,
    segments: &[&str],
    prefix: &mut Vec<usize>,
    out: &mut Vec<Vec<usize>>,
) {
    if segments.is_empty() {
        out.push(prefix.clone());
        return;
    }
    let segment = segments[0];
    for (idx, child) in element.children.iter().enumerate() {
        if let xmltree::XMLNode::Element(e) = child {
            if segment == "*" || e.name == segment {
                prefix.push(idx);
                collect_index_paths(e, &segments[1..], prefix, out);
                prefix.pop();
            }
        }
    }
}

/// Serialize an XML element tree with 2-space indentation.
fn write_element(out: &mut Vec<u8>, element: &xmltree::Element, depth: usize) -> AppResult<()> {
    let indent = "  ".repeat(depth);
    out.extend_from_slice(indent.as_bytes());
    out.push(b'<');
    out.extend_from_slice(element.name.as_bytes());
    for (k, v) in &element.attributes {
        out.extend_from_slice(format!(" {k}=\"{}\"", escape_xml_attr(v)).into_bytes().as_slice());
    }
    let has_element_children = element
        .children
        .iter()
        .any(|c| matches!(c, xmltree::XMLNode::Element(_)));
    let text = element.get_text().map(|t| t.into_owned()).unwrap_or_default();

    if element.children.is_empty() {
        out.extend_from_slice(b"/>\n");
        return Ok(());
    }

    out.push(b'>');
    if !has_element_children {
        // Compact: <name>text</name>
        out.extend_from_slice(escape_xml_text(&text).as_bytes());
        out.extend_from_slice(format!("</{}>\n", element.name).into_bytes().as_slice());
        return Ok(());
    }
    out.push(b'\n');
    for child in &element.children {
        match child {
            xmltree::XMLNode::Element(e) => write_element(out, e, depth + 1)?,
            xmltree::XMLNode::Text(t) if !t.trim().is_empty() => {
                out.extend_from_slice("  ".repeat(depth + 1).as_bytes());
                out.extend_from_slice(escape_xml_text(t).as_bytes());
                out.push(b'\n');
            }
            xmltree::XMLNode::Comment(c) => {
                out.extend_from_slice("  ".repeat(depth + 1).as_bytes());
                out.extend_from_slice(format!("<!--{c}-->\n").into_bytes().as_slice());
            }
            _ => {}
        }
    }
    out.extend_from_slice(indent.as_bytes());
    out.extend_from_slice(format!("</{}>\n", element.name).into_bytes().as_slice());
    Ok(())
}

fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_xml_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::configuration::{PatternConfig, PatternReplace};

    fn rule(match_: &str, replace_with: &str) -> PatternReplace {
        PatternReplace {
            match_: match_.into(),
            replace_with: ReplaceValue::Str(replace_with.into()),
            if_value: String::new(),
        }
    }

    fn config(parser: &str, replace: Vec<PatternReplace>) -> PatternConfig {
        PatternConfig {
            file: "test".into(),
            parser: parser.into(),
            replace,
        }
    }

    fn apply_str(parser: &str, content: &str, rules: Vec<PatternReplace>) -> String {
        let dir = std::env::temp_dir().join(format!("roost-parser-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(&path, content).unwrap();
        let daemon = TemplatableConfig { interface: "172.18.0.1".into() };
        apply(&path, &config(parser, rules), &daemon).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn text_file_replaces_prefix() {
        let out = apply_str("file", "motd=A Server\nonline-mode=true\n", vec![rule("motd=", "Hello World")]);
        assert_eq!(out, "Hello World\nonline-mode=true\n");
    }

    #[test]
    fn json_sets_nested_and_types() {
        // wings getKeyValue: "true" given as a JSON string stays a string
        // (only boolean-typed values become booleans); "25565" becomes a
        // number because it parses as an integer.
        let rules = vec![rule("network.port", "25565"), rule("online-mode", "true")];
        let out = apply_str("json", r#"{"network":{"port":1},"online-mode":false}"#, rules);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["network"]["port"], serde_json::json!(25565));
        assert_eq!(v["online-mode"], serde_json::json!("true"));

        // Boolean-typed replacement values stay booleans.
        let rules = vec![PatternReplace {
            match_: "online-mode".into(),
            replace_with: ReplaceValue::Bool(true),
            if_value: String::new(),
        }];
        let out = apply_str("json", r#"{"online-mode":false}"#, rules);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["online-mode"], serde_json::json!(true));
    }

    #[test]
    fn json_wildcard() {
        let rules = vec![rule("servers.*.port", "25565")];
        let out = apply_str(
            "json",
            r#"{"servers":{"a":{"port":1},"b":{"port":2}}}"#,
            rules,
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["servers"]["a"]["port"], serde_json::json!(25565));
        assert_eq!(v["servers"]["b"]["port"], serde_json::json!(25565));
    }

    #[test]
    fn json_array_index() {
        let rules = vec![rule("things[0].name", "first")];
        let out = apply_str("json", r#"{"things":[]}"#, rules);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["things"][0]["name"], serde_json::json!("first"));
    }

    #[test]
    fn yaml_roundtrip() {
        let rules = vec![rule("a.b.c", "42")];
        let out = apply_str("yaml", "a:\n  b:\n    c: 1\nother: keep\n", rules);
        assert!(out.contains("c: 42"), "yaml output: {out}");
        assert!(out.contains("other: keep"));
    }

    #[test]
    fn config_template_lookup() {
        let rules = vec![rule("bind", "{{config.docker.interface}}")];
        let out = apply_str("json", r#"{"bind":"x"}"#, rules);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["bind"], serde_json::json!("172.18.0.1"));
    }

    #[test]
    fn properties_preserves_header_and_escapes() {
        let out = apply_str(
            "properties",
            "# my config\nmotd=A §B\nkey=val\n",
            vec![rule("key", "hello\u{00A7}world")],
        );
        assert!(out.starts_with("# my config\n"), "out: {out}");
        assert!(out.contains("key=hello\\u00a7world"), "out: {out}");
        // motd untouched but re-emitted in insertion order with escaping.
        assert!(out.contains("motd=A \\u00a7B"), "out: {out}");
    }

    #[test]
    fn ini_section_and_key() {
        let rules = vec![rule("server.port", "25565"), rule("server.bind", "0.0.0.0")];
        let out = apply_str("ini", "[server]\nport=1\n", rules);
        assert!(out.contains("[server]"));
        assert!(out.contains("port=25565"));
        assert!(out.contains("bind=0.0.0.0"));
    }

    #[test]
    fn xml_sets_text_and_creates_path() {
        let rules = vec![rule("config.host.name", "myhost")];
        let out = apply_str("xml", "<config></config>", rules);
        assert!(out.contains("<name>myhost</name>"), "out: {out}");
    }

    #[test]
    fn if_value_regex() {
        let mut r = rule("port", "10");
        r.if_value = "regex:^[0-9]{4}$".into();
        // The regex anchors both ends, so the whole value is replaced
        // (wings r.ReplaceAllString semantics).
        let out = apply_str("json", r#"{"port":8080}"#, vec![r]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["port"], serde_json::json!("10"));
    }
}
