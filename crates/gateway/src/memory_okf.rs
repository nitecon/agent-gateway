//! Optional OKF exchange: JSON data only; documents are never parsed or fetched.
use serde_json::Value;

pub const CAPABILITY: &str = "okf-markdown-v1";
pub const MAX_ENVELOPE_BYTES: usize = 1_048_576;
pub const MAX_DOCUMENT_BYTES: usize = 524_288;
pub const MAX_DEPTH: usize = 16;
pub const MAX_NODES: usize = 16_384;

pub fn capabilities() -> Value {
    serde_json::json!({
        "version": 1,
        "capabilities": [{
            "id": CAPABILITY, "version": 1, "format": "okf-markdown",
            "request_header": "X-Memory-Capabilities",
            "max_envelope_bytes": MAX_ENVELOPE_BYTES,
            "max_document_bytes": MAX_DOCUMENT_BYTES,
            "max_depth": MAX_DEPTH, "max_nodes": MAX_NODES
        }]
    })
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn validate(concept_hash: Option<&str>, okf: Option<&Value>) -> Result<(), String> {
    if concept_hash.is_some_and(|hash| !valid_hash(hash)) {
        return Err("concept_hash must be a lowercase SHA-256 hex digest".into());
    }
    let Some(okf) = okf else { return Ok(()) };
    let object = okf.as_object().ok_or("okf must be an object")?;
    // Check bounded traversal before serialization, including unknown extensions.
    let mut pending = vec![(okf, 1)];
    let mut nodes = 0;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if depth > MAX_DEPTH || nodes > MAX_NODES {
            return Err("okf exceeds nesting or node limit".into());
        }
        match value {
            Value::Array(values) => pending.extend(values.iter().map(|v| (v, depth + 1))),
            Value::Object(values) => {
                if values.keys().any(|key| key.contains('\0')) {
                    return Err("okf keys must not contain NUL".into());
                }
                pending.extend(values.values().map(|v| (v, depth + 1)));
            }
            Value::String(text) if text.contains('\0') => {
                return Err("okf strings must not contain NUL".into())
            }
            _ => {}
        }
    }
    if serde_json::to_vec(okf).map_err(|e| e.to_string())?.len() > MAX_ENVELOPE_BYTES {
        return Err("okf exceeds envelope byte limit".into());
    }
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported okf version; expected 1".into());
    }
    if object.get("format").and_then(Value::as_str) != Some("okf-markdown") {
        return Err("unsupported okf format; expected okf-markdown".into());
    }
    if object
        .get("revision")
        .and_then(Value::as_u64)
        .is_none_or(|n| n == 0)
    {
        return Err("okf revision must be a positive integer".into());
    }
    let hash = object
        .get("semantic_hash")
        .and_then(Value::as_str)
        .filter(|hash| valid_hash(hash))
        .ok_or("okf semantic_hash must be a lowercase SHA-256 hex digest")?;
    if concept_hash.is_some_and(|concept| concept != hash) {
        return Err("concept_hash does not match okf.semantic_hash".into());
    }
    let document = object
        .get("document")
        .and_then(Value::as_str)
        .ok_or("okf document must be a string")?;
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err("okf document exceeds byte limit".into());
    }
    Ok(())
}
