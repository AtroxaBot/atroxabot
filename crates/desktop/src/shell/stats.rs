//! The usage statistics, a modal: what was used over a period, where the
//! accounts' plan limits stood when last reported, a year of activity as
//! a grid of days, the period day by day, where the tokens went, and who
//! and what used them (providers, models, agents, tools).

#![expect(clippy::many_single_char_names, reason = "p, t, x, y and w are the drawing code's names everywhere")]

use std::collections::BTreeMap;

use atroxabot_core::stats::{Ledger, Summary, Tally, civil, day_of, weekday};
use atroxabot_core::unix_now;

use super::panel::limit_bars;
use super::widgets::{Scroll, chip, field_label, modal};
use super::{Account, Shell, ago, compact, money};
use crate::orb::{self, Mood};
use crate::paint::{Color, Painter, Rect, fade};
use crate::theme;
use crate::ui::{Ui, icon, icon_button};

/// The modal's size.
const SIZE: (f32, f32) = (940.0, 780.0);
/// Weeks in the grid of days: a year.
const WEEKS: u64 = 53;
/// Most days drawn as bars.
const MOST_BARS: u64 = 90;
/// Most rows of a breakdown.
const MOST_ROWS: usize = 6;
/// How strongly a day is drawn, from nothing to the most.
const SHADES: [f32; 5] = [0.07, 0.26, 0.46, 0.7, 1.0];
/// The months, as dates name them.
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// The period the numbers cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Range {
    Week,
    #[default]
    Month,
    Quarter,
    Year,
    All,
}

impl Range {
    const ALL: [Self; 5] = [Self::Week, Self::Month, Self::Quarter, Self::Year, Self::All];

    fn label(self) -> &'static str {
        match self {
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::Quarter => "90 days",
            Self::Year => "Year",
            Self::All => "All time",
        }
    }

    /// How many days it reaches back, today included; `None` for all of them.
    fn days(self) -> Option<u64> {
        match self {
            Self::Week => Some(7),
            Self::Month => Some(30),
            Self::Quarter => Some(90),
            Self::Year => Some(365),
            Self::All => None,
        }
    }
}

/// What the grid and the bars measure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Metric {
    #[default]
    Tokens,
    Cost,
    Tools,
}

impl Metric {
    const ALL: [Self; 3] = [Self::Tokens, Self::Cost, Self::Tools];

    fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Tokens",
            Self::Cost => "Cost",
            Self::Tools => "Tool calls",
        }
    }

    fn of(self, tally: &Tally) -> f64 {
        match self {
            Self::Tokens => tally.tokens() as f64,
            Self::Cost => tally.cost,
            Self::Tools => tally.tools as f64,
        }
    }

    fn show(self, value: f64) -> String {
        match self {
            Self::Tokens => format!("{} tokens", compact(value as u64)),
            Self::Cost => money(value),
            Self::Tools => format!("{value:.0} tool calls"),
        }
    }
}

/// The sessions' usage, read and summed.
struct Data {
    ledger: Ledger,
    /// What each day used.
    days: BTreeMap<u64, Tally>,
    today: u64,
    /// Days in a row up to today, and the longest run.
    streaks: (u64, u64),
    /// The chosen period.
    summary: Summary,
}

/// The open statistics.
#[derive(Default)]
pub(super) struct StatsView {
    /// `None` while the sessions are being read.
    data: Option<Data>,
    range: Range,
    metric: Metric,
    scroll: Scroll,
}

/// "12 Nov 2026".
fn date(day: u64) -> String {
    let (year, month, of_month) = civil(day);
    format!("{of_month} {} {year}", MONTHS[(month as usize).clamp(1, 12) - 1])
}

/// "1 day", "3 days".
fn days(count: u64) -> String {
    if count == 1 { "1 day".to_owned() } else { format!("{count} days") }
}

impl StatsView {
    /// The first day of the chosen period.
    fn from(range: Range, today: u64) -> u64 {
        range.days().map_or(0, |days| (today + 1).saturating_sub(days))
    }

    /// Every session's usage was read.
    pub fn loaded(&mut self, ledger: Ledger) {
        let today = day_of(unix_now());
        self.data = Some(Data {
            days: ledger.days(),
            streaks: ledger.streaks(today),
            summary: ledger.summary(Self::from(self.range, today)),
            today,
            ledger,
        });
    }

    /// Draws the statistics over `view`. Returns whether they were closed.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, shell: &Shell) -> bool {
        let t = p.theme;
        let (card, outside) = modal(p, ui, view, SIZE, "stats");
        let mut close = outside;
        let (x, w) = (card.x + 28.0, card.w - 56.0);
        p.label("Usage", theme::HEADING, x, card.y + 20.0, t.text);
        if icon_button(p, ui, Rect::new(card.right() - 44.0, card.y + 14.0, 30.0, 30.0), icon::X, true) {
            close = true;
        }
        // The period, as pills beside the way out.
        let mut right = card.right() - 58.0;
        for range in Range::ALL.into_iter().rev() {
            let width = p.layout(range.label(), theme::SMALL, None).width() + 22.0;
            right -= width;
            if chip(p, ui, Rect::new(right, card.y + 15.0, width, 28.0), range.label(), self.range == range) {
                self.range = range;
                if let Some(data) = &mut self.data {
                    data.summary = data.ledger.summary(Self::from(range, data.today));
                }
            }
            right -= 6.0;
        }
        let body = Rect::new(card.x, card.y + 58.0, card.w, card.h - 59.0);
        let note = match &self.data {
            None => Some("Reading your sessions…"),
            Some(data) if data.ledger.records.is_empty() => {
                Some("Nothing yet. Talk to an agent and what it used shows up here.")
            }
            Some(_) => None,
        };
        let (Some(data), None) = (&self.data, note) else {
            let note = p.layout(note.unwrap_or_default(), theme::BODY, None);
            p.text(&note, body.x + (body.w - note.width()) * 0.5, body.y + body.h * 0.5 - 20.0, t.text_faint);
            return close;
        };

        let hot = body.contains(ui.mouse);
        let offset = self.scroll.update(ui, hot);
        let clip = p.push_clip(body);
        let mut tip = None;
        let mut y = body.y + 8.0 - offset;
        y = cards(p, data, self.range, (x, y, w));
        y = limits(p, shell, (x, y, w));
        let (below, metric) = grid(p, ui, data, self.metric, (x, y, w), hot, &mut tip);
        y = bars(p, ui, data, (self.metric, self.range), (x, below, w), hot, &mut tip);
        y = composition(p, &data.summary.total, (x, y, w));
        y = breakdown(p, shell, &data.summary, (x, y, w));
        let content = y + offset - body.y + 16.0;
        p.set_clip(clip);
        self.scroll.finish(ui, content, body.h);
        self.scroll.bar(p, body, content);
        if let Some(metric) = metric {
            self.metric = metric;
        }
        // What the mouse is over, said in a small card beside it.
        if let Some((at, text)) = tip {
            let text = p.layout(&text, theme::TINY, None);
            let tip = Rect::new(
                // Not `clamp`, which panics when the card is narrower than the tip.
                (at.0 - text.width() * 0.5 - 10.0).min(card.right() - text.width() - 28.0).max(card.x + 8.0),
                at.1 - 34.0,
                text.width() + 20.0,
                26.0,
            );
            p.shadow(Rect::new(tip.x, tip.y + 4.0, tip.w, tip.h), t.shadow, theme::RADIUS_SM, 12.0);
            p.bordered(tip, t.surface, theme::RADIUS_SM, 1.0, t.border_strong);
            p.text(&text, tip.x + 10.0, tip.y + (tip.h - text.height()) * 0.5, t.text);
        }
        close
    }
}

/// Eight numbers on cards, four to a row. Returns where the next section starts.
fn cards(p: &mut Painter, data: &Data, range: Range, (x, y, w): (f32, f32, f32)) -> f32 {
    let t = p.theme;
    let (summary, total) = (&data.summary, &data.summary.total);
    let a_day = if summary.active_days == 0 { 0.0 } else { total.cost / summary.active_days as f64 };
    let busiest = summary.busiest.map_or_else(
        || ("—".to_owned(), String::new()),
        |(day, tokens)| (date(day), format!("{} tokens", compact(tokens))),
    );
    let span = range.days().map_or_else(|| "since the first session".to_owned(), |n| format!("of the last {n}"));
    let failed: u64 = summary.tools.iter().map(|tool| tool.errors).sum();
    let cards = [
        (
            icon::LAYERS,
            "TOKENS",
            compact(total.tokens()),
            format!("{} in · {} out", compact(total.input), compact(total.output)),
        ),
        (icon::ZAP, "COST", money(total.cost), format!("{} a day you used it", money(a_day))),
        (icon::MESSAGE, "SESSIONS", summary.sessions.to_string(), format!("{} messages", total.prompts)),
        (icon::WRENCH, "TOOL CALLS", total.tools.to_string(), format!("{failed} failed")),
        (
            icon::HISTORY,
            "FROM CACHE",
            format!("{:.0}%", total.cache_rate() * 100.0),
            format!("{} tokens reused", compact(total.cached)),
        ),
        (icon::ACTIVITY, "ACTIVE DAYS", summary.active_days.to_string(), span),
        (icon::FLAME, "STREAK", days(data.streaks.0), format!("longest {}", days(data.streaks.1))),
        (icon::CHART, "BUSIEST DAY", busiest.0, busiest.1),
    ];
    let (gap, height) = (10.0, 82.0);
    let width = (w - 3.0 * gap) / 4.0;
    for (index, (glyph, caption, value, about)) in cards.into_iter().enumerate() {
        let card =
            Rect::new(x + (index % 4) as f32 * (width + gap), y + (index / 4) as f32 * (height + gap), width, height);
        p.bordered(card, t.surface, theme::RADIUS, 1.0, t.border);
        icon::draw(p, glyph, (card.x + 20.0, card.y + 19.0), 13.0, t.text_faint);
        p.label(caption, theme::CAPTION, card.x + 32.0, card.y + 11.0, t.text_muted);
        let mut value = p.layout(&value, theme::TITLE, None);
        value.truncate(p.fonts, card.w - 26.0);
        p.text(&value, card.x + 13.0, card.y + 29.0, t.text);
        let mut about = p.layout(&about, theme::TINY, None);
        about.truncate(p.fonts, card.w - 26.0);
        p.text(&about, card.x + 13.0, card.y + 59.0, t.text_faint);
    }
    y + 2.0 * (height + gap) + 16.0
}

/// Where each Claude account's plan limits stood when Claude Code last
/// reported them, an account a column. Nothing is asked of anyone: these
/// are the numbers kept from the turns that ran. Returns where the next
/// section starts (`y` itself when none are known).
fn limits(p: &mut Painter, shell: &Shell, (x, y, w): (f32, f32, f32)) -> f32 {
    let t = p.theme;
    let accounts: Vec<_> = shell
        .config
        .all_connections()
        .into_iter()
        .filter_map(|connection| {
            Some((shell.limits.get(&connection.id).filter(|known| !known.windows.is_empty())?, connection))
        })
        .collect();
    if accounts.is_empty() {
        return y;
    }
    field_label(p, icon::ACTIVITY, "PLAN LIMITS", x, y);
    let (gap, across) = (28.0, accounts.len().clamp(2, 3));
    let width = (w - gap * (across as f32 - 1.0)) / across as f32;
    let now = unix_now();
    let mut top = y + 30.0;
    for row in accounts.chunks(across) {
        let mut bottom = top;
        for (column, (known, connection)) in row.iter().enumerate() {
            let left = x + column as f32 * (width + gap);
            shell.provider_mark(p, connection, (left + 8.0, top + 9.0), 15.0, t.text_muted);
            let mut name = p.layout(&connection.name, theme::LABEL, None);
            name.truncate(p.fonts, width - 26.0);
            p.text(&name, left + 24.0, top, t.text);
            // Who it is, and how old the numbers are.
            let email = match shell.claude.get(&connection.id) {
                Some(Account::Ready(info)) if !info.account.email.is_empty() => format!("{} · ", info.account.email),
                _ => String::new(),
            };
            let seen =
                if known.at == 0 { "kept from earlier".to_owned() } else { format!("{} ago", ago(now, known.at)) };
            let mut about = p.layout(&format!("{email}{seen}").replace("now ago", "just now"), theme::TINY, None);
            about.truncate(p.fonts, width);
            p.text(&about, left, top + 22.0, t.text_faint);
            bottom = bottom.max(limit_bars(p, known, (left, top + 46.0, width)));
        }
        top = bottom + 10.0;
    }
    top + 12.0
}

/// How strongly a day that used `value` of the most (`max`) is drawn.
fn shade(value: f64, max: f64) -> f32 {
    if value <= 0.0 || max <= 0.0 {
        return SHADES[0];
    }
    // The root spreads the many small days apart from the few huge ones.
    SHADES[1 + (((value / max).sqrt() * 3.999) as usize).min(3)]
}

/// A year of days as a grid, a column a week, the busier the brighter.
/// Returns where the next section starts, and the metric a click chose.
fn grid(
    p: &mut Painter,
    ui: &mut Ui,
    data: &Data,
    metric: Metric,
    (x, y, w): (f32, f32, f32),
    hot: bool,
    tip: &mut Option<((f32, f32), String)>,
) -> (f32, Option<Metric>) {
    let t = p.theme;
    field_label(p, icon::ACTIVITY, "THE LAST YEAR", x, y);
    let mut picked = None;
    let mut right = x + w;
    for choice in Metric::ALL.into_iter().rev() {
        let width = p.layout(choice.label(), theme::SMALL, None).width() + 20.0;
        right -= width;
        if chip(p, ui, Rect::new(right, y - 6.0, width, 26.0), choice.label(), metric == choice) {
            picked = Some(choice);
        }
        right -= 6.0;
    }
    let pitch = ((w - 34.0) / WEEKS as f32).floor().min(17.0);
    let cell = pitch - 3.0;
    let (left, top) = (x + 34.0, y + 50.0);
    let first = (data.today - u64::from(weekday(data.today))).saturating_sub((WEEKS - 1) * 7);
    let max = data.days.range(first..=data.today).map(|(_, tally)| metric.of(tally)).fold(0.0, f64::max);
    for (row, name) in [(1.0, "Mon"), (3.0, "Wed"), (5.0, "Fri")] {
        p.label(name, theme::TINY, x, top + row * pitch - 2.0, t.text_faint);
    }
    let (mut month, mut labelled) = (0, None::<u64>);
    for week in 0..WEEKS {
        let start = first + week * 7;
        let column = left + week as f32 * pitch;
        // A month is named over the column it starts in, where there is room for the name.
        let (_, starts, _) = civil(start);
        if starts != month {
            month = starts;
            let room = labelled.is_none_or(|last| week >= last + 3) && (week > 0 || civil(start + 14).1 == starts);
            if room {
                p.label(MONTHS[(starts as usize).clamp(1, 12) - 1], theme::TINY, column, y + 30.0, t.text_faint);
                labelled = Some(week);
            }
        }
        for weekday in 0..7 {
            let day = start + weekday;
            if day > data.today {
                break;
            }
            let square = Rect::new(column, top + weekday as f32 * pitch, cell, cell);
            let used = data.days.get(&day);
            p.rect(square, fade(t.text, shade(used.map_or(0.0, |tally| metric.of(tally)), max)), 3.0);
            let reach = Rect::new(square.x - 1.5, square.y - 1.5, pitch, pitch);
            if hot && reach.contains(ui.mouse) {
                p.bordered(square, fade(t.text, 0.0), 3.0, 1.0, t.text);
                let said = used.map_or_else(
                    || "nothing".to_owned(),
                    |tally| {
                        format!(
                            "{} · {} · {} tool calls",
                            Metric::Tokens.show(tally.tokens() as f64),
                            money(tally.cost),
                            tally.tools
                        )
                    },
                );
                *tip = Some(((square.x + cell * 0.5, square.y), format!("{} · {said}", date(day))));
            }
        }
    }
    // Under it: how the shades read.
    let foot = top + 7.0 * pitch + 6.0;
    let mut legend = x + w;
    let more = p.layout("More", theme::TINY, None);
    legend -= more.width();
    p.text(&more, legend, foot, t.text_faint);
    for shade in SHADES.iter().rev() {
        legend -= cell + 3.0;
        p.rect(Rect::new(legend - 3.0, foot + 1.0, cell, cell), fade(t.text, *shade), 3.0);
    }
    let less = p.layout("Less", theme::TINY, None);
    p.text(&less, legend - 9.0 - less.width(), foot, t.text_faint);
    (foot + 40.0, picked)
}

/// The chosen period day by day, as bars. Returns where the next section starts.
fn bars(
    p: &mut Painter,
    ui: &Ui,
    data: &Data,
    (metric, range): (Metric, Range),
    (x, y, w): (f32, f32, f32),
    hot: bool,
    tip: &mut Option<((f32, f32), String)>,
) -> f32 {
    let t = p.theme;
    let count = range.days().unwrap_or(MOST_BARS).min(MOST_BARS);
    field_label(p, icon::CHART, &format!("THE LAST {count} DAYS"), x, y);
    let area = Rect::new(x, y + 32.0, w, 96.0);
    let first = (data.today + 1).saturating_sub(count);
    let value = |day: u64| data.days.get(&day).map_or(0.0, |tally| metric.of(tally));
    let max = (first..=data.today).map(value).fold(0.0, f64::max);
    let most = p.layout(&metric.show(max), theme::TINY, None);
    p.text(&most, x + w - most.width(), y, t.text_faint);
    let pitch = w / count as f32;
    let width = (pitch - 2.0).clamp(2.0, 30.0);
    p.rect(Rect::new(x, area.bottom(), w, 1.0), t.border, 0.0);
    for index in 0..count {
        let day = first + index;
        let column = Rect::new(x + index as f32 * pitch, area.y, pitch, area.h);
        let used = value(day);
        let over = hot && column.contains(ui.mouse);
        if used > 0.0 && max > 0.0 {
            let height = ((used / max) as f32 * area.h).max(2.0);
            let bar = Rect::new(column.x + (pitch - width) * 0.5, area.bottom() - height, width, height);
            p.rect(bar, fade(t.text, if over { 1.0 } else { 0.62 }), width.min(6.0) * 0.5);
        }
        if over {
            *tip = Some(((column.x + pitch * 0.5, area.y), format!("{} · {}", date(day), metric.show(used))));
        }
    }
    p.label(&date(first), theme::TINY, x, area.bottom() + 7.0, t.text_faint);
    let last = p.layout("Today", theme::TINY, None);
    p.text(&last, x + w - last.width(), area.bottom() + 7.0, t.text_faint);
    area.bottom() + 46.0
}

/// Where the period's tokens went, as one bar in four parts. Returns where
/// the next section starts.
fn composition(p: &mut Painter, total: &Tally, (x, y, w): (f32, f32, f32)) -> f32 {
    let t = p.theme;
    field_label(p, icon::LAYERS, "WHERE THE TOKENS WENT", x, y);
    let fresh = total.input.saturating_sub(total.cached + total.written);
    let parts = [
        ("Output", total.output, 1.0),
        ("New input", fresh, 0.62),
        ("Written to cache", total.written, 0.36),
        ("Read from cache", total.cached, 0.17),
    ];
    let all = total.tokens().max(1) as f32;
    let bar = Rect::new(x, y + 30.0, w, 10.0);
    let mut left = bar.x;
    for (_, tokens, strength) in parts {
        let width = (tokens as f32 / all * bar.w).round();
        if width >= 1.0 {
            p.rect(Rect::new(left, bar.y, (width - 2.0).max(1.0), bar.h), fade(t.text, strength), 3.0);
        }
        left += width;
    }
    let column = w / 4.0;
    for (index, (name, tokens, strength)) in parts.into_iter().enumerate() {
        let at = x + index as f32 * column;
        p.rect(Rect::new(at, y + 56.0, 9.0, 9.0), fade(t.text, strength), 3.0);
        p.label(name, theme::SMALL, at + 16.0, y + 50.0, t.text_muted);
        let share = format!("{} · {:.0}%", compact(tokens), tokens as f32 / all * 100.0);
        p.label(&share, theme::TINY, at + 16.0, y + 69.0, t.text_faint);
    }
    y + 110.0
}

/// One row of a breakdown: a name, what it used at the right, a bar of its
/// share of the most, and a line about it.
fn share_row(
    p: &mut Painter,
    (x, y, w): (f32, f32, f32),
    (name, used, about): (&str, &str, &str),
    (share, ink): (f32, Color),
) {
    let t = p.theme;
    let used = p.layout(used, theme::SMALL, None);
    p.text(&used, x + w - used.width(), y, t.text_muted);
    let mut name = p.layout(name, theme::SMALL, None);
    name.truncate(p.fonts, w - used.width() - 14.0);
    p.text(&name, x, y, t.text);
    p.rect(Rect::new(x, y + 23.0, w, 4.0), t.hover, 2.0);
    p.rect(Rect::new(x, y + 23.0, (w * share.clamp(0.0, 1.0)).max(4.0), 4.0), ink, 2.0);
    p.label(about, theme::TINY, x, y + 31.0, t.text_faint);
}

/// Who and what used the period's tokens: providers beside models, then
/// agents beside tools. Returns where it ends.
fn breakdown(p: &mut Painter, shell: &Shell, summary: &Summary, (x, y, w): (f32, f32, f32)) -> f32 {
    let t = p.theme;
    let half = (w - 28.0) * 0.5;
    let right = x + half + 28.0;
    let pitch = 56.0;
    let ink = fade(t.text, 0.7);
    let used = |tally: &Tally| {
        if tally.cost > 0.0 {
            format!("{} · {}", compact(tally.tokens()), money(tally.cost))
        } else {
            compact(tally.tokens())
        }
    };
    let about = |tally: &Tally| format!("{} tool calls · {:.0}% from cache", tally.tools, tally.cache_rate() * 100.0);
    let more = |p: &mut Painter, count: usize, x: f32, y: f32| {
        if count > MOST_ROWS {
            p.label(&format!("and {} more", count - MOST_ROWS), theme::TINY, x, y, t.text_faint);
        }
    };

    field_label(p, icon::PLUG, "PROVIDERS", x, y);
    field_label(p, icon::SPARKLES, "MODELS", right, y);
    let top = y + 30.0;
    let most = summary.providers.first().map_or(1, |(_, _, tally)| tally.tokens().max(1)) as f32;
    for (index, (id, name, tally)) in summary.providers.iter().take(MOST_ROWS).enumerate() {
        let row = top + index as f32 * pitch;
        let share = tally.tokens() as f32 / most;
        // A provider that still exists wears its mark and its name of today.
        if let Some(connection) = shell.config.connection(id) {
            shell.provider_mark(p, &connection, (x + 8.0, row + 9.0), 15.0, t.text_muted);
            let said = (connection.name.as_str(), used(tally), about(tally));
            share_row(p, (x + 26.0, row, half - 26.0), (said.0, &said.1, &said.2), (share, ink));
        } else {
            let known = match (id.is_empty(), name.is_empty()) {
                (true, _) => "Not recorded then",
                (false, true) => "A provider since removed",
                (false, false) => name,
            };
            share_row(p, (x, row, half), (known, &used(tally), &about(tally)), (share, ink));
        }
    }
    let most = summary.models.first().map_or(1, |(_, tally)| tally.tokens().max(1)) as f32;
    for (index, (model, tally)) in summary.models.iter().take(MOST_ROWS).enumerate() {
        let share = tally.tokens() as f32 / most;
        share_row(p, (right, top + index as f32 * pitch, half), (model, &used(tally), &about(tally)), (share, ink));
    }
    let rows = summary.providers.len().max(summary.models.len()).min(MOST_ROWS);
    let mut y = top + rows as f32 * pitch;
    more(p, summary.providers.len(), x, y);
    more(p, summary.models.len(), right, y);
    y += 30.0;

    field_label(p, icon::BOT, "AGENTS", x, y);
    if !summary.tools.is_empty() {
        field_label(p, icon::WRENCH, "TOOLS", right, y);
    }
    let top = y + 30.0;
    let most = summary.agents.first().map_or(1, |(_, tally)| tally.tokens().max(1)) as f32;
    for (index, (id, tally)) in summary.agents.iter().take(MOST_ROWS).enumerate() {
        let row = top + index as f32 * pitch;
        let share = tally.tokens() as f32 / most;
        // An agent wears its own colour here too.
        match shell.agents.iter().find(|agent| agent.id == *id) {
            Some(agent) => {
                let color = Shell::color_of(agent);
                orb::agent(p, &agent.portrait, (x + 9.0, row + 9.0), 18.0, color, Mood::Idle, 0.0);
                share_row(p, (x + 26.0, row, half - 26.0), (&agent.name, &used(tally), &about(tally)), (share, color));
            }
            None => share_row(p, (x, row, half), ("An agent since deleted", &used(tally), &about(tally)), (share, ink)),
        }
    }
    let most_calls = summary.tools.first().map_or(1, |tool| tool.calls.max(1)) as f32;
    for (index, tool) in summary.tools.iter().take(MOST_ROWS).enumerate() {
        let failed = if tool.errors == 0 {
            "none failed".to_owned()
        } else {
            format!("{} failed · {:.0}%", tool.errors, tool.errors as f32 / tool.calls.max(1) as f32 * 100.0)
        };
        let calls = if tool.calls == 1 { "1 call".to_owned() } else { format!("{} calls", tool.calls) };
        let row = (right, top + index as f32 * pitch, half);
        share_row(p, row, (&tool.name, &calls, &failed), (tool.calls as f32 / most_calls, ink));
    }
    let rows = summary.agents.len().max(summary.tools.len()).min(MOST_ROWS);
    y = top + rows as f32 * pitch;
    more(p, summary.agents.len(), x, y);
    more(p, summary.tools.len(), right, y);
    y + 14.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_dates_and_shades_read_well() {
        assert_eq!(StatsView::from(Range::Week, 100), 94, "seven days, today included");
        assert_eq!((StatsView::from(Range::All, 100), StatsView::from(Range::Year, 100)), (0, 0));
        assert_eq!(
            (date(20_767), days(1), days(4)),
            ("10 Nov 2026".to_owned(), "1 day".to_owned(), "4 days".to_owned())
        );
        let shades = [shade(0.0, 10.0), shade(0.1, 10.0), shade(3.0, 10.0), shade(10.0, 10.0), shade(5.0, 0.0)];
        assert!(shades[0] < shades[1] && shades[1] < shades[2] && shades[2] < shades[3], "{shades:?}");
        assert!((shades[3] - 1.0).abs() < f32::EPSILON && (shades[4] - SHADES[0]).abs() < f32::EPSILON);
        assert_eq!(Metric::Cost.show(1.5), "$1.50");
    }
}
