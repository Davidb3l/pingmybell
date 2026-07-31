//! Spoken-summary cleanup (AC-4.3): strip markdown, code blocks, and paths;
//! first sentence, capped at 220 characters. Derived data only — never log
//! the output (§9 invariant 2).

const MAX_CHARS: usize = 220;

pub fn clean(raw: &str) -> String {
    let no_fences = strip_links(&strip_code_fences(raw));
    let mut words: Vec<String> = Vec::new();
    for token in no_fences.split_whitespace() {
        let t = strip_markdown_token(token);
        if t.is_empty() {
            continue;
        }
        words.push(t);
    }
    let joined = words.join(" ");
    let sentence = first_sentence(&joined);
    truncate_chars(sentence.trim(), MAX_CHARS)
}

fn strip_code_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// `[text](url)` → `text`, across whitespace (link text is often multi-word).
fn strip_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(mid) = rest.find("](") {
        let before = &rest[..mid];
        if let (Some(open), Some(close)) = (before.rfind('['), rest[mid + 2..].find(')')) {
            out.push_str(&before[..open]);
            out.push_str(&before[open + 1..]);
            rest = &rest[mid + 2 + close + 1..];
        } else {
            out.push_str(&rest[..mid + 2]);
            rest = &rest[mid + 2..];
        }
    }
    out.push_str(rest);
    out
}

fn strip_markdown_token(token: &str) -> String {
    let trimmed = token
        .trim_matches(|c: char| matches!(c, '*' | '_' | '`' | '#' | '>' | '(' | ')' | '[' | ']'));

    // Paths and URLs don't read well aloud: keep only the basename of
    // path-like tokens, drop URLs entirely.
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return String::new();
    }
    if trimmed.matches('/').count() >= 2 || trimmed.starts_with('/') || trimmed.starts_with("~/") {
        return trimmed
            .trim_end_matches(&['.', ',', ':', ';'][..])
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
    }
    trimmed.to_string()
}

fn first_sentence(text: &str) -> &str {
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?') {
            // Sentence boundary only if followed by whitespace/end (keeps
            // "v2.1" or "e.g." from truncating too early is out of scope —
            // good enough for spoken one-liners).
            let rest = &text[i + c.len_utf8()..];
            if rest.is_empty() || rest.starts_with(' ') {
                return &text[..i + c.len_utf8()];
            }
        }
    }
    text
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markdown_and_code() {
        let raw = "**Done!** I fixed the `parse_config` bug.\n```rust\nfn main() {}\n```\nAll tests pass.";
        assert_eq!(clean(raw), "Done!");
    }

    #[test]
    fn takes_first_sentence_and_caps_length() {
        let raw = "I refactored the ingest server. Then I did many other things.";
        assert_eq!(clean(raw), "I refactored the ingest server.");
        let long = "word ".repeat(100);
        assert!(clean(&long).chars().count() <= 220);
    }

    #[test]
    fn paths_become_basenames_and_urls_vanish() {
        let raw = "Updated /Users/dave/projects/api-server/src/main.rs per https://example.com/spec fully";
        let out = clean(raw);
        assert!(out.contains("main.rs"));
        assert!(!out.contains("/Users"));
        assert!(!out.contains("example.com"));
    }

    #[test]
    fn links_keep_text_only() {
        assert_eq!(
            clean("See [the docs](https://docs.rs) now"),
            "See the docs now"
        );
    }
}
