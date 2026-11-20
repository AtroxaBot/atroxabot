//! What a frame of the shell costs, measured: the time to build it, what it allocates, how many times
//! over it covers the window, and what the GPU needs for it. Run by hand, in release:
//! `cargo test --release -p atroxabot costs -- --ignored --nocapture --test-threads=1`
//!
//! The sessions are made up, in the shapes of real ones: a short one, a day's work, and one whose
//! subagents made its file tens of megabytes. Every test of the crate runs under the allocator that
//! counts here; it only counts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use atroxabot_core::{AgentEvent, ToolUse, Usage, unix_now};

use super::Shell;
use super::snapshot::{repository, shell};
use super::thread::Thread;
use crate::gpu::{Instance, Offscreen};
use crate::image::ImageAtlas;
use crate::paint::{Painter, Rect};
use crate::text::{Fonts, GlyphAtlas};
use crate::theme::Scheme;
use crate::ui::Ui;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);

// SAFETY: every call goes to the system's allocator as it came; the counters only watch.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size() as u64, Relaxed);
        LIVE.fetch_add(layout.size() as u64, Relaxed);
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as u64, Relaxed);
        // SAFETY: as above.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(size as u64, Relaxed);
        LIVE.fetch_add(size as u64, Relaxed);
        LIVE.fetch_sub(layout.size() as u64, Relaxed);
        // SAFETY: as above.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn live_mb() -> f64 {
    LIVE.load(Relaxed) as f64 / 1e6
}

fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
}

fn words(length: usize, seed: usize) -> String {
    const WORDS: [&str; 24] = [
        "the", "parser", "reads", "tokens", "until", "a", "quote", "ends", "string", "and", "then", "checks", "escape",
        "branch", "before", "it", "moves", "on", "to", "next", "line", "of", "input", "here",
    ];
    let mut out = String::with_capacity(length + 8);
    let mut at = seed;
    while out.len() < length {
        out.push_str(WORDS[at % WORDS.len()]);
        out.push(' ');
        at = at.wrapping_mul(31).wrapping_add(7);
    }
    out
}

fn code(lines: usize, seed: usize) -> String {
    let mut out = String::new();
    for line in 0..lines {
        let about = words(18, line + seed);
        let _ = writeln!(out, "    let value_{} = compute(input, {line}) + offset; // {about}", line + seed);
    }
    out
}

fn answer(seed: usize) -> String {
    format!(
        "{}\n\n- {}\n- {}\n- {}\n\n```rust\n{}```\n\n{} **{}** and `cargo test`.\n",
        words(420, seed),
        words(70, seed + 1),
        words(70, seed + 2),
        words(70, seed + 3),
        code(12, seed),
        words(300, seed + 4),
        words(20, seed + 5)
    )
}

fn tool_input(kind: usize) -> (&'static str, String) {
    let file = format!("C:\\code\\parser\\src\\module_{kind}.rs");
    match kind % 5 {
        0 => {
            let command = format!("cargo test -p parser {} -- --nocapture", words(60, kind));
            ("Bash", serde_json::json!({ "command": command, "description": words(40, kind) }).to_string())
        }
        1 => ("Read", serde_json::json!({ "file_path": file }).to_string()),
        2 => ("Grep", serde_json::json!({ "pattern": "fn \\w+", "path": file, "output_mode": "content" }).to_string()),
        3 => (
            "Edit",
            serde_json::json!({ "file_path": file, "old_string": code(14, kind), "new_string": code(16, kind + 1) })
                .to_string(),
        ),
        _ => ("Write", serde_json::json!({ "file_path": file, "content": code(60, kind) }).to_string()),
    }
}

/// A session to measure with: how many turns, replies a turn, tool calls a reply, and subagents a
/// turn with their own tool calls.
#[derive(Clone, Copy)]
pub(super) struct Shape {
    name: &'static str,
    turns: usize,
    replies: usize,
    tools: usize,
    agents: usize,
    sub_tools: usize,
}

/// Adds `shape` to the open session of `shell`: turns of replies with reasoning, tool calls and subagents.
pub(super) fn fill(shell: &mut Shell, shape: Shape) {
    let thread = shell.thread_mut().expect("the open session");
    let session = &mut thread.session;
    let mut n = 0usize;
    for turn in 0..shape.turns {
        session.prompt(&words(160, turn), "");
        for reply in 0..shape.replies {
            session.apply(&AgentEvent::ReplyStarted);
            for tool in 0..shape.tools {
                n += 1;
                if tool % 2 == 0 {
                    session.apply(&AgentEvent::Thinking(words(900, n)));
                }
                if tool % 6 == 0 {
                    session.apply(&AgentEvent::Text(words(260, n)));
                }
                let (name, input) = tool_input(n);
                let id = format!("t{n}");
                session.apply(&AgentEvent::ToolCall(ToolUse {
                    id: id.clone(),
                    name: name.into(),
                    input,
                    ..ToolUse::default()
                }));
                session.apply(&AgentEvent::ToolResult { id, output: words(2400, n), is_error: n.is_multiple_of(23) });
            }
            if reply == 0 {
                for _ in 0..shape.agents {
                    n += 1;
                    let id = format!("a{n}");
                    let input = serde_json::json!({
                        "subagent_type": "Explore",
                        "description": words(40, n),
                        "prompt": words(1500, n),
                    });
                    session.apply(&AgentEvent::ToolCall(ToolUse {
                        id: id.clone(),
                        name: "Agent".into(),
                        input: input.to_string(),
                        ..ToolUse::default()
                    }));
                    for sub_tool in 0..shape.sub_tools {
                        n += 1;
                        let sub = |event: AgentEvent| AgentEvent::Sub { parent: id.clone(), event: Box::new(event) };
                        if sub_tool % 2 == 0 {
                            session.apply(&sub(AgentEvent::Thinking(words(1000, n))));
                        }
                        let (name, input) = tool_input(n);
                        session.apply(&sub(AgentEvent::ToolCall(ToolUse {
                            id: format!("s{n}"),
                            name: name.into(),
                            input,
                            output: Some(words(2500, n)),
                            ..ToolUse::default()
                        })));
                    }
                    session.apply(&AgentEvent::ToolResult { id, output: words(1200, n), is_error: false });
                }
            }
            if reply + 1 == shape.replies {
                session.apply(&AgentEvent::Text(answer(n)));
            }
            session.apply(&AgentEvent::ReplyEnded {
                raw: Vec::new(),
                wire: atroxabot_core::Wire::default(),
                model: "claude-opus-5-5".into(),
                usage: Usage {
                    input_tokens: 48_210,
                    output_tokens: 1_204,
                    cached_tokens: 44_100,
                    cache_write_tokens: 0,
                },
                cost: 0.18,
                problem: String::new(),
            });
        }
        session.apply(&AgentEvent::TurnEnded { problem: None });
    }
}

/// A few turns.
const SHORT: Shape = Shape { name: "a short session", turns: 3, replies: 2, tools: 4, agents: 0, sub_tools: 0 };
/// A day's work: a file of a few megabytes.
pub(super) const DAY: Shape =
    Shape { name: "a day's session", turns: 9, replies: 5, tools: 8, agents: 1, sub_tools: 17 };
/// Waves of subagents: a file of tens of megabytes.
const HEAVY: Shape =
    Shape { name: "a session with many subagents", turns: 30, replies: 4, tools: 8, agents: 3, sub_tools: 100 };

struct Rig {
    fonts: Fonts,
    atlas: GlyphAtlas,
    images: ImageAtlas,
    instances: Vec<Instance>,
    ui: Ui,
    time: f32,
    view: Rect,
}

impl Rig {
    fn new() -> Self {
        let mut ui = Ui::default();
        (ui.focused, ui.mouse) = (true, (-1.0, -1.0));
        Self {
            fonts: Fonts::load(),
            atlas: GlyphAtlas::default(),
            images: ImageAtlas::default(),
            instances: Vec::new(),
            ui,
            time: 10.0,
            view: Rect::new(0.0, 0.0, 1920.0, 1040.0),
        }
    }

    /// One frame as the app builds it, without the GPU: what is due, then the whole interface.
    fn frame(&mut self, shell: &mut Shell, dt: f32) -> Duration {
        self.time += dt;
        self.ui.begin(dt, self.time);
        let mut actions = Vec::new();
        let started = Instant::now();
        shell.tick(&mut actions);
        {
            let mut painter = Painter::new(
                &self.fonts,
                (&mut self.atlas, &mut self.images),
                &mut self.instances,
                1.0,
                self.view,
                Scheme::Black.palette(),
            );
            shell.draw(&mut painter, &mut self.ui, self.view, Scheme::Black, &mut actions);
            crate::titlebar::draw(&mut painter, &mut self.ui, self.view, false);
        }
        let took = started.elapsed();
        self.ui.end();
        took
    }

    /// How many quads the last frame drew, how many of them glyphs, and how many times over they cover the window.
    fn drawn(&self) -> (usize, usize, f64) {
        let (mut glyphs, mut area) = (0, 0.0f64);
        for quad in &self.instances {
            let glyph = (quad.params[3] - 1.0).abs() < 0.5;
            glyphs += usize::from(glyph);
            area += shaded(quad, self.view);
        }
        (self.instances.len(), glyphs, area / f64::from(self.view.w * self.view.h))
    }

    /// The largest quads of the last frame, as a share of the window each.
    fn largest(&self, most: usize) -> Vec<String> {
        let whole = f64::from(self.view.w * self.view.h);
        let mut quads: Vec<(f64, &Instance)> =
            self.instances.iter().map(|quad| (shaded(quad, self.view), quad)).collect();
        quads.sort_by(|a, b| b.0.total_cmp(&a.0));
        quads
            .iter()
            .take(most)
            .map(|(area, quad)| {
                format!(
                    "{:4.0}% kind {} blur {} alpha {:.2}",
                    100.0 * area / whole,
                    quad.params[3],
                    quad.params[2],
                    quad.color[3].max(quad.color2[3])
                )
            })
            .collect()
    }
}

/// The pixels of `quad` the fragment shader colours: its rectangle with the room for blur, inside its clip.
fn shaded(quad: &Instance, view: Rect) -> f64 {
    let glyph = (quad.params[3] - 1.0).abs() < 0.5;
    let pad = if glyph { 0.0 } else { quad.params[2] + 1.0 };
    let (left, top) =
        ((quad.rect[0] - pad).max(quad.clip[0]).max(0.0), (quad.rect[1] - pad).max(quad.clip[1]).max(0.0));
    let right = (quad.rect[0] + quad.rect[2] + pad).min(quad.clip[2]).min(view.w);
    let bottom = (quad.rect[1] + quad.rect[3] + pad).min(quad.clip[3]).min(view.h);
    if right > left && bottom > top { f64::from(right - left) * f64::from(bottom - top) } else { 0.0 }
}

/// Runs `frames` frames, `before` each doing what happens between two (its time is told apart), and says what they cost.
fn run(
    name: &str,
    rig: &mut Rig,
    shell: &mut Shell,
    frames: usize,
    mut before: impl FnMut(&mut Rig, &mut Shell, usize) -> Duration,
) {
    let (mut times, mut events) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
    let (allocs, bytes) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    for index in 0..frames {
        events.push(before(rig, shell, index));
        times.push(rig.frame(shell, 1.0 / 82.5));
    }
    let allocs = (ALLOCS.load(Relaxed) - allocs) / frames as u64;
    let kilobytes = (BYTES.load(Relaxed) - bytes) as f64 / 1e3 / frames as f64;
    times.sort();
    events.sort();
    let at = |series: &[Duration], share: f64| ms(series[((series.len() - 1) as f64 * share) as usize]);
    let (quads, glyphs, cover) = rig.drawn();
    println!(
        "  {name:<40} frame {:7.3} ms  p95 {:7.3}  max {:7.3} | event {:6.3} max {:7.3} | {allocs:6} allocs {kilobytes:9.1} KB/frame | {quads:5} quads ({glyphs} glyphs) cover {cover:.2}x",
        at(&times, 0.5),
        at(&times, 0.95),
        at(&times, 1.0),
        at(&events, 0.5),
        at(&events, 1.0),
    );
}

const NOTHING: fn(&mut Rig, &mut Shell, usize) -> Duration = |_, _, _| Duration::ZERO;

fn one(shape: Shape, gpu: &mut Option<Offscreen>) {
    let before = live_mb();
    let mut shell = shell();
    let id = shell.current.clone().expect("an open session");
    fill(&mut shell, shape);
    let data = live_mb() - before;
    let session = &shell.threads[&id].session;
    let tools: usize = session
        .entries
        .iter()
        .map(|entry| match entry {
            atroxabot_core::Entry::Reply(reply) => reply.blocks.len(),
            _ => 0,
        })
        .sum();
    let started = Instant::now();
    let json = serde_json::to_vec(session).map_or(0, |bytes| bytes.len());
    let written = started.elapsed();
    let started = Instant::now();
    let copy = session.clone();
    let cloned = started.elapsed();
    drop(copy);
    let started = Instant::now();
    let summary = session.summary();
    let summed = started.elapsed();
    drop(summary);
    println!(
        "\n== {}: {} entries, {} blocks, {:.1} MB as a file, {:.1} MB in memory",
        shape.name,
        session.entries.len(),
        tools,
        json as f64 / 1e6,
        data
    );
    println!(
        "  a save: copying the session {:.2} ms (on the UI thread), writing it as JSON {:.2} ms (writer thread); its summary {:.3} ms",
        ms(cloned),
        ms(written),
        ms(summed)
    );
    let mut rig = Rig::new();
    let before = live_mb();
    // Opened: frames that only measure until it can be shown at its end, then the rest of it is
    // measured a little a frame.
    let mut until = |done: fn(&Thread) -> bool| {
        let (mut frames, mut all, mut longest) = (0, Duration::ZERO, Duration::ZERO);
        while !shell.thread().is_some_and(done) && frames < 5000 {
            let took = rig.frame(&mut shell, 1.0 / 165.0);
            (frames, all, longest) = (frames + 1, all + took, longest.max(took));
        }
        (frames, ms(all), ms(longest))
    };
    let (hidden, opening, longest) = until(Thread::shown);
    let (more, rest, slowest) = until(Thread::measured);
    println!(
        "  opening it: shown after {hidden} frames ({opening:.1} ms in all, the longest {longest:.1} ms); the rest          measured over {more} more frames ({rest:.1} ms in all, the longest {slowest:.1} ms); its layouts hold {:.1} MB",
        live_mb() - before
    );
    for _ in 0..6 {
        rig.frame(&mut shell, 1.0);
    }
    run("idle, mouse away", &mut rig, &mut shell, 300, NOTHING);
    if shape.name == DAY.name {
        println!("  largest quads: {:?}", rig.largest(9));
        if let Some(gpu) = gpu {
            let clear = Scheme::Black.palette().panel;
            let size = (1920, 1040);
            let (handed, done) =
                gpu.bench(size, &rig.instances, (&mut rig.atlas.uploads, &mut rig.images.uploads), 1.0, clear, 600);
            let (_, empty) = gpu.bench(size, &[], (&mut Vec::new(), &mut Vec::new()), 1.0, clear, 600);
            println!(
                "  GPU flat out, 1920x1040: {:.3} ms a frame (handing over {:.3} ms), an empty frame {:.3} ms",
                ms(done) / 600.0,
                ms(handed) / 600.0,
                ms(empty) / 600.0
            );
        }
    }
    run("mouse moving over the conversation", &mut rig, &mut shell, 300, |rig, _, index| {
        rig.ui.mouse = (700.0 + (index % 100) as f32 * 3.0, 300.0 + (index % 50) as f32 * 8.0);
        Duration::ZERO
    });
    run("wheel up through the conversation", &mut rig, &mut shell, 300, |rig, _, _| {
        rig.ui.mouse = (960.0, 500.0);
        rig.ui.scroll = -240.0;
        Duration::ZERO
    });
    run("wheel back down", &mut rig, &mut shell, 300, |rig, _, _| {
        rig.ui.scroll = 4000.0;
        Duration::ZERO
    });
    rig.ui.mouse = (-1.0, -1.0);
    for _ in 0..6 {
        rig.frame(&mut shell, 1.0);
    }
    println!("  after scrolling through all of it the layouts hold {:.1} MB", live_mb() - before);

    // A turn: it works, streams an answer a few words at a time, and ends.
    let mut actions = Vec::new();
    shell.agent_event(&id, AgentEvent::Working, &mut actions);
    run("working, nothing arriving", &mut rig, &mut shell, 300, NOTHING);
    shell.agent_event(&id, AgentEvent::ReplyStarted, &mut actions);
    let stream = |_: &mut Rig, shell: &mut Shell, index: usize| {
        let mut actions = Vec::new();
        let id = shell.current.clone().unwrap_or_default();
        let started = Instant::now();
        shell.agent_event(&id, AgentEvent::Text(words(24, index)), &mut actions);
        started.elapsed()
    };
    run("an answer streaming in (to 7 KB)", &mut rig, &mut shell, 300, stream);
    shell.agent_event(&id, AgentEvent::Text(answer(1).repeat(12)), &mut actions);
    rig.frame(&mut shell, 1.0);
    run("streaming on after 30 KB of answer", &mut rig, &mut shell, 300, stream);
    let call = |n: usize| {
        let (name, input) = tool_input(n);
        ToolUse { id: format!("live{n}"), name: name.into(), input, ..ToolUse::default() }
    };
    run("tool calls and their results arriving", &mut rig, &mut shell, 200, |_, shell, index| {
        let mut actions = Vec::new();
        let id = shell.current.clone().unwrap_or_default();
        let started = Instant::now();
        if index % 2 == 0 {
            shell.agent_event(&id, AgentEvent::ToolCall(call(index)), &mut actions);
        } else {
            let result = AgentEvent::ToolResult {
                id: format!("live{}", index - 1),
                output: words(2400, index),
                is_error: false,
            };
            shell.agent_event(&id, result, &mut actions);
        }
        started.elapsed()
    });
    let started = Instant::now();
    shell.agent_event(&id, AgentEvent::TurnEnded { problem: None }, &mut actions);
    println!("  the turn ending, on the UI thread: {:.2} ms ({} actions)", ms(started.elapsed()), actions.len());
    drop(actions);

    shell.config.panel = true;
    shell.git.folder = shell.work_folder();
    shell.git.status = Some(repository());
    for _ in 0..6 {
        rig.frame(&mut shell, 1.0);
    }
    run("idle, side panel open", &mut rig, &mut shell, 300, NOTHING);
    shell.config.panel = false;
    let agent = shell.threads[&id].session.agent.clone();
    for index in 0..300 {
        shell.sessions.push(atroxabot_core::SessionSummary {
            id: format!("many{index}"),
            agent: agent.clone(),
            title: words(40, index),
            preview: words(80, index),
            updated: unix_now() - index as u64 * 600,
            ..atroxabot_core::SessionSummary::default()
        });
    }
    for _ in 0..6 {
        rig.frame(&mut shell, 1.0);
    }
    run("idle, 300 sessions in the sidebar", &mut rig, &mut shell, 300, NOTHING);
    // Left alone long enough, it gives its messages back (`warm.rs`).
    let held = live_mb();
    drop(shell.threads.remove(&id));
    println!("  let go of, it gives back {:.1} MB", held - live_mb());
}

#[test]
#[ignore = "measures; run by hand, in release"]
fn costs() {
    let mut gpu = Offscreen::new();
    for shape in [SHORT, DAY, HEAVY] {
        one(shape, &mut gpu);
    }
}
