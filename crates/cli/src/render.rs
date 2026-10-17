//! Renderings of one result value: compact lines, or filtered JSON. Both read
//! the same wire value, so neither carries anything the other lacks.

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use vuoxi_rpc::types::*;

/// Keys that survive `--fields`: the envelope and the paging cursor.
const KEEP: [&str; 4] = ["kind", "schema", "next_cursor", "head"];

fn pick(o: &Map<String, Value>, fields: &[String], keep_envelope: bool) -> Map<String, Value> {
    o.iter()
        .filter(|(k, _)| fields.contains(k) || keep_envelope && KEEP.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Restrict a result to `fields`: applied to each item of a list, else to the
/// object itself. The envelope and cursor are always kept.
pub fn filter(v: &Value, fields: &[String]) -> Value {
    let Some(o) = v.as_object() else {
        return v.clone();
    };
    let mut out = pick(o, fields, true);
    if let Some(Value::Array(items)) = o.get("items") {
        let items = items
            .iter()
            .map(|i| {
                i.as_object()
                    .map_or(i.clone(), |i| Value::Object(pick(i, fields, false)))
            })
            .collect();
        out.insert("items".into(), Value::Array(items));
    }
    Value::Object(out)
}

pub fn bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let (mut x, mut i) = (n as f64, 0);
    while x >= 1024.0 && i < U.len() - 1 {
        x /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{x:.1}{}", U[i])
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn kv_line(o: &Map<String, Value>) -> String {
    let parts: Vec<String> = o
        .iter()
        .map(|(k, v)| format!("{k}={}", scalar(v)))
        .collect();
    parts.join(" ")
}

fn next_line(v: &Value) -> Option<String> {
    v.get("next_cursor")?.as_u64().map(|n| format!("next={n}"))
}

/// Text for a `--fields` selection: `k=v` pairs, one line per item.
fn fields_text(v: &Value, fields: &[String]) -> String {
    let f = filter(v, fields);
    let mut lines = Vec::new();
    match f.get("items").and_then(Value::as_array) {
        Some(items) => {
            lines.extend(items.iter().filter_map(Value::as_object).map(kv_line));
            lines.extend(next_line(&f));
        }
        None => {
            if let Some(o) = f.as_object() {
                let o: Map<_, _> = o
                    .iter()
                    .filter(|(k, _)| fields.contains(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                lines.push(kv_line(&o));
            }
        }
    }
    lines.join("\n")
}

fn typed<T: DeserializeOwned>(v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("unexpected result shape: {e}"))
}

fn list(lines: Vec<String>, next: Option<u64>) -> String {
    let mut lines = lines;
    if lines.is_empty() {
        lines.push("(none)".into());
    }
    lines.extend(next.map(|n| format!("next={n}")));
    lines.join("\n")
}

/// The compact text form, one line per item.
pub fn text(v: &Value, fields: Option<&[String]>) -> Result<String, String> {
    if let Some(f) = fields {
        return Ok(fields_text(v, f));
    }
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
    Ok(match kind {
        "status" => {
            let s: Status = typed(v)?;
            format!(
                "head={} folders={} files={} bytes={} conflicts={} peers={} mode={}",
                s.head,
                s.folders,
                s.files,
                bytes(s.bytes),
                s.conflicts,
                s.peers,
                s.mode
            )
        }
        "changes" => {
            let c: Changes = typed(v)?;
            let lines = c
                .items
                .iter()
                .map(|i| {
                    format!(
                        "#{} {} {} {} {}",
                        i.seq,
                        i.op,
                        i.path,
                        bytes(i.bytes),
                        i.peer.as_deref().unwrap_or("-")
                    )
                })
                .collect();
            list(lines, c.next_cursor)
        }
        "conflicts" => {
            let c: Conflicts = typed(v)?;
            let lines = c
                .items
                .iter()
                .map(|i| {
                    format!(
                        "#{} {} ours={} theirs={} peer={}",
                        i.seq, i.path, i.ours, i.theirs, i.peer
                    )
                })
                .collect();
            list(lines, c.next_cursor)
        }
        "plan" => {
            let p: Plan = typed(v)?;
            format!("{} {} {}", p.id, p.state, effect(&p.effect))
        }
        "applied" => {
            let a: Applied = typed(v)?;
            format!("applied {} {} head={}", a.plan, effect(&a.effect), a.head)
        }
        "reverted" => {
            let r: Reverted = typed(v)?;
            format!("reverted {} head={}", r.plan, r.head)
        }
        other => return Err(format!("unknown result kind: {other:?}")),
    })
}

fn effect(e: &Effect) -> String {
    format!(
        "files={} bytes={} peers={}",
        e.files,
        bytes(e.bytes),
        e.peers
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bytes_are_short() {
        assert_eq!(bytes(0), "0B");
        assert_eq!(bytes(4096), "4.0K");
        assert_eq!(bytes(1_288_490_188), "1.2G");
    }

    #[test]
    fn filter_keeps_envelope_and_cursor() {
        let v = json!({"kind":"changes","schema":1,"head":9,"next_cursor":3,
            "items":[{"seq":1,"op":"create","path":"p"}]});
        let f = filter(&v, &["seq".to_string()]);
        assert_eq!(
            f,
            json!({"kind":"changes","schema":1,"head":9,"next_cursor":3,"items":[{"seq":1}]})
        );
    }

    #[test]
    fn fields_text_is_key_value_lines() {
        let v = json!({"kind":"changes","schema":1,"next_cursor":2,
            "items":[{"seq":1,"op":"create"},{"seq":2,"op":"delete"}]});
        let t = text(&v, Some(&["seq".to_string(), "op".to_string()])).unwrap();
        assert_eq!(t, "op=create seq=1\nop=delete seq=2\nnext=2");
    }
}
