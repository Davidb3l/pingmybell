//! Spoken-summary cleanup (AC-4.3): strip markdown, code blocks, and paths;
//! first sentence, capped at 220 characters. Derived data only — never log
//! the output (§9 invariant 2).

const MAX_CHARS: usize = 220;

/// Hard cap on the RAW text we are willing to walk.
///
/// Everything below allocates at least one full-size copy of what it is
/// handed, and `split_whitespace` over a body of single-character tokens
/// produces one `String` per token — a 2 MB payload is ~1M allocations, tens
/// of MB transient, on a request path an agent is parked against. The ingest
/// body limit bounds that but does not make it cheap. A 220-character result
/// cannot plausibly have come from further in than this.
const MAX_INPUT_CHARS: usize = 8 * 1024;

pub fn clean(raw: &str) -> String {
    // Bound the input FIRST, so every pass below is proportional to what we
    // could ever show rather than to what was sent.
    let bounded: String = raw.chars().take(MAX_INPUT_CHARS).collect();
    // Then sanitize in two steps, in the only order that works:
    //
    //   * anything a ``` fence can hide behind goes FIRST, or the block's
    //     contents leak into a summary we persist and speak. That is every
    //     invisible character AND every other control: `strip_code_fences`
    //     matches on `line.trim_start()`, and `trim_start` only trims Unicode
    //     White_Space, so a bare U+0001 in front of a fence hides it exactly
    //     as well as a U+200B does;
    //   * newlines are the sole exception, held back because the fence scan
    //     is made of them — folding them up front would hand it a single line
    //     and disable it entirely. They are folded away afterwards.
    let visible = fold_controls_keeping_lines(&strip_invisible(&bounded));
    let no_fences = fold_controls(&strip_links(&strip_code_fences(&visible)));
    let mut words: Vec<String> = Vec::new();
    for token in no_fences.split_whitespace() {
        let t = strip_markdown_token(token);
        // Drop tokens left as bare punctuation ("`", "-", "—") — stripping
        // residue that reads as garbage in a spoken/visual one-liner.
        if t.is_empty() || t.chars().all(|c| !c.is_alphanumeric()) {
            continue;
        }
        words.push(t);
    }
    let joined = words.join(" ");
    let sentence = first_sentence(&joined);
    truncate_chars(sentence.trim(), MAX_CHARS)
}

/// Neutralize external text and cap it at `max` VISIBLE characters, in one
/// pass. Returns the text and whether anything was dropped off the end, so a
/// caller that shows an ellipsis can show a truthful one.
///
/// The approval card is why this exists. It shows the literal command the
/// user is authorizing, so a U+202E RIGHT-TO-LEFT OVERRIDE smuggled into a
/// tool payload — the "trojan source" trick, and an agent's context routinely
/// includes repository content it did not write — would make the card read as
/// something benign while the agent runs the real thing. The one surface
/// whose entire job is showing the user what they are about to authorize
/// cannot be allowed to lie.
///
/// The budget is spent on VISIBLE characters only, and that is the whole
/// point of doing this in one pass: sanitize-then-truncate lets a payload
/// spend the cap on zero-width padding and push the real command off the
/// card, which is the same bypass in a new costume. Whitespace runs collapse
/// and a leading run is dropped for the same reason — 5,000 newlines render
/// as nothing but would otherwise fill the line.
///
/// Deliberately NOT "strip anything unusual": Arabic, Hebrew, CJK and emoji
/// must render exactly as written. The Unicode bidi algorithm lays out real
/// RTL text from character properties alone, so removing the EXPLICIT
/// override/embedding/isolate controls costs legitimate text nothing.
///
/// Allocates at most `max` characters however long `raw` is; walking the rest
/// is a scan, not a copy.
pub fn sanitize_capped(raw: &str, max: usize) -> (String, bool) {
    let mut out = String::new();
    let mut kept = 0usize;
    let mut pending_space = false;
    for c in raw.chars() {
        if is_invisible(c) {
            continue;
        }
        // Control characters become whitespace rather than vanishing, so a
        // newline separates the words it sat between instead of welding them
        // together — and no single-line card or utterance ever carries a line
        // break, an ANSI escape, or a lone `\r`.
        if c.is_whitespace() || c.is_control() {
            pending_space = kept > 0;
            continue;
        }
        if pending_space {
            // `kept + 1` because a separator we cannot follow with a real
            // character is not worth spending the budget on: emitting it
            // would leave a dangling space that renders as "abc …".
            if kept + 1 >= max {
                return (out, true);
            }
            out.push(' ');
            kept += 1;
            pending_space = false;
        }
        if kept == max {
            return (out, true);
        }
        out.push(c);
        kept += 1;
    }
    (out, false)
}

/// Everything `sanitize_capped` does, with no length limit. For text that is
/// already bounded by its own caller.
#[cfg(test)]
fn sanitize(raw: &str) -> String {
    sanitize_capped(raw, usize::MAX).0
}

/// Drop the characters that reorder what follows them or occupy no space at
/// all. Both are ways to make a rendered string differ from the string that
/// will actually be executed.
fn strip_invisible(text: &str) -> String {
    text.chars().filter(|c| !is_invisible(*c)).collect()
}

/// Fold every control character to a space EXCEPT `\n`, which the fence scan
/// still needs. Runs before `strip_code_fences` so no control character can
/// sit in front of a ``` and hide it. A `\r` folded this early is harmless:
/// `str::lines` strips a trailing one anyway.
fn fold_controls_keeping_lines(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => c,
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

/// Fold the rest — the newlines held back above — once the line-based passes
/// are done, so nothing on a card or in an utterance carries a line break.
fn fold_controls(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Formatting characters that either reorder what follows them or occupy no
/// space at all. Both are ways to make a rendered string differ from the
/// string that will actually be executed.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00ad}'                  // soft hyphen
        | '\u{061c}'                // arabic letter mark (a bidi control)
        | '\u{180e}'                // mongolian vowel separator
        | '\u{200b}'                // zero-width space
        | '\u{200e}' | '\u{200f}'   // LRM / RLM
        | '\u{202a}'..='\u{202e}'   // bidi embeddings and OVERRIDES
        | '\u{2060}'..='\u{2064}'   // word joiner, invisible operators
        | '\u{2066}'..='\u{2069}'   // bidi ISOLATES
        | '\u{feff}'                // zero-width no-break space / BOM
        | '\u{fff9}'..='\u{fffb}'   // interlinear annotation
        // Tag characters can carry an entire hidden line of text inside one
        // visible glyph. Their only living use is the three subdivision flag
        // emoji, which degrade to a plain black flag here — a worse label,
        // never a misleading one.
        | '\u{e0000}'..='\u{e007f}'
    )
    // NOT stripped, on purpose: U+200C ZWNJ and U+200D ZWJ. They are real
    // content — Persian and Indic scripts spell words with them and emoji
    // sequences (👩‍💻, family emoji) are built from them — and unlike the set
    // above they cannot reorder or hide a neighbouring character.
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

    /// A fence marker must not be smuggleable past the stripper. `trim_start`
    /// only trims White_Space, so ANY control character in front of the ```
    /// would otherwise leave the block's contents in a summary we persist and
    /// speak aloud.
    #[test]
    fn a_cap_that_lands_on_a_word_boundary_does_not_leave_a_dangling_space() {
        // The separator is only worth its slot if a real character can follow.
        assert_eq!(sanitize_capped("abc d", 4), ("abc".into(), true));
        assert_eq!(sanitize_capped("abc d", 5), ("abc d".into(), false));
        // Trailing whitespace is not truncation: nothing was lost.
        assert_eq!(sanitize_capped("abc   ", 3), ("abc".into(), false));
    }

    #[test]
    fn no_control_character_can_hide_a_code_fence() {
        for hider in ['\u{0001}', '\u{0008}', '\u{000e}', '\u{001b}', '\u{001f}', '\u{007f}'] {
            let raw = format!("Report\n{hider}```\nsecret_token=abc123\n```\nready");
            let out = clean(&raw);
            assert!(
                !out.contains("secret_token"),
                "{:?} smuggled a fenced block into {out:?}",
                hider
            );
        }
        // The same trick on the CLOSING fence used to swallow everything
        // after it instead.
        let out = clean("Report\n```\nsecret_token=abc123\n\u{0001}```\nready");
        assert!(!out.contains("secret_token"), "got {out:?}");
        assert!(out.contains("ready"), "closing fence ate the tail: {out:?}");
    }

    #[test]
    fn strips_markdown_and_code() {
        let raw = "**Done!** I fixed the `parse_config` bug.\n```rust\nfn main() {}\n```\nAll tests pass.";
        assert_eq!(clean(raw), "Done!");
    }

    /// Sanitizing must not disable the fence stripper. Folding control
    /// characters to spaces before `strip_code_fences` runs would hand it a
    /// single line, and a code block's contents would then be persisted to
    /// the events table and read aloud (AC-4.3, §9 invariant 4).
    #[test]
    fn code_blocks_are_still_stripped_after_neutralizing() {
        assert_eq!(
            clean("Report\n```\nsecret_token=abc123\n```\nready"),
            "Report ready"
        );
        assert_eq!(clean("Fixed it\n```\nrm -rf /\n```\nyes."), "Fixed it yes.");
        // A turn that OPENS with a code block is common; the sentence after
        // it must survive.
        assert_eq!(clean("```\nrm -rf /tmp/thing\n```\nDone."), "Done.");
        // …and NOTHING may sit in front of a fence and hide it from the line
        // scan. `trim_start` only trims White_Space, so every one of these
        // used to leak the block: zero-width characters and, just as
        // effectively, any bare control character.
        for hider in ["\u{200b}", "\u{feff}", "\u{1}", "\u{1b}", "\u{7f}", "\u{e}"] {
            assert_eq!(
                clean(&format!(
                    "Report\n{hider}```\nsecret_token=abc123\n```\nready"
                )),
                "Report ready",
                "{hider:?} hid the opening fence"
            );
            // Hiding the CLOSING fence is the other half: it would swallow
            // the rest of the message instead of leaking the block.
            assert_eq!(
                clean(&format!(
                    "Report\n```\nsecret_token=abc123\n{hider}```\nready"
                )),
                "Report ready",
                "{hider:?} hid the closing fence"
            );
        }
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

    /// The trojan-source case. A bidi override on the approval surface would
    /// let a command render as the reverse of what runs.
    #[test]
    fn bidi_overrides_and_invisibles_never_survive() {
        // What an attack looks like: the override makes the tail of the line
        // render right-to-left, so `rm -rf ~` can be dressed up as a comment.
        let attack = "echo \u{202e}~ fr- mr\u{202c} hi";
        let out = sanitize(attack);
        assert!(
            !out.chars().any(is_invisible),
            "no bidi control may reach the card: {out:?}"
        );
        assert_eq!(out, "echo ~ fr- mr hi", "only the controls are removed");

        for hostile in [
            "\u{202a}", "\u{202b}", "\u{202c}", "\u{202d}", "\u{202e}", // embeddings/overrides
            "\u{2066}", "\u{2067}", "\u{2068}", "\u{2069}", // isolates
            "\u{200b}", "\u{200e}", "\u{200f}", "\u{feff}", "\u{00ad}", "\u{061c}",
            "\u{2060}", "\u{fff9}", "\u{e0041}",
        ] {
            assert_eq!(sanitize(hostile), "", "{hostile:?} must vanish");
            assert_eq!(clean(&format!("ok{hostile}ay.")), "okay.");
        }

        // Control characters separate, they do not weld.
        assert_eq!(sanitize("rm\u{1b}[2K -rf\t/x\nnow"), "rm [2K -rf /x now");
        assert_eq!(clean("first line\r\nsecond"), "first line second");

        // The cap is spent on VISIBLE characters, and reports honestly what
        // it dropped — otherwise zero-width padding just pushes the real
        // command off the card again, this time silently.
        let padded = format!("{}rm -rf ~", "\u{200b}".repeat(10_000));
        assert_eq!(sanitize_capped(&padded, 8), ("rm -rf ~".to_string(), false));
        assert_eq!(sanitize_capped(&padded, 4), ("rm -".to_string(), true));
        // Whitespace runs collapse to one, and a leading run costs nothing.
        assert_eq!(
            sanitize_capped("  \n\n echo \n\n hi \n", 40),
            ("echo hi".to_string(), false)
        );
        // Boundaries. Trailing whitespace is not "dropped content", and a
        // separator we cannot follow with a real character is never emitted —
        // otherwise the caller renders "abc …".
        assert_eq!(sanitize_capped("abc   ", 3), ("abc".into(), false));
        assert_eq!(sanitize_capped("abc d", 5), ("abc d".into(), false));
        assert_eq!(sanitize_capped("abc d", 4), ("abc".into(), true));
        assert_eq!(sanitize_capped("abc d", 3), ("abc".into(), true));
        assert_eq!(sanitize_capped("abc", 0), (String::new(), true));
        assert_eq!(sanitize_capped("", 0), (String::new(), false));
        assert_eq!(sanitize_capped("   ", 5), (String::new(), false));
    }

    /// The other half of the contract: real text must come out intact. A
    /// sanitizer that mangles Arabic or emoji is a bug, not a fix.
    #[test]
    fn legitimate_text_in_any_script_is_untouched() {
        for good in [
            "مرحبا بالعالم",                    // Arabic
            "שלום עולם",                        // Hebrew
            "更新了配置文件",                    // CJK
            "👩‍💻 shipped it 🎉",                // emoji, incl. a ZWJ sequence
            "café naïve — résumé",              // accents and punctuation
            "ЖУРНАЛ обновлён",                  // Cyrillic
            "मैंने काम पूरा किया",                    // Devanagari
        ] {
            assert_eq!(sanitize(good), good, "{good} must survive verbatim");
        }
        // And through the full cleaner, which must not eat the sentence.
        assert_eq!(clean("تم تحديث الملف."), "تم تحديث الملف.");
        assert_eq!(clean("更新了配置文件。 next"), "更新了配置文件。 next");
        // A lone emoji still disappears in `clean` — but that is the
        // spoken-summary rule dropping a token with nothing to pronounce,
        // not the sanitizer, which leaves it alone (asserted above).
        assert_eq!(clean("Deployed 🎉 to prod."), "Deployed to prod.");
    }

    /// Bounded work per request: the cleaner must not walk (or allocate over)
    /// a megabyte of hostile input to produce 220 characters.
    #[test]
    fn the_input_is_capped_before_any_of_the_passes_run() {
        // The pathological shape from the report: single-character tokens,
        // one `String` each under the old code.
        let huge = "a ".repeat(1_000_000);
        let out = clean(&huge);
        assert!(out.chars().count() <= MAX_CHARS);

        // Anything past the input cap is simply not read.
        let beyond = "x ".repeat(MAX_INPUT_CHARS) + "NEEDLE.";
        assert!(
            !clean(&beyond).contains("NEEDLE"),
            "text past the input cap must not be reachable"
        );
        // A summary of a realistic length is unaffected by the cap.
        let realistic = "y".repeat(MAX_INPUT_CHARS / 2) + " done.";
        assert!(clean(&realistic).chars().count() <= MAX_CHARS);
    }

    #[test]
    fn stripping_residue_is_dropped() {
        let raw = "saved in `/tmp/x/` - manifest.json 1.7 MB — done";
        let out = clean(raw);
        assert!(!out.contains('`'), "no stray backticks: {out}");
        assert!(!out.contains(" - "), "no orphan dashes: {out}");
        assert!(out.contains("manifest.json"));
    }
}
