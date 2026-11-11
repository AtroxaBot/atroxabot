//! A small regular expression matcher for the `search` tool: enough of the
//! usual syntax to find code with (literals, `.`, classes, `\d \w \s \b`,
//! anchors, groups, `|`, `* + ?` and `{m,n}`), matched a line at a time.
//!
//! Written by hand rather than taken from a crate, and as a simulation of
//! every way the pattern could match at once (Thompson's construction), so
//! a hostile pattern costs time in proportion to its size times the line's
//! length and never more. There are no captures, no look-around and no
//! back-references: a pattern that uses them is refused, and the tool then
//! searches for it as plain text.

/// Most instructions a pattern may compile to.
const MAX_PROGRAM: usize = 4_000;
/// Largest count in `{m,n}`.
const MAX_COUNT: u32 = 500;
/// How deep groups may nest.
const MAX_DEPTH: usize = 50;
/// Most repetitions written one after another (`a*+?`).
const MAX_STACKED: usize = 3;
/// Longest pattern, in characters.
const MAX_LENGTH: usize = 1_000;

/// A pattern ready to be looked for in lines of text.
#[derive(Debug)]
pub(crate) struct Pattern {
    /// What a match has to get through, ending in [`Inst::Match`].
    program: Vec<Inst>,
    /// Whether letters match whatever their case.
    fold: bool,
    /// The text itself (lower-case when folding), when the pattern is nothing else.
    literal: Option<String>,
    /// The step each instruction was last entered in, so it is entered once a step.
    seen: Vec<u64>,
    /// The step the matcher is at, across all lines.
    step: u64,
    /// The instructions waiting for the character here.
    current: Vec<usize>,
    /// The instructions the last character led to.
    pending: Vec<usize>,
    /// The instructions still to be entered in this step.
    stack: Vec<usize>,
}

/// One step of a compiled pattern.
#[derive(Debug, Clone, PartialEq)]
enum Inst {
    /// This character (lower-case when folding).
    Char(char),
    /// Any character.
    Any,
    /// One of a class.
    Set(Set),
    /// The start of the line.
    Start,
    /// The end of the line.
    End,
    /// A word boundary (`true`), or anywhere that is none.
    Word(bool),
    /// Go on at both.
    Split(usize, usize),
    /// Go on there.
    Jump(usize),
    /// The pattern matched.
    Match,
}

/// What a pattern is made of, as it was written.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Char(char),
    Any,
    Set(Set),
    Start,
    End,
    Word(bool),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat { node: Box<Node>, min: u32, max: Option<u32> },
}

/// A character class, `[a-z\d]`.
#[derive(Debug, Clone, PartialEq)]
struct Set {
    /// Whether it matches what is not in it (`[^…]`).
    negated: bool,
    items: Vec<Item>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Item {
    /// The characters from the first to the last.
    Range(char, char),
    /// A kind of character, or (`true`) all that are not of it.
    Kind(Kind, bool),
}

/// The kinds `\d`, `\w` and `\s` stand for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Digit,
    Word,
    Space,
}

/// What follows a backslash.
enum Escaped {
    Char(char),
    Kind(Kind, bool),
    Word(bool),
}

impl Kind {
    fn has(self, c: char) -> bool {
        match self {
            Self::Digit => c.is_ascii_digit(),
            Self::Word => is_word(Some(c)),
            Self::Space => c.is_whitespace(),
        }
    }
}

impl Set {
    fn has(&self, c: char, fold: bool) -> bool {
        let holds = |c: char| {
            self.items.iter().any(|item| match *item {
                Item::Range(low, high) => low <= c && c <= high,
                Item::Kind(kind, negated) => kind.has(c) != negated,
            })
        };
        let found = holds(c) || (fold && (holds(lower(c)) || holds(upper(c))));
        found != self.negated
    }
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

fn lower(c: char) -> char {
    if c.is_ascii() { c.to_ascii_lowercase() } else { c.to_lowercase().next().unwrap_or(c) }
}

fn upper(c: char) -> char {
    if c.is_ascii() { c.to_ascii_uppercase() } else { c.to_uppercase().next().unwrap_or(c) }
}

impl Pattern {
    /// Reads `pattern` as a regular expression. Letters match whatever
    /// their case unless the pattern has a capital letter of its own, or
    /// starts with `(?i)`.
    ///
    /// # Errors
    /// What about it this matcher does not read, as a few words.
    pub(crate) fn new(pattern: &str) -> Result<Self, String> {
        if pattern.chars().count() > MAX_LENGTH {
            return Err("it is too long".to_owned());
        }
        let mut parser = Parser { chars: pattern.chars().collect(), at: 0, depth: 0, fold: false };
        let tree = parser.alternation()?;
        if parser.at < parser.chars.len() {
            return Err("a `)` closes nothing".to_owned());
        }
        let fold = parser.fold || !has_capital(&tree);
        let literal = literal(&tree).map(|text| if fold { text.to_lowercase() } else { text });
        let mut program = Vec::new();
        compile(&tree, &mut program, fold)?;
        emit(&mut program, Inst::Match)?;
        Ok(Self::with(program, fold, literal))
    }

    /// `text` itself as the pattern, with the same rule for capitals.
    pub(crate) fn plain(text: &str) -> Self {
        let fold = !text.chars().any(char::is_uppercase);
        Self::with(vec![Inst::Match], fold, Some(if fold { text.to_lowercase() } else { text.to_owned() }))
    }

    fn with(program: Vec<Inst>, fold: bool, literal: Option<String>) -> Self {
        let seen = vec![0; program.len()];
        Self { program, fold, literal, seen, step: 0, current: Vec::new(), pending: Vec::new(), stack: Vec::new() }
    }

    /// Whether the pattern matches anywhere in `line`.
    pub(crate) fn is_match(&mut self, line: &str) -> bool {
        if let Some(literal) = &self.literal {
            return if self.fold {
                line.to_lowercase().contains(literal.as_str())
            } else {
                line.contains(literal.as_str())
            };
        }
        let mut chars = line.chars();
        let mut before = None;
        self.pending.clear();
        loop {
            let here = chars.next();
            self.step += 1;
            self.current.clear();
            // Every way to be here: what the last character led to, and a fresh start.
            self.stack.append(&mut self.pending);
            self.stack.push(0);
            while let Some(at) = self.stack.pop() {
                let (Some(inst), Some(seen)) = (self.program.get(at), self.seen.get_mut(at)) else { continue };
                if std::mem::replace(seen, self.step) == self.step {
                    continue;
                }
                match *inst {
                    Inst::Match => {
                        self.stack.clear();
                        return true;
                    }
                    Inst::Jump(to) => self.stack.push(to),
                    Inst::Split(one, other) => self.stack.extend([one, other]),
                    Inst::Start if before.is_none() => self.stack.push(at + 1),
                    Inst::End if here.is_none() => self.stack.push(at + 1),
                    Inst::Word(wanted) if (is_word(before) != is_word(here)) == wanted => self.stack.push(at + 1),
                    Inst::Start | Inst::End | Inst::Word(_) => {}
                    Inst::Char(_) | Inst::Any | Inst::Set(_) => self.current.push(at),
                }
            }
            let Some(c) = here else { return false };
            let folded = if self.fold { lower(c) } else { c };
            for &at in &self.current {
                let fits = match self.program.get(at) {
                    Some(Inst::Char(wanted)) => *wanted == folded,
                    Some(Inst::Any) => true,
                    Some(Inst::Set(set)) => set.has(c, self.fold),
                    _ => false,
                };
                if fits {
                    self.pending.push(at + 1);
                }
            }
            before = Some(c);
        }
    }
}

/// Whether the pattern asks for a capital letter anywhere.
fn has_capital(node: &Node) -> bool {
    match node {
        Node::Char(c) => c.is_uppercase(),
        Node::Set(set) => set.items.iter().any(|item| match *item {
            Item::Range(low, high) => low.is_uppercase() || high.is_uppercase(),
            Item::Kind(..) => false,
        }),
        Node::Cat(nodes) | Node::Alt(nodes) => nodes.iter().any(has_capital),
        Node::Repeat { node, .. } => has_capital(node),
        Node::Any | Node::Start | Node::End | Node::Word(_) => false,
    }
}

/// The pattern's text, when it is plain characters and nothing else.
fn literal(node: &Node) -> Option<String> {
    let Node::Cat(nodes) = node else { return None };
    if nodes.is_empty() {
        return None;
    }
    nodes.iter().map(|node| if let Node::Char(c) = node { Some(*c) } else { None }).collect()
}

fn emit(program: &mut Vec<Inst>, inst: Inst) -> Result<usize, String> {
    if program.len() >= MAX_PROGRAM {
        return Err("it repeats too much".to_owned());
    }
    program.push(inst);
    Ok(program.len() - 1)
}

/// Points the placeholder at `at` to where it goes.
fn patch(program: &mut [Inst], at: usize, inst: Inst) {
    if let Some(slot) = program.get_mut(at) {
        *slot = inst;
    }
}

fn compile(node: &Node, program: &mut Vec<Inst>, fold: bool) -> Result<(), String> {
    match node {
        Node::Char(c) => emit(program, Inst::Char(if fold { lower(*c) } else { *c })).map(drop),
        Node::Any => emit(program, Inst::Any).map(drop),
        Node::Set(set) => emit(program, Inst::Set(set.clone())).map(drop),
        Node::Start => emit(program, Inst::Start).map(drop),
        Node::End => emit(program, Inst::End).map(drop),
        Node::Word(wanted) => emit(program, Inst::Word(*wanted)).map(drop),
        Node::Cat(nodes) => nodes.iter().try_for_each(|node| compile(node, program, fold)),
        Node::Alt(branches) => {
            let mut jumps = Vec::new();
            for (index, branch) in branches.iter().enumerate() {
                if index + 1 == branches.len() {
                    compile(branch, program, fold)?;
                } else {
                    let split = emit(program, Inst::Split(0, 0))?;
                    compile(branch, program, fold)?;
                    jumps.push(emit(program, Inst::Jump(0))?);
                    let next = program.len();
                    patch(program, split, Inst::Split(split + 1, next));
                }
            }
            let end = program.len();
            for jump in jumps {
                patch(program, jump, Inst::Jump(end));
            }
            Ok(())
        }
        Node::Repeat { node, min, max } => {
            for _ in 0..*min {
                compile(node, program, fold)?;
            }
            if let Some(max) = max {
                // Each further one may be left out, and with it all that follow.
                let mut splits = Vec::new();
                for _ in *min..*max {
                    splits.push(emit(program, Inst::Split(0, 0))?);
                    compile(node, program, fold)?;
                }
                let end = program.len();
                for split in splits {
                    patch(program, split, Inst::Split(split + 1, end));
                }
            } else {
                let split = emit(program, Inst::Split(0, 0))?;
                compile(node, program, fold)?;
                emit(program, Inst::Jump(split))?;
                let end = program.len();
                patch(program, split, Inst::Split(split + 1, end));
            }
            Ok(())
        }
    }
}

/// Reads a pattern, a character at a time.
struct Parser {
    chars: Vec<char>,
    at: usize,
    depth: usize,
    /// Whether the pattern said `(?i)`.
    fold: bool,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += 1;
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        let found = self.peek() == Some(c);
        self.at += usize::from(found);
        found
    }

    fn alternation(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.sequence()?];
        while self.eat('|') {
            branches.push(self.sequence()?);
        }
        Ok(match <[Node; 1]>::try_from(branches) {
            Ok([only]) => only,
            Err(branches) => Node::Alt(branches),
        })
    }

    fn sequence(&mut self) -> Result<Node, String> {
        let mut parts = Vec::new();
        while self.peek().is_some_and(|c| c != '|' && c != ')') {
            let atom = self.atom()?;
            parts.push(self.repeated(atom)?);
        }
        Ok(Node::Cat(parts))
    }

    fn atom(&mut self) -> Result<Node, String> {
        Ok(match self.next() {
            Some('.') => Node::Any,
            Some('^') => Node::Start,
            Some('$') => Node::End,
            Some('(') => self.group()?,
            Some('[') => self.set()?,
            Some('\\') => match self.escape()? {
                Escaped::Char(c) => Node::Char(c),
                Escaped::Kind(kind, negated) => {
                    Node::Set(Set { negated: false, items: vec![Item::Kind(kind, negated)] })
                }
                Escaped::Word(wanted) => Node::Word(wanted),
            },
            Some('*' | '+' | '?') => return Err("a repetition repeats nothing".to_owned()),
            // Braces and a stray `]` are themselves, as code is full of them.
            Some(c) => Node::Char(c),
            None => Node::Cat(Vec::new()),
        })
    }

    /// `node` with the repetitions written after it.
    fn repeated(&mut self, mut node: Node) -> Result<Node, String> {
        for _ in 0..=MAX_STACKED {
            let (min, max) = match self.peek() {
                Some('*') => (0, None),
                Some('+') => (1, None),
                Some('?') => (0, Some(1)),
                Some('{') => match self.counts() {
                    Some(counts) => counts,
                    None => return Ok(node),
                },
                _ => return Ok(node),
            };
            // Past the `*`, `+` or `?`, or the brace that closes a count.
            self.at += 1;
            // Lazy or greedy, the same lines match.
            self.eat('?');
            if min > MAX_COUNT || max.is_some_and(|max| max > MAX_COUNT || max < min) {
                return Err("a count is out of range".to_owned());
            }
            node = Node::Repeat { node: Box::new(node), min, max };
        }
        Err("too many repetitions in a row".to_owned())
    }

    /// The counts of a `{m}`, `{m,}` or `{m,n}` here, leaving the parser
    /// before its closing brace; `None`, and nothing read, when the brace
    /// opens no such thing.
    fn counts(&mut self) -> Option<(u32, Option<u32>)> {
        let start = self.at;
        self.at += 1;
        let read = (|| {
            let min = self.number()?;
            let max = if self.eat(',') {
                if self.peek() == Some('}') { None } else { Some(self.number()?) }
            } else {
                Some(min)
            };
            (self.peek() == Some('}')).then_some((min, max))
        })();
        if read.is_none() {
            self.at = start;
        }
        read
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.at += 1;
        }
        let digits: String = self.chars.get(start..self.at)?.iter().collect();
        // One too long to be a number is out of range, which the caller says.
        (!digits.is_empty()).then(|| digits.parse().unwrap_or(u32::MAX))
    }

    fn group(&mut self) -> Result<Node, String> {
        if self.eat('?') {
            match self.next() {
                Some(':') => {}
                Some('i') if self.eat(')') => {
                    self.fold = true;
                    return Ok(Node::Cat(Vec::new()));
                }
                // A named group: the name is of no use here.
                Some('P' | '<') if !matches!(self.peek(), Some('=' | '!')) => {
                    self.eat('<');
                    while self.next().is_some_and(|c| c != '>') {}
                }
                _ => return Err("look-around and flags are not supported".to_owned()),
            }
        }
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("its groups nest too deep".to_owned());
        }
        let inner = self.alternation()?;
        self.depth -= 1;
        if self.eat(')') { Ok(inner) } else { Err("a `(` is not closed".to_owned()) }
    }

    fn escape(&mut self) -> Result<Escaped, String> {
        Ok(match self.next() {
            None => return Err("it ends in a `\\`".to_owned()),
            Some('d') => Escaped::Kind(Kind::Digit, false),
            Some('D') => Escaped::Kind(Kind::Digit, true),
            Some('w') => Escaped::Kind(Kind::Word, false),
            Some('W') => Escaped::Kind(Kind::Word, true),
            Some('s') => Escaped::Kind(Kind::Space, false),
            Some('S') => Escaped::Kind(Kind::Space, true),
            Some('b') => Escaped::Word(true),
            Some('B') => Escaped::Word(false),
            Some('n') => Escaped::Char('\n'),
            Some('t') => Escaped::Char('\t'),
            Some('r') => Escaped::Char('\r'),
            Some(c) if c.is_alphanumeric() => return Err(format!("`\\{c}` is not supported")),
            Some(c) => Escaped::Char(c),
        })
    }

    fn set(&mut self) -> Result<Node, String> {
        let open = || "a `[` is not closed".to_owned();
        let negated = self.eat('^');
        let mut items = Vec::new();
        loop {
            let c = self.next().ok_or_else(open)?;
            // A `]` right after the opening is one of the class.
            if c == ']' && !items.is_empty() {
                break;
            }
            let low = match c {
                '\\' => match self.escape()? {
                    Escaped::Char(c) => c,
                    Escaped::Kind(kind, negated) => {
                        items.push(Item::Kind(kind, negated));
                        continue;
                    }
                    Escaped::Word(_) => return Err("`\\b` in a class is not supported".to_owned()),
                },
                c => c,
            };
            let ranged = self.peek() == Some('-') && self.chars.get(self.at + 1).is_some_and(|c| *c != ']');
            let high = if ranged {
                self.at += 1;
                match self.next().ok_or_else(open)? {
                    '\\' => match self.escape()? {
                        Escaped::Char(c) => c,
                        _ => return Err("a range ends in a class".to_owned()),
                    },
                    c => c,
                }
            } else {
                low
            };
            if high < low {
                return Err("a range is out of order".to_owned());
            }
            items.push(Item::Range(low, high));
        }
        Ok(Node::Set(Set { negated, items }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, line: &str) -> bool {
        Pattern::new(pattern).unwrap_or_else(|e| panic!("{pattern}: {e}")).is_match(line)
    }

    #[test]
    fn finds_what_code_is_searched_for() {
        assert!(matches("hello", "say Hello") && !matches("Hello", "say hello"), "capitals decide the case");
        assert!(matches("(?i)Hello", "say hello") && matches("[A-Z]x", "Zx") && !matches("[A-Z]x", "zx"));
        assert!(matches(r"fn \w+\(", "pub fn run(a: u8)") && !matches(r"fn \w+\(", "fn (a)"));
        assert!(matches("foo|bar", "a bar") && matches("^(foo|ba+r)$", "baaar") && !matches("^(foo|bar)$", "a bar"));
        assert!(matches(r"\bcat\b", "a cat.") && !matches(r"\bcat\b", "concat") && matches(r"\Bcat", "concat"));
        assert!(matches(r"^\s*use ", "    use std;") && !matches(r"^use ", "  use std;") && matches("a.c", "abc"));
        assert!(matches(r"\d{3}-\d{2,}", "tel 555-1234") && !matches(r"^\d{3}$", "5555") && matches("a{2,3}", "caab"));
        assert!(matches(r"[^\s\d]+\.rs$", "main.rs") && matches("[]x]", "]") && matches("[a-]", "-"));
        assert!(matches("colou?r", "color") && matches("a*?b", "aab") && matches("(?:ab)+c", "ababc"));
        assert!(matches("(?P<name>x)y", "xy") && matches("(?<name>x)y", "xy") && matches("é+", "caféé"));
        // Braces that are no count are themselves, as in code.
        assert!(matches("struct Foo {", "pub struct Foo {") && matches("a{,2}", "a{,2}") && matches("x}", "x}"));
        assert!(matches("", "anything") && matches("^$", "") && !matches("^$", "x"));
    }

    #[test]
    fn plain_text_is_looked_for_as_it_is() {
        assert!(Pattern::plain("foo(").is_match("call FOO(1)") && !Pattern::plain("Foo(").is_match("foo(1)"));
        assert!(Pattern::new("needle").unwrap().literal.is_some() && Pattern::new("ne+dle").unwrap().literal.is_none());
    }

    #[test]
    fn refuses_what_it_does_not_read() {
        for bad in
            ["foo(", "a)", "[abc", "*a", r"\p{L}", "(?=x)", "(?<!x)y", r"a\", "[z-a]", "a{600}", "a{3,2}", "a*+*+"]
        {
            assert!(Pattern::new(bad).is_err(), "{bad}");
        }
        assert!(Pattern::new(&"(".repeat(60)).is_err() && Pattern::new(&"a".repeat(1_001)).is_err());
    }

    #[test]
    fn hostile_patterns_stay_bounded() {
        assert!(Pattern::new("((a{400}){400}){400}").is_err(), "too large to build");
        // Backtracking would never finish these.
        let line = "a".repeat(5_000);
        assert!(!matches("(a*)*b", &line) && !matches("(a|aa)+$b", &line) && !matches("(a+)+c", &line));
        assert!(matches("(a*)*$", &line) && matches("(|a)*", &line));
    }
}
