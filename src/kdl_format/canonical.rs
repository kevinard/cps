//! The canonical KDL text CPS writes for documents whose bytes are signed or
//! digested, such as package manifests.
//!
//! The `kdl` crate's formatter is not a specified format: a release may change
//! quoting, spacing, or number spelling. This writer is the specification of
//! the canonical form, so the same document produces the same bytes with any
//! CPS build:
//!
//! - Nodes are written one per line, indented four spaces per level, and every
//!   line ends with `\n`, including the last.
//! - A node is its name, then its arguments, then its properties, each
//!   preceded by one space and in document order. A node with children ends
//!   its line with ` {`, and a line with the parent's indentation and `}`
//!   closes the block. A node without children has no block.
//! - Node names and property keys are written bare when they are an ASCII
//!   letter followed by ASCII letters, digits, `-`, `_`, or `.`; otherwise
//!   they are quoted strings.
//! - String values are always quoted. `\` and `"` are escaped, as are
//!   newline, carriage return, tab, backspace, and form feed (`\n`, `\r`,
//!   `\t`, `\b`, `\f`). Every other code point that KDL 2.0 does not allow
//!   literally, or that is a control or bidirectional formatting character, is
//!   written as `\u{X}` with lowercase hexadecimal digits. Everything else is
//!   written as itself.
//! - Integers are decimal, with a leading `-` when negative. Booleans are
//!   `#true` and `#false`, and null is `#null`.
//! - Type annotations and comments are never written.

use anyhow::{Result, bail, ensure};
use kdl::{KdlDocument, KdlNode, KdlValue};

/// Write `doc` in canonical form.
pub(crate) fn write(doc: &KdlDocument) -> Result<String> {
    let mut out = String::new();
    nodes(&mut out, doc, 0)?;
    Ok(out)
}

fn nodes(out: &mut String, doc: &KdlDocument, depth: usize) -> Result<()> {
    for node in doc.nodes() {
        self::node(out, node, depth)?;
    }
    Ok(())
}

fn node(out: &mut String, node: &KdlNode, depth: usize) -> Result<()> {
    ensure!(node.ty().is_none(), "canonical KDL has no type annotations");
    indent(out, depth);
    identifier(out, node.name().value());
    let (arguments, properties): (Vec<_>, Vec<_>) = node
        .entries()
        .iter()
        .partition(|entry| entry.name().is_none());
    for entry in arguments.into_iter().chain(properties) {
        ensure!(
            entry.ty().is_none(),
            "canonical KDL has no type annotations"
        );
        out.push(' ');
        if let Some(name) = entry.name() {
            identifier(out, name.value());
            out.push('=');
        }
        value(out, entry.value())?;
    }
    match node
        .children()
        .filter(|children| !children.nodes().is_empty())
    {
        Some(children) => {
            out.push_str(" {\n");
            nodes(out, children, depth + 1)?;
            indent(out, depth);
            out.push_str("}\n");
        }
        None => out.push('\n'),
    }
    Ok(())
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

fn identifier(out: &mut String, name: &str) {
    let mut chars = name.chars();
    let bare = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if bare {
        out.push_str(name);
    } else {
        string(out, name);
    }
}

fn value(out: &mut String, value: &KdlValue) -> Result<()> {
    match value {
        KdlValue::String(text) => string(out, text),
        KdlValue::Integer(number) => out.push_str(&number.to_string()),
        KdlValue::Bool(true) => out.push_str("#true"),
        KdlValue::Bool(false) => out.push_str("#false"),
        KdlValue::Null => out.push_str("#null"),
        KdlValue::Float(_) => bail!("canonical KDL has no floating-point values"),
    }
    Ok(())
}

fn string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if escaped(c) => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Code points written as `\u{X}`: controls, the KDL 2.0 newline and
/// disallowed code points, and bidirectional formatting characters.
fn escaped(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_specified_bytes() -> Result<()> {
        let doc = super::super::document(concat!(
            "// comments are dropped\n",
            "schema-version 1 kind=package-manifest\n",
            "package \"a/b\" publisher=canopa  version=\"1.0.0-0\" requires-reboot=#true {\n",
            "  file \"usr/bin/x\" mode=\"0755\" uid=0 gid=-1 \"variant.debug\"=#false\n",
            "  dir #\"with \"quote\" and \\ slash\"# mode=\"0755\" {}\n",
            "  trigger t { watch \"line\\nbreak\\ttab\\u{7f}\\u{202e}é\" }\n",
            "  \"odd name\" key=#null\n",
            "}",
        ))?;
        assert_eq!(
            write(&doc)?,
            concat!(
                "schema-version 1 kind=\"package-manifest\"\n",
                "package \"a/b\" publisher=\"canopa\" version=\"1.0.0-0\" requires-reboot=#true {\n",
                "    file \"usr/bin/x\" mode=\"0755\" uid=0 gid=-1 variant.debug=#false\n",
                "    dir \"with \\\"quote\\\" and \\\\ slash\" mode=\"0755\"\n",
                "    trigger \"t\" {\n",
                "        watch \"line\\nbreak\\ttab\\u{7f}\\u{202e}é\"\n",
                "    }\n",
                "    \"odd name\" key=#null\n",
                "}\n",
            )
        );
        Ok(())
    }

    #[test]
    fn canonical_text_parses_back_to_itself() -> Result<()> {
        let text = "a \"\\u{0}\\u{85}\\u{2028}\\u{feff}x\" \"\\\\\\\"\\b\\f\\r\"\n";
        let doc = super::super::document(text)?;
        assert_eq!(write(&doc)?, text);
        assert_eq!(write(&super::super::document(&write(&doc)?)?)?, text);
        Ok(())
    }

    #[test]
    fn refuses_what_it_cannot_write_canonically() {
        for text in ["a 1.5", "(t)a", "a (t)\"x\""] {
            let doc = super::super::document(text).expect(text);
            assert!(write(&doc).is_err(), "{text}");
        }
    }
}
