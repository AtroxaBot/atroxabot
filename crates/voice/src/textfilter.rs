//! Streaming markdown-to-speech filter: turns the agent's text into chunks worth saying out loud.
//!
//! Code blocks, markdown markup, links, bracketed cues and emoji are not spoken. Text is released one
//! sentence (or line) at a time so speech can start before the reply is complete.

/// Said once per reply in place of its code blocks.
pub(crate) const CODE_NOTICE: &str = "I've put the code on screen. ";
/// A partial line longer than this is cut at a space even without a sentence end.
const MAX_HOLD: usize = 160;

/// Feed it text as it streams in; every returned chunk is clean and ends with one space.
pub(crate) struct SpeechFilter {
    buffer: String,
    /// Does `buffer` begin at the start of a line?
    line_start: bool,
    in_code: bool,
    noticed: bool,
}

impl SpeechFilter {
    pub(crate) const fn new() -> Self {
        Self { buffer: String::new(), line_start: true, in_code: false, noticed: false }
    }

    /// Adds streamed text and returns the chunks that are ready to be spoken.
    pub(crate) fn feed(&mut self, text: &str) -> Vec<String> {
        self.buffer.push_str(text);
        let mut out = Vec::new();
        while let Some((line, rest)) = self.buffer.split_once('\n') {
            let (line, rest) = (line.to_owned(), rest.to_owned());
            self.buffer = rest;
            out.extend(self.emit(&line, true));
        }
        // Hold partial lines in code and ones that may become a fence or a table row.
        if self.in_code || (self.line_start && self.buffer.trim_start().starts_with(['`', '~', '|'])) {
            return out;
        }
        let mut cut = sentence_end(&self.buffer);
        if cut == 0 && self.buffer.chars().count() > MAX_HOLD {
            cut = self.buffer.rfind(' ').map_or(0, |space| space + 1);
        }
        if cut > 0
            && let Some((part, rest)) = self.buffer.split_at_checked(cut)
        {
            let (part, rest) = (part.to_owned(), rest.to_owned());
            self.buffer = rest;
            out.extend(self.emit(&part, false));
        }
        out
    }

    /// The reply is complete: returns what was still held back.
    pub(crate) fn flush(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buffer);
        if rest.is_empty() { Vec::new() } else { self.emit(&rest, true).into_iter().collect() }
    }

    /// `whole`: `text` runs to the end of its line.
    fn emit(&mut self, text: &str, whole: bool) -> Option<String> {
        let at_start = std::mem::replace(&mut self.line_start, whole);
        if at_start && is_fence(text) {
            self.in_code = !self.in_code;
            if self.in_code && !self.noticed {
                self.noticed = true;
                return Some(CODE_NOTICE.to_owned());
            }
            return None;
        }
        if self.in_code {
            return None;
        }
        let cleaned = if at_start { inline(&line_rules(text)) } else { inline(text) };
        (!cleaned.is_empty()).then(|| cleaned + " ")
    }
}

/// The byte offset just past the last `.!?:;` that is followed by whitespace, or 0.
fn sentence_end(text: &str) -> usize {
    let mut cut = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if matches!(c, '.' | '!' | '?' | ':' | ';') && chars.peek().is_some_and(|(_, next)| next.is_whitespace()) {
            cut = at + 1;
        }
    }
    cut
}

/// Splits `text` into pieces of at most `limit` bytes: at a sentence end where there is one, else at a
/// space, else between two characters.
pub(crate) fn pieces(mut text: &str, limit: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    while text.len() > limit {
        let ends = text.char_indices().map(|(at, c)| at + c.len_utf8());
        let window = ends.take_while(|&end| end <= limit).last().unwrap_or(text.len());
        let head = text.get(..window).unwrap_or(text);
        let cut = match sentence_end(head) {
            0 => head.rfind(' ').map_or(window, |space| space + 1),
            end => end,
        };
        let Some((piece, rest)) = text.split_at_checked(cut) else { break };
        pieces.push(piece);
        text = rest.trim_start();
    }
    if !text.is_empty() {
        pieces.push(text);
    }
    pieces
}

fn is_fence(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("```") || line.starts_with("~~~")
}

/// Markup that only means something at the start of a line: rules, tables, headings, quotes, lists.
fn line_rules(line: &str) -> String {
    let stripped = line.trim();
    let mut marks = stripped.chars().filter(|c| !c.is_whitespace());
    let is_rule = marks.next().is_some_and(|first| "-*_".contains(first) && marks.clone().all(|c| c == first))
        && marks.count() >= 2;
    let is_table_separator =
        stripped.contains('|') && stripped.contains('-') && stripped.chars().all(|c| "|-: ".contains(c));
    if is_rule || is_table_separator {
        return String::new();
    }
    if let Some(row) = stripped.strip_prefix('|').and_then(|row| row.strip_suffix('|')) {
        return row.split('|').map(str::trim).filter(|cell| !cell.is_empty()).collect::<Vec<_>>().join(", ");
    }
    strip_list(strip_quote(strip_heading(line))).to_owned()
}

/// Removes `#` to `######` and the whitespace after it.
fn strip_heading(line: &str) -> &str {
    let rest = line.trim_start().trim_start_matches('#');
    let hashes = line.trim_start().len() - rest.len();
    if (1..=6).contains(&hashes) && rest.starts_with(char::is_whitespace) { rest.trim_start() } else { line }
}

/// Removes leading `>` markers, each with at most one whitespace after it.
fn strip_quote(line: &str) -> &str {
    let mut rest = line.trim_start();
    if !rest.starts_with('>') {
        return line;
    }
    while let Some(after) = rest.strip_prefix('>') {
        rest = after.strip_prefix(char::is_whitespace).unwrap_or(after);
    }
    rest
}

/// Removes a list marker (`-`, `*`, `+`, `1.` or `1)`) and the whitespace after it.
fn strip_list(line: &str) -> &str {
    let start = line.trim_start();
    let after_marker = start.strip_prefix(['-', '*', '+']).or_else(|| {
        let after_digits = start.trim_start_matches(|c: char| c.is_ascii_digit());
        if after_digits.len() == start.len() { None } else { after_digits.strip_prefix(['.', ')']) }
    });
    match after_marker {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => line,
    }
}

/// Removes inline markup and collapses whitespace.
fn inline(text: &str) -> String {
    let text = strip_cues(&snake_case(&emphasis(&urls(&links(text)))));
    let text: String = text.chars().filter(|&c| !is_emoji(c)).collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `[label](target)` becomes `label`.
fn links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('[') {
        out.push_str(before);
        if let Some((label, tail)) = after.split_once(']')
            && let Some((_, remainder)) = tail.strip_prefix('(').and_then(|target| target.split_once(')'))
        {
            out.push_str(label);
            rest = remainder;
        } else {
            out.push('[');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// A bare URL becomes "a link"; punctuation that ends the sentence stays.
fn urls(text: &str) -> String {
    const TRAILING: [char; 8] = ['.', ',', ';', ':', '!', '?', ')', ']'];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once("http") {
        out.push_str(before);
        let address = after.strip_prefix("://").or_else(|| after.strip_prefix("s://"));
        let run = address.and_then(|address| address.split(char::is_whitespace).next()).unwrap_or("");
        let Some(first) = run.chars().next() else {
            out.push_str("http");
            rest = after;
            continue;
        };
        let url_len = run.trim_end_matches(TRAILING).len().max(first.len_utf8());
        out.push_str("a link");
        rest = address.and_then(|address| address.get(url_len..)).unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// Removes `*`, backticks and the pairs `__` and `~~`; what they wrap stays.
fn emphasis(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '`' => {}
            '_' | '~' if chars.peek() == Some(&c) => {
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// `my_var_name` becomes `my var name`.
fn snake_case(text: &str) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut previous = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let inside_word = previous.is_some_and(is_word) && chars.peek().copied().is_some_and(is_word);
        out.push(if c == '_' && inside_word { ' ' } else { c });
        previous = Some(c);
    }
    out
}

/// Removes bracketed cues like `[laughing]`: up to 30 characters, no nested brackets, not a link label.
pub(crate) fn strip_cues(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('[') {
        out.push_str(before);
        match after.split_once(']') {
            Some((cue, tail))
                if !cue.contains('[') && (1..=30).contains(&cue.chars().count()) && !tail.starts_with('(') =>
            {
                rest = tail;
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

const fn is_emoji(c: char) -> bool {
    matches!(
        c,
        '\u{1F000}'..='\u{1FAFF}'
            | '\u{2300}'..='\u{23FF}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{2B00}'..='\u{2BFF}'
            | '\u{FE0F}'
            | '\u{200D}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(pieces: &[&str]) -> Vec<String> {
        let mut filter = SpeechFilter::new();
        let mut out = Vec::new();
        for piece in pieces {
            out.extend(filter.feed(piece));
        }
        out.extend(filter.flush());
        out
    }

    #[test]
    fn releases_at_sentence_end_while_streaming() {
        let mut filter = SpeechFilter::new();
        assert!(filter.feed("Hello there").is_empty());
        assert_eq!(filter.feed(". How are"), ["Hello there. "]);
        assert_eq!(filter.feed(" you? I am"), ["How are you? "]);
        assert_eq!(filter.flush(), ["I am "]);
    }

    #[test]
    fn inline_markdown() {
        assert_eq!(
            run(&["This is **bold**, *italic*, __strong__ and ~~gone~~ with `code`.\n"]),
            ["This is bold, italic, strong and gone with code. "]
        );
    }

    #[test]
    fn urls_links_and_snake_case() {
        assert_eq!(
            run(&["See https://example.com/a_b. Then [the docs](https://x.y) for my_var_name.\n"]),
            ["See a link. Then the docs for my var name. "]
        );
    }

    #[test]
    fn url_edge_cases() {
        assert_eq!(urls("(see http://a.b/c?d=1), ok"), "(see a link), ok");
        assert_eq!(urls("https://"), "https://");
        assert_eq!(urls("the http verb"), "the http verb");
        assert_eq!(urls("x https://a.b"), "x a link");
    }

    #[test]
    fn headings_lists_quotes_and_rules() {
        let text = "# Title\n- one\n* two\n+ three\n1. four\n2) five\n> quoted\n---\n***\n";
        assert_eq!(run(&[text]), ["Title ", "one ", "two ", "three ", "four ", "five ", "quoted "]);
    }

    #[test]
    fn line_markup_needs_its_whitespace() {
        assert_eq!(
            run(&["#hashtag\n-5 degrees\n1.5 litres\n####### seven\n> > nested\n- - -\n"]),
            ["#hashtag ", "-5 degrees ", "1.5 litres ", "####### seven ", "nested "]
        );
    }

    #[test]
    fn code_fence_split_across_chunks() {
        let out = run(&["Here:\n``", "`python\nprint(1)\n", "x = 2. y = 3\n``", "`\nDone.", "\n```\nmore\n```\n"]);
        assert_eq!(out, ["Here: ", CODE_NOTICE, "Done. "]);
    }

    #[test]
    fn tables() {
        assert_eq!(run(&["| Name | Age |\n|---|:--:|\n| Ann | 30 |\n"]), ["Name, Age ", "Ann, 30 "]);
    }

    #[test]
    fn long_text_without_sentence_end_is_cut_at_a_space() {
        let mut filter = SpeechFilter::new();
        let out = filter.feed(&"word ".repeat(40));
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("word "));
        assert!(out[0].len() <= 200);
        assert!(filter.flush().is_empty());
    }

    #[test]
    fn emoji_removed() {
        assert_eq!(run(&["Great job 🎉✨ really, thanks 👍\n"]), ["Great job really, thanks "]);
    }

    #[test]
    fn bracket_cues_removed() {
        assert_eq!(run(&["[laughing] That is funny. [soft tone] Okay.", "\n"]), ["That is funny. ", "Okay. "]);
    }

    #[test]
    fn links_become_their_label() {
        assert_eq!(run(&["Read [the guide](https://x.y/guide) now.\n"]), ["Read the guide now. "]);
    }

    #[test]
    fn long_chunks_are_split_at_sentence_ends_then_spaces() {
        assert_eq!(pieces("Short one. ", 20), ["Short one. "]);
        assert_eq!(pieces("One two. Three four five. Six. ", 20), ["One two.", "Three four five.", "Six. "]);
        assert_eq!(pieces("no sentence end anywhere here", 12), ["no sentence ", "end ", "anywhere ", "here"]);
        assert_eq!(pieces("ééééé", 4), ["éé", "éé", "é"]);
        assert_eq!(pieces("日本語", 1), ["日本語"]);
        let long = "Word after word. ".repeat(100);
        let split = pieces(&long, 300);
        assert!(split.iter().all(|piece| piece.len() <= 300 && piece.starts_with("Word")));
        assert_eq!(split.concat().replace('.', ". ").split_whitespace().count(), 300);
    }

    #[test]
    fn non_ascii_text_survives() {
        assert_eq!(
            run(&["Één **ijsje**, graag… Dank je. ", "日本語。\n"]),
            ["Één ijsje, graag… Dank je. ", "日本語。 "]
        );
    }
}
