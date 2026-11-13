//! What a tool call that changes a file changed, as the lines of a diff:
//! what the conversation shows for an edit in place of the call's JSON.
//!
//! Every engine names its edits its own way: Claude Code's `Edit`
//! (`old_string`, `new_string`), `MultiEdit` (`edits`) and `Write`
//! (`content`), an API agent's `edit` (`old`, `new`, `edits`) and `write`,
//! and Codex's file changes (`changes`, each a unified diff). A replaced
//! text is compared line by line with what replaces it, so the lines an
//! edit only quotes to find its place show as context, not as a change.

use atroxabot_core::{Block, ToolUse};
use serde_json::Value;

use crate::doc::DiffKind;

/// Most lines of one call's changes shown; the rest is counted.
const SHOWN: usize = 400;
/// Most cells of the table two texts are compared in: larger ones (after
/// what they begin and end with alike) show as all removed, then all added.
const CELLS: usize = 400_000;

/// The lines a call changed, and how many it added and removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Change {
    /// The lines, at most [`SHOWN`] of them and a note of the rest.
    pub lines: Vec<(DiffKind, String)>,
    /// How many lines it added.
    pub added: u32,
    /// How many lines it removed.
    pub removed: u32,
}

/// A file a run of tool calls changed, with all they did to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Touched {
    /// The file, as the calls named it.
    pub path: String,
    /// How many lines they added.
    pub added: u32,
    /// How many lines they removed.
    pub removed: u32,
    /// The lines of every change, in the order they were made, an empty
    /// note between two changes.
    pub lines: Vec<(DiffKind, String)>,
}

impl Touched {
    /// The changes as the lines of a diff, each after its sign: what the diff's window shows.
    pub fn text(&self) -> String {
        let line = |(kind, line): &(DiffKind, String)| match kind {
            DiffKind::Added => format!("+{line}"),
            DiffKind::Removed => format!("-{line}"),
            DiffKind::Context => format!(" {line}"),
            DiffKind::Note if line.starts_with("@@") => line.clone(),
            DiffKind::Note => format!("@@ {line}").trim_end().to_owned(),
        };
        self.lines.iter().map(line).collect::<Vec<_>>().join("\n")
    }
}

/// How many of `lines` were added and removed.
fn counts(lines: &[(DiffKind, String)]) -> (u32, u32) {
    let count = |kind: DiffKind| lines.iter().filter(|(of, _)| *of == kind).count() as u32;
    (count(DiffKind::Added), count(DiffKind::Removed))
}

/// What the call of the tool `name` with `input` did, file by file: each
/// file it names with the lines of its change (none for a call that
/// changes no file, or says nothing of what it changed).
fn files(name: &str, input: &Value) -> Vec<(String, Vec<(DiffKind, String)>)> {
    let text = |value: &Value, keys: &[&str]| keys.iter().find_map(|key| value.get(key)?.as_str().map(str::to_owned));
    if let Some(changes) = input.get("changes").and_then(Value::as_array) {
        // Codex: each file's change is a unified diff, or the whole of a new file.
        let file = |change: &Value| {
            let diff = text(change, &["diff"]).unwrap_or_default();
            let lines: Vec<(DiffKind, String)> = match change.get("kind").and_then(Value::as_str) {
                Some("add") => diff.lines().map(|line| (DiffKind::Added, line.to_owned())).collect(),
                Some("delete") => diff.lines().map(|line| (DiffKind::Removed, line.to_owned())).collect(),
                _ => unified(&diff),
            };
            (text(change, &["path"]).unwrap_or_default(), lines)
        };
        return changes.iter().map(file).collect();
    }
    // Only a call that names a file changes one.
    let Some(path) = text(input, &["file_path", "path", "notebook_path"]) else { return Vec::new() };
    let pair = |value: &Value| text(value, &["old_string", "old"]).zip(text(value, &["new_string", "new"]));
    let edits = input.get("edits").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let pairs: Vec<(String, String)> = pair(input).into_iter().chain(edits.iter().filter_map(pair)).collect();
    let mut lines = Vec::new();
    for (index, (old, new)) in pairs.iter().enumerate() {
        if index > 0 {
            lines.push((DiffKind::Note, "⋯".to_owned()));
        }
        compare(old, new, &mut lines);
    }
    if pairs.is_empty() {
        // A file written whole: every line of it is new, as far as the call says.
        let name = name.to_lowercase();
        let writes = ["write", "create", "notebook"].iter().any(|word| name.contains(word));
        let whole = text(input, &["content", "new_source"]).filter(|_| writes).unwrap_or_default();
        lines.extend(whole.lines().map(|line| (DiffKind::Added, line.to_owned())));
    }
    vec![(path, lines)]
}

/// What the call of the tool `name` with `input` changed, for its row in
/// the conversation; `None` for a call that changes no file (or says
/// nothing of what it changed). Several files follow each other, each
/// under its name.
pub(super) fn of(name: &str, input: &Value) -> Option<Change> {
    let files = files(name, input);
    let several = files.len() > 1;
    let mut lines = Vec::new();
    for (path, changed) in files {
        if several {
            lines.push((DiffKind::Note, path));
        }
        lines.extend(changed);
    }
    let (added, removed) = counts(&lines);
    if added + removed == 0 {
        return None;
    }
    if lines.len() > SHOWN {
        let more = lines.len() - SHOWN;
        lines.truncate(SHOWN);
        lines.push((DiffKind::Note, format!("… {more} more {}", if more == 1 { "line" } else { "lines" })));
    }
    Some(Change { lines, added, removed })
}

/// The files that `calls` changed, in the order they first were: what a
/// turn did, as far as its own edits say (what a command it ran changed
/// is not known). Only calls that went through count.
pub(super) fn touched<'a>(calls: impl IntoIterator<Item = &'a ToolUse>) -> Vec<Touched> {
    let mut out: Vec<Touched> = Vec::new();
    for call in calls {
        if call.output.is_none() || call.is_error {
            continue;
        }
        let Ok(input) = serde_json::from_str::<Value>(&call.input) else { continue };
        for (path, lines) in files(&call.name, &input) {
            let (added, removed) = counts(&lines);
            if added + removed == 0 {
                continue;
            }
            match out.iter_mut().find(|file| file.path == path) {
                Some(file) => {
                    file.lines.push((DiffKind::Note, String::new()));
                    file.lines.extend(lines);
                    (file.added, file.removed) = (file.added + added, file.removed + removed);
                }
                None => out.push(Touched { path, added, removed, lines }),
            }
        }
    }
    out
}

/// The tool calls among `blocks`, those of the subagents they started
/// included, in the order they were made.
pub(super) fn calls(blocks: &[Block]) -> Vec<&ToolUse> {
    let mut out = Vec::new();
    for block in blocks {
        if let Block::Tool(call) = block {
            out.push(call);
            out.extend(calls(&call.sub));
        }
    }
    out
}

/// A number that changes when the tool calls among `blocks` (their
/// subagents' included) do: what a list made from them is kept by.
pub(super) fn weight(blocks: &[Block]) -> usize {
    let of = |block: &Block| match block {
        Block::Tool(call) => {
            1 + call.input.len()
                + usize::from(call.output.is_some())
                + 2 * usize::from(call.is_error)
                + weight(&call.sub)
        }
        _ => 0,
    };
    blocks.iter().map(of).sum()
}

/// The lines of a unified diff: added, removed, kept, or where a part of
/// the file begins. The lines before the first part that only name the
/// file are left out; within a part a line is what its first character
/// says, whatever follows it.
fn unified(diff: &str) -> Vec<(DiffKind, String)> {
    let mut part = false;
    let mut lines = Vec::new();
    for line in diff.lines() {
        if line.starts_with("@@") {
            part = true;
            lines.push((DiffKind::Note, line.to_owned()));
            continue;
        }
        let names = ["+++", "---", "diff ", "index "].iter().any(|start| line.starts_with(start));
        if (!part && names) || line.starts_with("\\ No newline") {
            continue;
        }
        lines.push(match line.as_bytes().first() {
            Some(b'+') => (DiffKind::Added, line[1..].to_owned()),
            Some(b'-') => (DiffKind::Removed, line[1..].to_owned()),
            Some(b' ') => (DiffKind::Context, line[1..].to_owned()),
            _ => (DiffKind::Context, line.to_owned()),
        });
    }
    lines
}

/// Appends to `out` how `new` differs from `old`, line by line: the lines
/// both have as context, in the order of the texts.
fn compare(old: &str, new: &str, out: &mut Vec<(DiffKind, String)>) {
    let (old, new): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..].iter().rev().zip(new[head..].iter().rev()).take_while(|(a, b)| a == b).count();
    let (gone, come) = (&old[head..old.len() - tail], &new[head..new.len() - tail]);
    let mut push = |kind: DiffKind, lines: &[&str]| out.extend(lines.iter().map(|line| (kind, (*line).to_owned())));
    push(DiffKind::Context, &old[..head]);
    if gone.len().saturating_mul(come.len()) > CELLS {
        push(DiffKind::Removed, gone);
        push(DiffKind::Added, come);
    } else {
        // The longest run of lines both keep, from each pair of places to the end.
        let width = come.len() + 1;
        let mut kept = vec![0u32; (gone.len() + 1) * width];
        for i in (0..gone.len()).rev() {
            for j in (0..come.len()).rev() {
                kept[i * width + j] = if gone[i] == come[j] {
                    kept[(i + 1) * width + j + 1] + 1
                } else {
                    kept[(i + 1) * width + j].max(kept[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < gone.len() && j < come.len() {
            if gone[i] == come[j] {
                push(DiffKind::Context, &gone[i..=i]);
                (i, j) = (i + 1, j + 1);
            } else if kept[(i + 1) * width + j] >= kept[i * width + j + 1] {
                push(DiffKind::Removed, &gone[i..=i]);
                i += 1;
            } else {
                push(DiffKind::Added, &come[j..=j]);
                j += 1;
            }
        }
        push(DiffKind::Removed, &gone[i..]);
        push(DiffKind::Added, &come[j..]);
    }
    push(DiffKind::Context, &old[old.len() - tail..]);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::doc::DiffKind::{Added, Context, Note, Removed};

    fn signs(change: &Change) -> String {
        let sign = |kind: &DiffKind| match kind {
            Added => '+',
            Removed => '-',
            Context => ' ',
            Note => '@',
        };
        change.lines.iter().map(|(kind, line)| format!("{}{line}", sign(kind))).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn a_replaced_text_shows_what_changed_among_what_it_quotes() {
        // As Claude Code's Edit puts it: the old text quotes its surroundings to find its place.
        let input = json!({
            "file_path": "/src/a.rs",
            "old_string": "fn main() {\n    let x = 1;\n    run(x);\n}",
            "new_string": "fn main() {\n    let x = 2;\n    let y = 3;\n    run(x);\n}",
        });
        let change = of("Edit", &input).expect("an edit");
        assert_eq!(signs(&change), " fn main() {\n-    let x = 1;\n+    let x = 2;\n+    let y = 3;\n     run(x);\n }");
        assert_eq!((change.added, change.removed), (2, 1));
        // An API agent's edit names the same things its own way.
        let own = of("edit", &json!({ "path": "a.rs", "old": "one\ntwo", "new": "one\n2" })).expect("an edit");
        assert_eq!(signs(&own), " one\n-two\n+2");
        // Text only taken out, or only put in.
        assert_eq!(signs(&of("Edit", &json!({ "path": "a", "old": "a\nb", "new": "a" })).unwrap()), " a\n-b");
        assert_eq!(signs(&of("Edit", &json!({ "path": "a", "old": "", "new": "x\ny" })).unwrap()), "+x\n+y");
    }

    #[test]
    fn several_edits_of_one_call_follow_each_other() {
        let input = json!({ "path": "a.txt", "old": "three", "new": "3", "edits": [
            { "old": "one", "new": "1" }, { "old_string": "keep\nfive", "new_string": "keep\n5" }, { "old": 7 },
        ]});
        let change = of("edit", &input).expect("edits");
        assert_eq!(signs(&change), "-three\n+3\n@⋯\n-one\n+1\n@⋯\n keep\n-five\n+5");
        assert_eq!((change.added, change.removed), (3, 3));
    }

    #[test]
    fn a_file_written_whole_is_all_new_and_long_ones_are_counted() {
        let change = of("Write", &json!({ "file_path": "/a.md", "content": "# Title\n\nText\n" })).expect("a write");
        assert_eq!((signs(&change).as_str(), change.added, change.removed), ("+# Title\n+\n+Text", 3, 0));
        let long = "line\n".repeat(SHOWN + 30);
        let change = of("write", &json!({ "path": "a", "content": long })).expect("a write");
        assert_eq!((change.lines.len(), change.added), (SHOWN + 1, SHOWN as u32 + 30));
        assert_eq!(change.lines.last(), Some(&(Note, "… 30 more lines".to_owned())));
    }

    #[test]
    fn codex_changes_are_read_from_their_diffs() {
        let input = json!({ "file_path": "a.rs", "changes": [
            { "path": "a.rs", "kind": "update", "diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n fn a() {\n-    old();\n+    new();\n }\n\\ No newline at end of file" },
            { "path": "b.rs", "kind": "add", "diff": "fn b() {}" },
            { "path": "c.rs", "kind": "delete", "diff": "fn c() {}" },
        ]});
        let change = of("Edit", &input).expect("changes");
        let shown =
            "@a.rs\n@@@ -1,3 +1,3 @@\n fn a() {\n-    old();\n+    new();\n }\n@b.rs\n+fn b() {}\n@c.rs\n-fn c() {}";
        assert_eq!((signs(&change).as_str(), change.added, change.removed), (shown, 2, 2));
        // One file needs no name above it: the row says which.
        let one = json!({ "changes": [{ "path": "b.rs", "kind": "add", "diff": "fn b() {}" }] });
        assert_eq!(signs(&of("Edit", &one).unwrap()), "+fn b() {}");
    }

    #[test]
    fn a_turns_edits_are_gathered_file_by_file() {
        let call = |name: &str, input: Value, output: Option<&str>, is_error: bool| ToolUse {
            name: name.into(),
            input: input.to_string(),
            output: output.map(str::to_owned),
            is_error,
            ..ToolUse::default()
        };
        let edit =
            |path: &str, old: &str, new: &str| json!({ "file_path": path, "old_string": old, "new_string": new });
        // A subagent wrote a file of its own.
        let wrote = call("Write", json!({ "file_path": "/b.rs", "content": "fn b() {}" }), Some("ok"), false);
        let agent =
            ToolUse { sub: vec![Block::Tool(wrote)], ..call("Agent", json!({ "prompt": "x" }), Some("done"), false) };
        let mut blocks = vec![
            Block::Text { text: "I'll change two files.".into() },
            Block::Tool(call("Edit", edit("/a.rs", "one", "1"), Some("ok"), false)),
            Block::Tool(agent),
            Block::Tool(call("Edit", edit("/a.rs", "two\nthree", "two\n3"), Some("ok"), false)),
            // One that failed, one still running and one that only looked changed nothing.
            Block::Tool(call("Edit", edit("/c.rs", "x", "y"), Some("not found"), true)),
            Block::Tool(call("Edit", edit("/d.rs", "x", "y"), None, false)),
            Block::Tool(call("Read", json!({ "file_path": "/e.rs" }), Some("text"), false)),
        ];
        let files = touched(calls(&blocks));
        let listed: Vec<_> = files.iter().map(|file| (file.path.as_str(), file.added, file.removed)).collect();
        assert_eq!(listed, [("/a.rs", 2, 2), ("/b.rs", 1, 0)]);
        assert_eq!(files[0].text(), "-one\n+1\n@@\n two\n-three\n+3");
        // What the list was made from is weighed: it differs once the running call has ended.
        let before = weight(&blocks);
        if let Some(Block::Tool(running)) = blocks.get_mut(5) {
            running.output = Some("ok".into());
        }
        assert_ne!(weight(&blocks), before);
        assert_eq!(touched(calls(&blocks)).len(), 3);
        assert_eq!((weight(&[]), touched(calls(&[]))), (0, Vec::new()));
    }

    #[test]
    fn a_line_of_a_diff_is_what_its_first_character_says() {
        // Past the lines that name the file, "--- x" is the removed line "-- x", not another name.
        let diff = "--- a/q.sql\n+++ b/q.sql\n@@ -1,2 +1,2 @@\n--- old\n+-- new\n keep\n\\ No newline at end of file";
        let lines = unified(diff);
        let expected = [(Note, "@@ -1,2 +1,2 @@"), (Removed, "-- old"), (Added, "-- new"), (Context, "keep")];
        assert_eq!(lines, expected.map(|(kind, line)| (kind, line.to_owned())));
        // In the window of a diff each line stands after its sign, and a part begins at "@@".
        let file = Touched { lines, ..Touched::default() };
        assert_eq!(file.text(), "@@ -1,2 +1,2 @@\n--- old\n+-- new\n keep");
    }

    #[test]
    fn what_changes_no_file_is_no_change() {
        for (name, input) in [
            ("Read", json!({ "file_path": "/a.rs" })),
            ("Bash", json!({ "command": "echo old new" })),
            // No file is named, or nothing differs, or the tool only happens to take "content".
            ("Edit", json!({ "old_string": "a", "new_string": "b" })),
            ("Edit", json!({ "file_path": "a", "old_string": "same", "new_string": "same" })),
            ("mcp__notes__post", json!({ "path": "/inbox", "content": "hello" })),
            ("Edit", json!({ "changes": [] })),
            ("Edit", json!(null)),
        ] {
            assert_eq!(of(name, &input), None, "{name} {input}");
        }
    }

    #[test]
    fn texts_too_large_to_compare_show_as_replaced() {
        let numbered = |word: &str| (0..700).map(|n| format!("{word} {n}")).collect::<Vec<_>>().join("\n");
        let (old, new) = (numbered("old"), numbered("new"));
        let mut lines = Vec::new();
        compare(&format!("same\n{old}\nend"), &format!("same\n{new}\nend"), &mut lines);
        assert_eq!(lines.len(), 1402);
        assert_eq!(
            (lines[0].0, lines[1].0, lines[700].0, lines[701].0, lines[1401].0),
            (Context, Removed, Removed, Added, Context)
        );
    }
}
