//! Drop-down lists: the model (and effort) under the message box, the
//! provider and folder in the top bar, the branch in the side panel, and the
//! dropdowns of the agent editor. One list is open at a
//! time, over everything else.

use arboard::Clipboard;
use atroxabot_core::{Autonomy, Connection, ConnectionKind};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use super::agent_form::{VOICES, autonomy_hint};
use super::settings::{Settings, Tab};
use super::widgets::{Scroll, chip, logo_badge};
use super::{Account, Modal, Models, Shell};
use crate::app::Action;
use crate::editor::Editor;
use crate::image::Lookup;
use crate::logo::Logo;
use crate::paint::{Color, Painter, Rect, fade};
use crate::theme;
use crate::ui::{Ui, edit_key, icon, id};

/// Which list is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Menu {
    /// The selected agent's model (and effort), from its provider's list or typed.
    Model,
    /// The folder the session works in.
    Folder,
    /// The provider the selected agent thinks through.
    Provider,
    /// The branch checked out in the session's folder.
    Branch,
    /// The agent editor's provider.
    FormProvider,
    /// The agent editor's autonomy.
    FormAutonomy,
    /// The agent editor's voice provider.
    FormVoice,
}

impl Menu {
    /// Whether it opens under its button (in a form) rather than over it.
    fn below(self) -> bool {
        self != Self::Model
    }

    /// Whether what is typed narrows it (and may name a choice of its own).
    fn typed(self) -> bool {
        self == Self::Model
    }
}

/// An open list.
pub(super) struct MenuState {
    /// Which one.
    pub menu: Menu,
    /// The button it hangs from.
    anchor: Rect,
    /// Where it was drawn last frame; the mouse belongs to it there.
    pub rect: Rect,
    /// What was typed to narrow the models, or to name one not listed.
    query: Editor,
    scroll: Scroll,
}

/// One choice.
#[derive(Default)]
struct Item {
    /// What choosing it stores.
    value: String,
    glyph: Option<char>,
    /// The provider whose mark the row wears instead of a glyph.
    provider: Option<Connection>,
    label: String,
    detail: String,
    selected: bool,
    /// A note rather than a choice (the list is loading).
    inert: bool,
    /// Set apart from the choices above it by a line.
    apart: bool,
}

/// A model a provider offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ModelChoice {
    /// What the provider calls it.
    pub value: String,
    /// Its name, e.g. `Opus 5.5`.
    pub label: String,
    /// A line about it.
    pub detail: String,
    /// The efforts it takes, weakest first.
    pub efforts: Vec<String>,
}

/// The efforts offered where a provider does not say which a model takes.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The icon of a level of autonomy.
pub(super) fn autonomy_glyph(level: Autonomy) -> char {
    match level {
        Autonomy::Ask => icon::SHIELD,
        Autonomy::Edits => icon::PEN,
        Autonomy::Auto => icon::SPARKLES,
        Autonomy::Full => icon::ZAP,
        Autonomy::ReadOnly => icon::EYE,
    }
}

/// What a provider's own picture is filed under in the user's `icons`
/// folder: `claude`, `anthropic`, or the name in an API's address
/// (`openrouter` of `openrouter.ai`).
pub(super) fn icon_key(connection: &Connection) -> String {
    match connection.kind {
        ConnectionKind::ClaudeAgent => "claude".to_owned(),
        ConnectionKind::ClaudeApi => "anthropic".to_owned(),
        ConnectionKind::OpenAi => {
            let host = connection.host().split(':').next().unwrap_or_default();
            let mut labels = host.rsplit('.');
            let last = labels.next().unwrap_or_default();
            labels.next().unwrap_or(last).to_lowercase()
        }
    }
}

/// The effort a model starts at: Sonnet thinks hard, the others at medium.
fn default_effort(model: &ModelChoice) -> &'static str {
    let named =
        |family: &str| model.value.to_lowercase().contains(family) || model.label.to_lowercase().contains(family);
    if named("sonnet") { "high" } else { "medium" }
}

/// An effort as a menu shows it.
pub(super) fn effort_label(effort: &str) -> &str {
    match effort {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra",
        "max" => "Max",
        other => other,
    }
}

impl Shell {
    /// Opens `menu` at the button `anchor`.
    pub(super) fn open_menu(&mut self, menu: Menu, anchor: Rect) {
        self.menu = Some(MenuState { menu, anchor, rect: anchor, query: Editor::default(), scroll: Scroll::default() });
    }

    /// Opens `menu` at `anchor`, or closes it if it is the one open.
    pub(super) fn toggle_menu(&mut self, menu: Menu, anchor: Rect) {
        if self.menu.as_ref().is_some_and(|state| state.menu == menu) {
            self.menu = None;
        } else {
            self.open_menu(menu, anchor);
        }
    }

    /// Whether `menu` is the list that is open.
    pub(super) fn menu_open(&self, menu: Menu) -> bool {
        self.menu.as_ref().is_some_and(|state| state.menu == menu)
    }

    /// The icon of a kind of provider called `name` (or filed under it).
    pub(super) fn preset_glyph(kind: ConnectionKind, name: &str) -> char {
        let name = name.to_lowercase();
        match kind {
            ConnectionKind::ClaudeAgent => icon::SPARKLES,
            ConnectionKind::ClaudeApi => icon::KEY,
            ConnectionKind::OpenAi if name.contains("openrouter") => icon::ROUTE,
            ConnectionKind::OpenAi if name.contains("openai") => icon::HEXAGON,
            ConnectionKind::OpenAi if name.contains("ollama") || name.contains("localhost") => icon::SERVER,
            ConnectionKind::OpenAi => icon::PLUG,
        }
    }

    /// The colour a provider's mark wears: its platform's, when the app knows one.
    pub(super) fn provider_tint(connection: &Connection) -> Option<Color> {
        theme::provider_color(&icon_key(connection))
    }

    /// The icon the app draws for a provider.
    pub(super) fn provider_glyph(connection: &Connection) -> char {
        Self::preset_glyph(connection.kind, &icon_key(connection))
    }

    /// Draws what a provider is known by over the badge in `rect`: the
    /// user's own picture of it (a file in the `icons` folder), or its
    /// platform's mark. Returns whether there was either.
    pub(super) fn provider_picture(&self, p: &mut Painter, connection: &Connection, rect: Rect) -> bool {
        let key = icon_key(connection);
        let Some(path) = self.icons.get(&key) else {
            let logo = Logo::named(&key);
            if let Some(logo) = logo {
                logo_badge(p, rect, logo, Self::provider_tint(connection));
            }
            return logo.is_some();
        };
        // Over whatever stood there, so a picture with clear parts shows no glyph through them.
        let under = p.theme.surface;
        p.rect(rect, under, rect.w * 0.28);
        matches!(p.image(path, rect, rect.w * 0.22), Lookup::Ready(_))
    }

    /// Draws a provider's mark `size` across at `center`: the user's
    /// picture of it when there is one, else its platform's logo (in the
    /// platform's colour, or `ink` where it has none), else an icon.
    pub(super) fn provider_mark(
        &self,
        p: &mut Painter,
        connection: &Connection,
        center: (f32, f32),
        size: f32,
        ink: Color,
    ) {
        let rect = Rect::new(center.0 - size * 0.5, center.1 - size * 0.5, size, size);
        let pictured = self
            .icons
            .get(&icon_key(connection))
            .is_some_and(|path| matches!(p.image(path, rect, size * 0.22), Lookup::Ready(_)));
        if pictured {
            return;
        }
        let ink = Self::provider_tint(connection).unwrap_or(ink);
        match Logo::named(&icon_key(connection)) {
            Some(logo) => p.logo(logo, center, size, ink),
            None => icon::draw(p, Self::provider_glyph(connection), center, size - 1.0, ink),
        }
    }

    /// One line on a provider: a Claude account's sign-in, or where an API goes.
    pub(super) fn provider_detail(&self, connection: &Connection) -> String {
        if connection.kind != ConnectionKind::ClaudeAgent {
            return format!("{} · {}", connection.kind.label(), connection.host());
        }
        match self.claude.get(&connection.id) {
            Some(Account::Ready(info)) if info.account.email.is_empty() => "Not signed in".to_owned(),
            Some(Account::Ready(info)) if info.account.subscription.is_empty() => info.account.email.clone(),
            Some(Account::Ready(info)) => format!("{} · {}", info.account.email, info.account.subscription),
            Some(Account::Missing) => "Claude Code is not installed".to_owned(),
            Some(Account::Failed(why)) => why.clone(),
            Some(Account::Checking) | None => "Checking…".to_owned(),
        }
    }

    /// The models `connection` offers, its default first. Empty while they
    /// are being fetched or when they could not be.
    pub(super) fn model_choices(&self, connection: &Connection) -> Vec<ModelChoice> {
        if connection.kind == ConnectionKind::ClaudeAgent {
            let Some(Account::Ready(info)) = self.claude.get(&connection.id) else { return Vec::new() };
            // Aliases of one model (`default`, `opus`) are listed once: the first is the default.
            // Of a family listed in several versions (Fable 5.1 beside 5.2), only the newest is offered.
            let newer = |model: &atroxabot_core::ClaudeModel| {
                let Some((family, version)) = model_parts(&model.resolved) else { return false };
                let beats = |other: &atroxabot_core::ClaudeModel| {
                    model_parts(&other.resolved).is_some_and(|(theirs, newer)| theirs == family && newer > version)
                };
                info.models.iter().any(beats)
            };
            let mut seen: Vec<&str> = Vec::new();
            let mut choices = Vec::new();
            for model in &info.models {
                if newer(model) {
                    continue;
                }
                if !model.resolved.is_empty() {
                    if seen.contains(&model.resolved.as_str()) {
                        continue;
                    }
                    seen.push(&model.resolved);
                }
                // Its name comes from the model it stands for; what Claude Code says of it goes under it.
                let (first, about) = model.description.split_once(" · ").unwrap_or((&model.description, ""));
                let label = model_name(&model.resolved)
                    .or_else(|| Some(first.to_owned()).filter(|first| !first.is_empty()))
                    .unwrap_or_else(|| model.display_name.clone());
                let detail =
                    if about.is_empty() || first == label { about.to_owned() } else { model.description.clone() };
                choices.push(ModelChoice { value: model.value.clone(), label, detail, efforts: model.efforts.clone() });
            }
            return choices;
        }
        let Some(Models::Ready(models)) = self.models.get(&connection.id) else { return Vec::new() };
        let mut choices: Vec<ModelChoice> = models
            .iter()
            .map(|model| ModelChoice {
                value: model.id.clone(),
                label: if model.name.is_empty() { model.id.clone() } else { model.name.clone() },
                detail: if model.name.is_empty() { String::new() } else { model.id.clone() },
                efforts: model.reasoning_levels.clone(),
            })
            .collect();
        // The app's usual Claude model leads Anthropic's list, whatever its order.
        if connection.kind == ConnectionKind::ClaudeApi
            && let Some(at) = choices.iter().position(|choice| choice.value == super::DEFAULT_CLAUDE)
            && let Some(leading) = choices.get_mut(..=at)
        {
            leading.rotate_right(1);
        }
        choices
    }

    /// The model `chosen` stands for on `connection`: the one named, or the
    /// provider's default when none is.
    pub(super) fn model_in_use(&self, connection: &Connection, chosen: &str) -> ModelChoice {
        let choices = self.model_choices(connection);
        let found = if chosen.is_empty() {
            choices.into_iter().next()
        } else {
            choices.into_iter().find(|choice| choice.value == chosen)
        };
        found.unwrap_or_else(|| ModelChoice {
            value: chosen.to_owned(),
            label: if chosen.is_empty() { "Loading models…".to_owned() } else { chosen.to_owned() },
            ..ModelChoice::default()
        })
    }

    /// The efforts to offer for `model`: what its provider says it takes.
    /// Anthropic's API does not say, and its models take the usual ones;
    /// elsewhere a model that lists none takes none.
    pub(super) fn effort_choices(kind: ConnectionKind, model: &ModelChoice) -> Vec<String> {
        if model.efforts.is_empty() && kind == ConnectionKind::ClaudeApi {
            EFFORTS.map(str::to_owned).into()
        } else {
            model.efforts.clone()
        }
    }

    /// The effort `model` thinks at: the one `chosen` if the model takes
    /// it, else the model's usual one. Empty for a model that takes none.
    pub(super) fn effort_in_use(kind: ConnectionKind, model: &ModelChoice, chosen: &str) -> String {
        let choices = Self::effort_choices(kind, model);
        let usual = default_effort(model);
        let offered = |effort: &str| choices.iter().any(|choice| choice == effort);
        if offered(chosen) {
            chosen.to_owned()
        } else if offered(usual) {
            usual.to_owned()
        } else {
            // Neither is offered: the middle of what is.
            choices.get(choices.len() / 2).cloned().unwrap_or_default()
        }
    }

    /// The models of `connection` as menu items, narrowed by `query`, with `chosen` marked.
    fn model_items(&self, connection: &Connection, chosen: &str, query: &str) -> Vec<Item> {
        let needle = query.trim().to_lowercase();
        let choices = self.model_choices(connection);
        let in_use = if chosen.is_empty() { choices.first().map(|c| c.value.clone()) } else { Some(chosen.to_owned()) };
        let fits = |c: &ModelChoice| {
            needle.is_empty() || c.label.to_lowercase().contains(&needle) || c.value.to_lowercase().contains(&needle)
        };
        let mut items: Vec<Item> = choices
            .into_iter()
            .filter(fits)
            .map(|choice| Item {
                selected: in_use.as_deref() == Some(choice.value.as_str()),
                value: choice.value,
                label: choice.label,
                detail: choice.detail,
                ..Item::default()
            })
            .collect();
        if items.is_empty() && needle.is_empty() {
            let note = match (connection.kind, self.claude.get(&connection.id), self.models.get(&connection.id)) {
                (ConnectionKind::ClaudeAgent, Some(Account::Missing), _) => "Claude Code is not installed".to_owned(),
                (ConnectionKind::ClaudeAgent, Some(Account::Failed(why)), _) | (_, _, Some(Models::Failed(why))) => {
                    format!("The models could not be listed: {why}")
                }
                _ => "Loading the models…".to_owned(),
            };
            items.push(Item { label: note, inert: true, ..Item::default() });
        }
        // Any model can be named, listed or not.
        let typed = query.trim();
        if !typed.is_empty() && !items.iter().any(|item| item.value == typed) {
            items.insert(
                0,
                Item {
                    value: typed.to_owned(),
                    label: format!("Use \u{201c}{typed}\u{201d}"),
                    detail: "A model id of your own".to_owned(),
                    ..Item::default()
                },
            );
        }
        items
    }

    /// Every provider as a menu item, with `chosen` marked.
    fn provider_items(&self, chosen: &str) -> Vec<Item> {
        self.config
            .all_connections()
            .iter()
            .map(|connection| Item {
                value: connection.id.clone(),
                provider: Some(connection.clone()),
                label: connection.name.clone(),
                detail: self.provider_detail(connection),
                selected: connection.id == chosen,
                ..Item::default()
            })
            .collect()
    }

    /// The agent editor, when it is open.
    fn form(&self) -> Option<&super::agent_form::AgentForm> {
        match &self.modal {
            Some(Modal::Agent(form)) => Some(form),
            _ => None,
        }
    }

    /// The choices of `menu`, narrowed by `query`.
    fn menu_items(&self, menu: Menu, query: &str) -> Vec<Item> {
        let form = self.form();
        let agent = self.selected_agent();
        match menu {
            Menu::Model => agent
                .and_then(|agent| Some((self.connection(agent)?, agent)))
                .map(|(connection, agent)| self.model_items(&connection, &agent.model, query))
                .unwrap_or_default(),
            Menu::Provider => {
                let mut items = self.provider_items(agent.map_or("", |agent| agent.connection.as_str()));
                items.push(Item {
                    value: "manage".to_owned(),
                    glyph: Some(icon::SETTINGS),
                    label: "Manage providers…".to_owned(),
                    apart: true,
                    ..Item::default()
                });
                items
            }
            Menu::FormProvider => self.provider_items(form.map_or("", |form| form.connection.as_str())),
            Menu::Folder => {
                let current = self.folder_in_use();
                let mut items: Vec<Item> = self
                    .config
                    .recent_folders
                    .iter()
                    .map(|folder| Item {
                        value: folder.clone(),
                        glyph: Some(icon::FOLDER),
                        label: folder_name(folder).to_owned(),
                        detail: folder.clone(),
                        selected: *folder == current,
                        ..Item::default()
                    })
                    .collect();
                items.push(Item {
                    value: "choose".to_owned(),
                    glyph: Some(icon::PLUS),
                    label: "Choose a folder…".to_owned(),
                    apart: !items.is_empty(),
                    ..Item::default()
                });
                items
            }
            Menu::Branch => {
                let Some(status) = &self.git.status else { return Vec::new() };
                let branch = |name: &String| Item {
                    value: name.clone(),
                    glyph: Some(icon::BRANCH),
                    label: name.clone(),
                    selected: *name == status.branch,
                    ..Item::default()
                };
                status.branches.iter().map(branch).collect()
            }
            Menu::FormAutonomy => {
                let connection = form.and_then(|form| self.config.connection(&form.connection));
                let level = |level: Autonomy| Item {
                    value: level.label().to_owned(),
                    glyph: Some(autonomy_glyph(level)),
                    label: level.label().to_owned(),
                    detail: autonomy_hint(level, connection.as_ref()).to_owned(),
                    selected: form.is_some_and(|form| form.autonomy == level),
                    ..Item::default()
                };
                Autonomy::ALL.into_iter().map(level).collect()
            }
            Menu::FormVoice => VOICES
                .iter()
                .map(|(value, label)| Item {
                    value: (*value).to_owned(),
                    label: (*label).to_owned(),
                    selected: form.is_some_and(|form| form.voice == *value),
                    ..Item::default()
                })
                .collect(),
        }
    }

    /// The value of the default model of the provider `connection`, if its models are known.
    fn default_model(&self, connection: &str) -> Option<String> {
        let connection = self.config.connection(connection)?;
        self.model_choices(&connection).into_iter().next().map(|choice| choice.value)
    }

    /// Applies a choice.
    fn choose(&mut self, menu: Menu, value: &str, actions: &mut Vec<Action>) {
        self.menu = None;
        match menu {
            Menu::Model => {
                let Some(mut agent) = self.selected_agent().cloned() else { return };
                // The provider's default is not pinned: it stays the default when that changes.
                let default = self.default_model(&agent.connection);
                let model = if default.as_deref() == Some(value) { String::new() } else { value.to_owned() };
                if model != agent.model {
                    // Another model thinks at its own usual effort.
                    agent.effort.clear();
                    agent.model = model;
                    self.update_agent(agent, actions);
                }
            }
            Menu::Provider if value == "manage" => {
                self.modal = Some(Modal::Settings(Box::new(Settings::new(Tab::Providers))));
            }
            Menu::Provider => {
                let Some(mut agent) = self.selected_agent().cloned() else { return };
                if agent.connection != value {
                    value.clone_into(&mut agent.connection);
                    // Another provider has other models: its default, until one is picked.
                    agent.model.clear();
                    agent.effort.clear();
                    if let Some(connection) = self.config.connection(value) {
                        self.want_models(&connection, actions);
                    }
                    self.update_agent(agent, actions);
                }
            }
            Menu::Folder if value == "choose" => actions.push(Action::PickFolder),
            Menu::Folder => self.set_folder(value, actions),
            Menu::FormProvider => {
                if let Some(connection) = self.config.connection(value) {
                    self.want_models(&connection, actions);
                }
                if let Some(Modal::Agent(form)) = &mut self.modal {
                    value.clone_into(&mut form.connection);
                }
            }
            Menu::Branch => self.switch_branch(value, actions),
            Menu::FormAutonomy => {
                if let (Some(Modal::Agent(form)), Some(level)) =
                    (&mut self.modal, Autonomy::ALL.into_iter().find(|level| level.label() == value))
                {
                    form.autonomy = level;
                }
            }
            Menu::FormVoice => {
                if let Some(Modal::Agent(form)) = &mut self.modal {
                    value.clone_into(&mut form.voice);
                }
            }
        }
    }

    /// Draws the open list over everything and handles its clicks.
    #[expect(clippy::many_single_char_names, reason = "p, t, x, y and w are the drawing code's names everywhere")]
    pub(super) fn draw_menu(&mut self, p: &mut Painter, ui: &mut Ui, actions: &mut Vec<Action>) {
        let t = p.theme;
        let Some(state) = &self.menu else { return };
        let (menu, anchor) = (state.menu, state.anchor);
        let mut scroll = state.scroll;
        let items = self.menu_items(menu, state.query.text());
        let searchable = menu.typed() && (items.len() > 7 || !state.query.text().is_empty());
        // The composer's model list also sets the effort, at its foot.
        let thinking = match (menu, self.selected_agent()) {
            (Menu::Model, Some(agent)) => self.connection(agent).map(|connection| {
                let model = self.model_in_use(&connection, &agent.model);
                let in_use = Self::effort_in_use(connection.kind, &model, &agent.effort);
                (Self::effort_choices(connection.kind, &model), in_use)
            }),
            _ => None,
        };
        let (efforts, effort_in_use) = thinking.unwrap_or_default();
        let row_h =
            |item: &Item| (if item.detail.is_empty() { 34.0 } else { 48.0 }) + if item.apart { 9.0 } else { 0.0 };
        let content: f32 = items.iter().map(row_h).sum();
        let search_h = if searchable { 44.0 } else { 0.0 };
        let foot_h = if efforts.is_empty() { 0.0 } else { 78.0 };
        let width = match menu {
            Menu::Model => 452.0,
            Menu::Folder => 400.0,
            Menu::Provider => 340.0,
            Menu::FormAutonomy => anchor.w.max(460.0),
            _ => anchor.w.max(260.0),
        };
        let view = p.clip();
        let room = if menu.below() { view.bottom() - anchor.bottom() - 16.0 } else { anchor.y - view.y - 16.0 };
        let height = (content + search_h + foot_h + 12.0).min(440.0).min(room.max(120.0));
        let x = anchor.x.min(view.right() - width - 12.0).max(view.x + 12.0);
        let y = if menu.below() { anchor.bottom() + 6.0 } else { anchor.y - 8.0 - height };
        let rect = Rect::new(x, y, width, height);
        let appear = ui.appear(id(("menu", menu)));
        p.shadow(Rect::new(rect.x, rect.y + 8.0, rect.w, rect.h), fade(t.shadow, appear), theme::RADIUS, 28.0);
        p.bordered(rect, t.surface, theme::RADIUS, 1.0, t.border_strong);

        let mut picked = None;
        if searchable {
            let field = Rect::new(rect.x + 6.0, rect.y + 6.0, rect.w - 12.0, 32.0);
            icon::draw(p, icon::SEARCH, (field.x + 16.0, field.y + 16.0), 14.0, t.text_faint);
            let query = state.query.text();
            if query.is_empty() {
                p.label("Search, or type a model id", theme::SMALL, field.x + 32.0, field.y + 7.0, t.text_faint);
            } else {
                let w = p.label(query, theme::SMALL, field.x + 32.0, field.y + 7.0, t.text);
                if ui.caret_visible() {
                    p.rect(Rect::new(field.x + 33.0 + w, field.y + 8.0, 1.5, 16.0), t.accent, 0.0);
                }
            }
            p.rect(Rect::new(rect.x, field.bottom() + 5.0, rect.w, 1.0), t.border, 0.0);
        }
        let list = Rect::new(rect.x, rect.y + search_h + 6.0, rect.w, rect.h - search_h - foot_h - 12.0);
        let offset = scroll.update(ui, rect.contains(ui.mouse));
        let clip = p.push_clip(list);
        let mut y = list.y - offset;
        for item in &items {
            if item.apart {
                p.rect(Rect::new(rect.x + 10.0, y + 4.0, rect.w - 20.0, 1.0), t.border, 0.0);
                y += 9.0;
            }
            let h = row_h(item) - if item.apart { 9.0 } else { 0.0 };
            let row = Rect::new(rect.x + 6.0, y, rect.w - 12.0, h - 2.0);
            y += h;
            if row.bottom() < list.y || row.y > list.bottom() {
                continue;
            }
            let hovered = !item.inert && row.contains(ui.mouse) && list.contains(ui.mouse);
            if hovered || item.selected {
                p.rect(row, if hovered { t.hover } else { fade(t.hover, 0.6) }, theme::RADIUS_SM);
            }
            let mut left = row.x + 10.0;
            let ink = if item.inert { t.text_faint } else { t.text_muted };
            if let Some(provider) = &item.provider {
                self.provider_mark(p, provider, (left + 8.0, row.y + row.h * 0.5), 16.0, ink);
                left += 26.0;
            } else if let Some(glyph) = item.glyph {
                icon::draw(p, glyph, (left + 8.0, row.y + row.h * 0.5), 15.0, ink);
                left += 26.0;
            }
            let room = row.right() - 34.0 - left;
            let mut label = p.layout(&item.label, theme::SMALL, None);
            label.truncate(p.fonts, room);
            let color = if item.inert { t.text_faint } else { t.text };
            if item.detail.is_empty() {
                p.text(&label, left, row.y + (row.h - label.height()) * 0.5, color);
            } else {
                p.text(&label, left, row.y + 6.0, color);
                let mut detail = p.layout(&item.detail, theme::TINY, None);
                detail.truncate(p.fonts, room);
                p.text(&detail, left, row.y + 25.0, t.text_faint);
            }
            if item.selected {
                icon::draw(p, icon::CHECK, (row.right() - 16.0, row.y + row.h * 0.5), 14.0, t.text);
            }
            if hovered {
                ui.cursor = CursorIcon::Pointer;
                if ui.released {
                    picked = Some(item.value.clone());
                }
            }
        }
        p.set_clip(clip);
        scroll.finish(ui, content, list.h);

        // Effort: how hard the model thinks, as a row of pills under its list.
        let mut effort = None;
        if !efforts.is_empty() {
            let top = rect.bottom() - foot_h;
            p.rect(Rect::new(rect.x, top, rect.w, 1.0), t.border, 0.0);
            icon::draw(p, icon::BRAIN, (rect.x + 20.0, top + 19.0), 13.0, t.text_faint);
            p.label("EFFORT", theme::CAPTION, rect.x + 32.0, top + 11.0, t.text_muted);
            let mut x = rect.x + 12.0;
            for level in efforts {
                let label = effort_label(&level);
                let w = p.layout(label, theme::SMALL, None).width() + 22.0;
                if x + w > rect.right() - 8.0 {
                    break;
                }
                if chip(p, ui, Rect::new(x, top + 36.0, w, 28.0), label, effort_in_use == level) {
                    effort = Some(level);
                }
                x += w + 6.0;
            }
        }

        // A click anywhere else puts it away.
        let outside = ui.released && !rect.contains(ui.mouse) && !anchor.contains(ui.mouse);
        if let Some(state) = &mut self.menu {
            (state.rect, state.scroll) = (rect, scroll);
        }
        if let Some(level) = effort {
            if let Some(mut agent) = self.selected_agent().cloned() {
                agent.effort = level;
                self.update_agent(agent, actions);
            }
        } else if let Some(value) = picked {
            self.choose(menu, &value, actions);
        } else if outside {
            self.menu = None;
        }
    }

    /// A key while a list is open: Esc closes it, Enter takes the first
    /// choice, and typing narrows the models.
    pub(super) fn menu_key(
        &mut self,
        event: &KeyEvent,
        mods: ModifiersState,
        clipboard: &mut Option<Clipboard>,
        actions: &mut Vec<Action>,
    ) {
        let Some(state) = &mut self.menu else { return };
        let menu = state.menu;
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.menu = None,
            Key::Named(NamedKey::Enter) => {
                let query = state.query.text().to_owned();
                let first = self.menu_items(menu, &query).into_iter().find(|item| !item.inert).map(|item| item.value);
                match first {
                    // With nothing typed, Enter just closes a list of models.
                    Some(value) if !query.trim().is_empty() || !menu.typed() => self.choose(menu, &value, actions),
                    _ => self.menu = None,
                }
            }
            _ if menu.typed() => {
                edit_key(&mut state.query, event, mods, clipboard);
                state.scroll.reset();
            }
            _ => {}
        }
    }
}

/// The family and version of a Claude model id: `claude-opus-5-5` is
/// `("opus", [5, 5])`. `None` for an id that is not shaped like one.
fn model_parts(id: &str) -> Option<(&str, Vec<u32>)> {
    let mut parts = id.strip_prefix("claude-")?.split('-');
    let family = parts.next().filter(|family| !family.is_empty() && family.chars().all(char::is_alphabetic))?;
    // Version numbers follow; a date stamp (or anything else) ends them.
    let version = parts.map_while(|part| part.parse::<u32>().ok().filter(|_| part.len() <= 2)).collect();
    Some((family, version))
}

/// A Claude model's name from its id: `claude-opus-5-5` is "Opus 5.5".
/// `None` for an id that is not shaped like one.
fn model_name(id: &str) -> Option<String> {
    let (family, version) = model_parts(id)?;
    let mut name: String = family.chars().take(1).flat_map(char::to_uppercase).chain(family.chars().skip(1)).collect();
    if !version.is_empty() {
        let version: Vec<String> = version.iter().map(u32::to_string).collect();
        name.push(' ');
        name.push_str(&version.join("."));
    }
    Some(name)
}

/// The last part of a folder's path: what it is called.
pub(super) fn folder_name(folder: &str) -> &str {
    let trimmed = folder.trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if name.is_empty() { folder } else { name }
}

#[cfg(test)]
mod tests {
    use atroxabot_core::{Agent, ClaudeInfo, ClaudeModel, Config, Model};

    use super::*;

    #[test]
    fn every_model_thinks_at_an_effort_of_its_own() {
        let model = |name: &str, efforts: &[&str]| ModelChoice {
            value: name.to_lowercase(),
            label: name.into(),
            efforts: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
            ..ModelChoice::default()
        };
        let all = ["low", "medium", "high", "xhigh", "max"];
        let claude = ConnectionKind::ClaudeAgent;
        let usual = |name: &str| Shell::effort_in_use(claude, &model(name, &all), "");
        assert_eq!((usual("Opus 5.5"), usual("Sonnet 5.5")), ("medium".to_owned(), "high".to_owned()));
        assert_eq!((usual("Haiku 5.5"), usual("Fable 5.2")), ("medium".to_owned(), "medium".to_owned()));
        assert_eq!(Shell::effort_in_use(claude, &model("Opus 5.5", &all), "max"), "max", "a choice stands");
        assert_eq!(
            Shell::effort_in_use(claude, &model("Opus 5.5", &["low", "high"]), "max"),
            "high",
            "unless the model lacks it"
        );
        // A model that takes no effort gets none; Anthropic's API does not list them, and its models take all.
        assert_eq!(Shell::effort_in_use(claude, &model("Haiku", &[]), "high"), "");
        assert_eq!(Shell::effort_in_use(ConnectionKind::OpenAi, &model("gpt", &[]), ""), "");
        assert_eq!(Shell::effort_in_use(ConnectionKind::ClaudeApi, &model("claude-sonnet-5-5", &[]), ""), "high");
    }

    #[test]
    fn providers_are_filed_by_what_they_are() {
        let api =
            |url: &str| Connection { kind: ConnectionKind::OpenAi, base_url: url.into(), ..Connection::default() };
        assert_eq!(icon_key(&Connection::claude_agent()), "claude");
        assert_eq!(icon_key(&api("https://openrouter.ai/api/v1")), "openrouter");
        assert_eq!(icon_key(&api("https://api.OpenAI.com/v1")), "openai");
        assert_eq!(icon_key(&api("http://localhost:11434/v1")), "localhost");
        assert_eq!(icon_key(&api("")), "");
        assert_eq!(Shell::provider_glyph(&api("https://openrouter.ai/api/v1")), icon::ROUTE);
    }

    fn claude(value: &str, resolved: &str, description: &str) -> ClaudeModel {
        ClaudeModel {
            value: value.into(),
            resolved: resolved.into(),
            description: description.into(),
            efforts: vec!["low".into(), "high".into()],
            ..ClaudeModel::default()
        }
    }

    #[test]
    fn claude_models_come_from_claude_code_with_the_default_first() {
        let agent = Agent::new("Scout");
        let mut shell = Shell::new(Config::default(), vec![agent], Vec::new());
        let own = Connection::claude_agent();
        assert!(shell.model_choices(&own).is_empty(), "nothing until Claude Code has answered");
        assert!(shell.model_items(&own, "", "")[0].inert);
        let info = ClaudeInfo {
            models: vec![
                claude("default", "claude-opus-5-5", "Opus 5.5 · Best for everyday, complex tasks"),
                claude("opus", "claude-opus-5-5", "Opus 5.5 · Best for everyday, complex tasks"),
                claude("claude-fable-5-1[1m]", "claude-fable-5-1", "Fable 5.1 · Most capable"),
                claude("fable", "claude-fable-5-2", "Fable 5.2 · Most capable"),
            ],
            ..ClaudeInfo::default()
        };
        shell.claude_info(&own.id, Ok(info));
        let choices = shell.model_choices(&own);
        let names: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(names, ["Opus 5.5", "Fable 5.2"], "aliases once, and only the newest of a family");
        assert_eq!(shell.model_in_use(&own, "").label, "Opus 5.5", "no choice means the default, by its name");
        let items = shell.model_items(&own, "", "");
        assert!(items[0].selected && !items[1].selected, "the default is the one in use");

        let mut actions = Vec::new();
        shell.choose(Menu::Model, "fable", &mut actions);
        assert_eq!(shell.selected_agent().unwrap().model, "fable");
        shell.choose(Menu::Model, "default", &mut actions);
        assert_eq!(shell.selected_agent().unwrap().model, "", "the default is not pinned");

        // An effort chosen for one model does not follow to the next.
        let mut agent = shell.selected_agent().unwrap().clone();
        agent.effort = "high".into();
        shell.update_agent(agent, &mut actions);
        shell.choose(Menu::Model, "fable", &mut actions);
        assert_eq!(shell.selected_agent().unwrap().effort, "");
    }

    #[test]
    fn api_models_are_listed_narrowed_and_typed() {
        let mut agent = Agent::new("Scout");
        agent.connection = "or".into();
        let mut config = Config::default();
        let connection = Connection {
            id: "or".into(),
            kind: ConnectionKind::OpenAi,
            name: "OpenRouter".into(),
            ..Connection::default()
        };
        config.connections.push(connection.clone());
        let mut shell = Shell::new(config, vec![agent], Vec::new());
        let listed = vec![
            Model { id: "x/alpha".into(), name: "Alpha".into(), ..Model::default() },
            Model { id: "y/beta".into(), ..Model::default() },
        ];
        shell.models_loaded("or", Ok(listed));
        let all = shell.model_items(&connection, "", "");
        assert_eq!(all.iter().map(|i| i.value.as_str()).collect::<Vec<_>>(), ["x/alpha", "y/beta"]);
        assert!(all[0].selected, "the first is the default");
        let narrowed = shell.model_items(&connection, "", "ALP");
        assert_eq!(narrowed.iter().map(|i| i.value.as_str()).collect::<Vec<_>>(), ["ALP", "x/alpha"]);

        // Switching provider drops a model the new one does not have.
        let mut actions = Vec::new();
        shell.choose(Menu::Model, "y/beta", &mut actions);
        shell.choose(Menu::Provider, Connection::CLAUDE_AGENT, &mut actions);
        let agent = shell.selected_agent().unwrap();
        assert_eq!((agent.connection.as_str(), agent.model.as_str()), (Connection::CLAUDE_AGENT, ""));
    }

    #[test]
    fn folders_and_efforts_read_well() {
        assert_eq!(folder_name("C:\\code\\parser\\"), "parser");
        assert_eq!(folder_name("/home/me/work"), "work");
        assert_eq!(folder_name("/"), "/");
        assert_eq!((effort_label("medium"), effort_label("xhigh"), effort_label("odd")), ("Medium", "Extra", "odd"));
        assert_eq!(model_name("claude-opus-5-5").as_deref(), Some("Opus 5.5"));
        assert_eq!(model_name("claude-haiku-4-5-20251001").as_deref(), Some("Haiku 4.5"));
        assert_eq!(model_name("claude-fable-5").as_deref(), Some("Fable 5"));
        assert_eq!((model_name("gpt-5"), model_name(""), model_name("claude-")), (None, None, None));
    }
}
