//! `lattice lsl check|fmt|compat`: work with schema files without a running server.

use lattice_lsl::check::{Compiled, check};
use lattice_lsl::compat::compare;
use lattice_lsl::format::format;
use lattice_lsl::parser::parse;

/// Parses and checks a schema. Errors are `line:col: message`, one per line.
fn compile(src: &str) -> Result<Compiled, String> {
    let schema = parse(src).map_err(|d| format!("{}:{}: {}", d.line, d.col, d.message))?;
    check(&schema).map_err(|ds| {
        ds.iter()
            .map(|d| format!("{}:{}: {}", d.line, d.col, d.message))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// A one-line summary of a valid schema.
pub(crate) fn check_text(src: &str) -> Result<String, String> {
    let c = compile(src)?;
    Ok(format!(
        "ok: {} types, {} enums\n",
        c.types.len(),
        c.enums.len()
    ))
}

/// The canonical text of a schema.
pub(crate) fn format_text(src: &str) -> Result<String, String> {
    parse(src)
        .map(|s| format(&s))
        .map_err(|d| format!("{}:{}: {}", d.line, d.col, d.message))
}

/// The compatibility verdict of changing `old` into `new`, and whether it is safe for old data.
pub(crate) fn compat_text(old: &str, new: &str) -> Result<(String, bool), String> {
    let verdict = compare(
        &compile(old).map_err(|e| format!("old schema: {e}"))?,
        &compile(new).map_err(|e| format!("new schema: {e}"))?,
    );
    let mut out = format!(
        "backward compatible (old data stays valid): {}\nforward compatible (old readers accept new data): {}\n",
        verdict.backward, verdict.forward
    );
    for b in &verdict.backward_breaks {
        out.push_str(&format!("  backward: {b}\n"));
    }
    for b in &verdict.forward_breaks {
        out.push_str(&format!("  forward: {b}\n"));
    }
    Ok((out, verdict.backward))
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = "type Ticket {\n  title: text\n}\n";
    const V2: &str = "type Ticket {\n  title: text\n  owner: text\n}\n";

    #[test]
    fn check_accepts_a_valid_schema_and_locates_errors() {
        assert!(check_text(V1).unwrap().starts_with("ok: 1 types"));
        let e = check_text("type Ticket {\n  title: nope\n}\n").unwrap_err();
        assert!(e.contains(": "), "{e}");
    }

    #[test]
    fn format_is_stable() {
        let once = format_text(V2).unwrap();
        assert_eq!(format_text(&once).unwrap(), once);
    }

    #[test]
    fn a_new_required_field_breaks_backward_compatibility() {
        let (text, safe) = compat_text(V1, V2).unwrap();
        assert!(!safe && text.contains("owner"), "{text}");
        assert!(compat_text(V1, V1).unwrap().1);
    }
}
