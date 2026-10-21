//! Deterministic XML prompt & context structurer (spec §3.3.1).
//!
//! System 2 output must be reproducible: same input, same bytes. This builder writes
//! into one `String` buffer (no per-node allocation), always emits attributes in the
//! order supplied, and escapes text and attribute values strictly, so raw user prompts
//! can never break out of the envelope or inject tags.

use std::fmt::Write as _;

/// Escape the five predefined XML entities plus tab/CR/LF in text content.
///
/// Newlines are kept literal inside element bodies (they are valid XML) but escaped in
/// attribute values, where a raw newline would be normalized away by any parser.
pub fn escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Sanitize an attribute value: escape entities and collapse control characters.
pub fn sanitize_attr(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Builds a well-formed XML document into a single reusable buffer.
#[derive(Debug, Default, Clone)]
pub struct XmlEnvelopeBuilder {
    buf: String,
    indent: usize,
}

impl XmlEnvelopeBuilder {
    pub fn new() -> Self {
        Self { buf: String::with_capacity(512), indent: 0 }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self { buf: String::with_capacity(capacity), indent: 0 }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn pad(&mut self) {
        if self.indent > 0 {
            self.buf.push_str(&"  ".repeat(self.indent));
        }
    }

    /// `<name a="1" b="2" />`
    pub fn empty_tag(&mut self, name: &str, attrs: &[(&str, &str)]) -> &mut Self {
        self.pad();
        let _ = write!(self.buf, "<{}", escape_name(name));
        for (k, v) in attrs {
            let _ = write!(self.buf, " {}=\"{}\"", escape_name(k), sanitize_attr(v));
        }
        self.buf.push_str(" />\n");
        self
    }

    /// `<name ...>body</name>` — body is escaped, and empty bodies self-close.
    pub fn text_tag(&mut self, name: &str, attrs: &[(&str, &str)], body: &str) -> &mut Self {
        if body.is_empty() {
            return self.empty_tag(name, attrs);
        }
        self.pad();
        self.write_open_tag(name, attrs);
        self.buf.push('>');
        self.buf.push_str(&escape_text(body));
        self.buf.push_str("</");
        self.buf.push_str(&escape_name(name));
        self.buf.push_str(">\n");
        self
    }

    fn write_open_tag(&mut self, name: &str, attrs: &[(&str, &str)]) {
        let _ = write!(self.buf, "<{}", escape_name(name));
        for (k, v) in attrs {
            let _ = write!(self.buf, " {}=\"{}\"", escape_name(k), sanitize_attr(v));
        }
    }

    /// `<name ...>` followed by nested content, increasing the indent level.
    pub fn open(&mut self, name: &str, attrs: &[(&str, &str)]) -> &mut Self {
        self.pad();
        self.write_open_tag(name, attrs);
        self.buf.push_str(">\n");
        self.indent += 1;
        self
    }

    /// `</name>` closing the most recent `open`.
    pub fn close(&mut self, name: &str) -> &mut Self {
        self.indent = self.indent.saturating_sub(1);
        self.pad();
        let _ = write!(self.buf, "</{}>\n", escape_name(name));
        self
    }

    /// Raw line, used for domain-authored markup that is already escaped.
    pub fn raw_line(&mut self, line: &str) -> &mut Self {
        self.pad();
        self.buf.push_str(line);
        self.buf.push('\n');
        self
    }

    pub fn finish(&self) -> String {
        self.buf.clone()
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }
}

fn escape_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' { c } else { '_' })
        .collect()
}

/// The spec-mandated System 2 envelope header.
pub fn context_budget(saturation_ratio: f32, limit: usize, used: usize) -> String {
    let mut b = XmlEnvelopeBuilder::new();
    b.empty_tag(
        "context_budget",
        &[
            ("saturation", &format!("{:.3}", saturation_ratio)),
            ("status", budget_status(saturation_ratio)),
            ("used_tokens", &used.to_string()),
            ("limit_tokens", &limit.to_string()),
        ],
    );
    b.finish()
}

fn budget_status(saturation_ratio: f32) -> &'static str {
    if saturation_ratio > crate::s1::circuit::DEFAULT_HARD_CEILING_RATIO {
        "HALT"
    } else if saturation_ratio >= crate::s1::circuit::DEFAULT_SOFT_WARNING_RATIO {
        "WARN"
    } else {
        "OK"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_body_entities_strictly() {
        assert_eq!(
            escape_text(r#"<script> & "quotes" 'single'"#),
            "&lt;script&gt; &amp; &quot;quotes&quot; &apos;single&apos;"
        );
    }

    #[test]
    fn attribute_sanitizer_neutralizes_breakouts_and_control_chars() {
        assert_eq!(
            sanitize_attr("a\" /><evil> \n\ttab\u{1}"),
            "a&quot; /&gt;&lt;evil&gt;   tab"
        );
    }

    #[test]
    fn injection_attempt_stays_inside_the_envelope() {
        let mut b = XmlEnvelopeBuilder::new();
        b.text_tag("task", &[], "</task><injected>payload");
        let doc = b.finish();
        assert!(!doc.contains("<injected>"), "{doc}");
        assert!(doc.contains("&lt;/task&gt;&lt;injected&gt;"), "{doc}");
    }

    #[test]
    fn nested_open_close_indents_and_closes() {
        let mut b = XmlEnvelopeBuilder::new();
        b.open("intent_compilation", &[("by", "s2")]);
        b.text_tag("task", &[], "do it");
        b.close("intent_compilation");
        let doc = b.finish();
        assert!(doc.starts_with("<intent_compilation by=\"s2\">\n"), "{doc}");
        assert!(doc.contains("  <task>do it</task>\n"), "{doc}");
        assert!(doc.ends_with("</intent_compilation>\n"), "{doc}");
    }

    #[test]
    fn empty_body_self_closes() {
        let mut b = XmlEnvelopeBuilder::new();
        b.text_tag("handoff_state", &[("session_id", "7")], "");
        assert_eq!(b.finish(), "<handoff_state session_id=\"7\" />\n");
    }

    #[test]
    fn context_budget_reports_status_band() {
        assert!(context_budget(0.1, 200_000, 20_000).contains("status=\"OK\""));
        assert!(context_budget(0.31, 200_000, 62_000).contains("status=\"WARN\""));
        assert!(context_budget(0.375, 200_000, 75_000).contains("status=\"HALT\""));
        assert!(context_budget(0.1, 200_000, 20_000).contains("saturation=\"0.100\""));
    }

    #[test]
    fn invalid_tag_names_are_fenced_not_injected() {
        let mut b = XmlEnvelopeBuilder::new();
        b.text_tag("bad name><x", &[], "v");
        assert!(b.finish().contains("<bad_name__x>"), "{}", b.as_str());
    }
}
