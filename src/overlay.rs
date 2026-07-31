//! The interactive layer-shell overlay.
//!
//! Shows the parsed item with, per affix, a checkbox and editable min/max, plus
//! a Search button. Editing filters only updates state; pressing Search (or the
//! initial open) runs the trade query. The blocking request runs on a worker
//! thread and returns via a oneshot channel awaited by iced, so the UI stays
//! responsive.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context;
use cosmic::iced::alignment::Vertical;
use cosmic::iced::core::layout::Limits;
use cosmic::iced::platform_specific::runtime::wayland::layer_surface::SctkLayerSurfaceSettings;
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    self, KeyboardInteractivity, Layer, get_layer_surface,
};
use cosmic::iced::widget::{
    Column, Row, button, checkbox, container, pick_list, slider, text, text_input,
};
use cosmic::iced::window;
use cosmic::iced::{self, Color, Element, Length, Subscription, Task};

use crate::config::Config;
use crate::item::mods::{ModType, Slot};
use crate::item::{ParsedItem, Rarity};
use crate::price::PriceQuote;
use crate::price::trade::{
    self, FilterSpec, MiscFilters, PriceResult, Status, TradeSource, cached_leagues, fetch_leagues,
};

const CARD_WIDTH: u32 = 540;
const CARD_MAX_HEIGHT: u32 = 1100;
const CARD_PADDING: u16 = 12;

/// Characters of mod text that fit one row at the card width.
const ROW_WRAP_CHARS: usize = 52;

static ITEM: OnceLock<ParsedItem> = OnceLock::new();
static CONFIG: OnceLock<Config> = OnceLock::new();

/// Show the interactive overlay for a parsed item. Blocks until dismissed.
pub fn show(item: ParsedItem, config: Config) -> anyhow::Result<()> {
    ITEM.set(item)
        .map_err(|_| anyhow::anyhow!("overlay already initialized in this process"))?;
    CONFIG
        .set(config)
        .map_err(|_| anyhow::anyhow!("overlay already initialized in this process"))?;
    iced::daemon(Overlay::new, Overlay::update, Overlay::view)
        .subscription(Overlay::subscription)
        .theme(theme_for_overlay)
        .run()
        .map_err(|e| anyhow::anyhow!("overlay failed: {e}"))
}

/// Load a parsed item from a JSON file and show the overlay (for testing).
pub fn run_from_file(path: &Path) -> anyhow::Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let item: ParsedItem =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    show(item, crate::config::load()?)
}

type SearchOutcome = Result<PriceResult, String>;

enum SearchState {
    Idle,
    Searching,
    Done(PriceResult),
    Failed(String),
}

/// Editable filter state for one affix (min/max as text so they can be typed).
struct FilterRow {
    enabled: bool,
    /// Whether a special-type mod (fractured/crafted/…) must match as that
    /// type; toggled off it searches as a plain explicit.
    typed: bool,
    /// Whether to search the mod's per-stat pseudo total (item-wide sum).
    pseudo: bool,
    min: String,
    max: String,
}

/// Corrupted meta-filter cycle: any / corrupted only / uncorrupted only.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Corrupted {
    Any,
    Yes,
    No,
}

impl Corrupted {
    fn next(self) -> Corrupted {
        match self {
            Corrupted::Any => Corrupted::Yes,
            Corrupted::Yes => Corrupted::No,
            Corrupted::No => Corrupted::Any,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Corrupted::Any => "Any",
            Corrupted::Yes => "Yes",
            Corrupted::No => "No",
        }
    }
    fn option(self) -> Option<bool> {
        match self {
            Corrupted::Any => None,
            Corrupted::Yes => Some(true),
            Corrupted::No => Some(false),
        }
    }
}

struct Overlay {
    item: ParsedItem,
    config: Config,
    leagues: Vec<String>,
    /// Bulk-exchange tag: currency/fragments/cards price per unit on the
    /// exchange instead of the item search.
    bulk_tag: Option<String>,
    rows: Vec<FilterRow>,
    /// Minimum sockets / links filters, as editable text ("" = no filter).
    sockets_min: String,
    links_min: String,
    /// Item level bounds, as editable text ("" = no filter).
    ilvl_min: String,
    ilvl_max: String,
    /// Minimum DPS filters (weapons), as editable text ("" = no filter).
    dps_min: String,
    pdps_min: String,
    edps_min: String,
    status: Status,
    corrupted: Corrupted,
    /// Search the exact base type rather than the item's class.
    exact_base: bool,
    /// How far each seeded roll bound reaches past the roll, in steps to the
    /// mod's tier limit. Re-seeds every row's min/max when moved.
    widen: f64,
    /// The trade-site URL for the last search, for "Open in browser".
    trade_url: Option<String>,
    search: SearchState,
}

#[derive(Debug, Clone)]
enum Message {
    SetEnabled(usize, bool),
    ToggleModType(usize),
    TogglePseudo(usize),
    SetMin(usize, String),
    SetMax(usize, String),
    SetSocketsMin(String),
    SetLinksMin(String),
    SetIlvlMin(String),
    SetIlvlMax(String),
    SetDpsMin(String),
    SetPdpsMin(String),
    SetEdpsMin(String),
    SetLeague(String),
    LeaguesLoaded(Vec<String>),
    CycleStatus,
    CycleCorrupted,
    /// Narrow the search from the item's class to its exact base type.
    ToggleExactBase,
    /// Re-seed every roll bound this far past its roll.
    SetWiden(f64),
    Search,
    OpenBrowser,
    Searched(SearchOutcome),
    Dismiss,
}

impl Message {
    /// Whether this message changes what the next search would ask for.
    fn edits_filters(&self) -> bool {
        matches!(
            self,
            Message::SetEnabled(..)
                | Message::ToggleModType(_)
                | Message::TogglePseudo(_)
                | Message::SetMin(..)
                | Message::SetMax(..)
                | Message::SetSocketsMin(_)
                | Message::SetLinksMin(_)
                | Message::SetIlvlMin(_)
                | Message::SetIlvlMax(_)
                | Message::SetDpsMin(_)
                | Message::SetPdpsMin(_)
                | Message::SetEdpsMin(_)
                | Message::SetLeague(_)
                | Message::CycleStatus
                | Message::CycleCorrupted
                | Message::ToggleExactBase
                | Message::SetWiden(_)
        )
    }
}

impl Overlay {
    fn new() -> (Self, Task<Message>) {
        let mut item = ITEM.get().cloned().expect("overlay item set before show()");
        let config = CONFIG.get().cloned().expect("overlay config set before show()");

        // Fold total-res / total-life pseudo rows; their contributors start
        // disabled (the pseudo represents them, and matches spread rolls).
        let subsumed = crate::item::pseudo::fold_pseudo(&mut item);
        let mut defaults = trade::default_filters(&item, trade::DEFAULT_WIDEN);
        for &index in &subsumed {
            if let Some(spec) = defaults.get_mut(index) {
                spec.enabled = false;
            }
        }
        let rows = defaults
            .iter()
            .map(|spec| FilterRow {
                enabled: spec.enabled,
                typed: true,
                pseudo: spec.use_pseudo,
                min: spec.min.map(fmt_amount).unwrap_or_default(),
                max: spec.max.map(fmt_amount).unwrap_or_default(),
            })
            .collect();

        // Explicit size: content autosize (`size: None`) fails to map on
        // cosmic-comp, so estimate the height from the card's actual content.
        let surface = get_layer_surface(SctkLayerSurfaceSettings {
            id: window::Id::unique(),
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            anchor: layer_surface::Anchor::empty(),
            namespace: "poechk".into(),
            size: Some((Some(CARD_WIDTH), Some(estimated_height(&item)))),
            size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
            ..Default::default()
        });
        // No search on open — the user reviews/adjusts filters, then presses
        // Search. (`defaults` above seeded the editable rows.)
        let leagues = {
            let cached = cached_leagues();
            if cached.is_empty() {
                vec![config.league.clone()]
            } else {
                cached
            }
        };

        // Pre-fill sockets/links from the item when they matter (5+ links or
        // 5+ sockets drive price); leave empty (= unfiltered) otherwise.
        let sockets_min = item
            .sockets
            .filter(|&n| n >= 5)
            .map(|n| n.to_string())
            .unwrap_or_default();
        let links_min = item
            .links
            .filter(|&n| n >= 5)
            .map(|n| n.to_string())
            .unwrap_or_default();

        // Item level gates which mod tiers an item can roll, so it prices every
        // item class; seed the floor from the item and let it be edited.
        let ilvl_min = item.item_level.map(|n| n.to_string()).unwrap_or_default();

        // Prefill the dominant DPS kind (floored) — pDPS for physical
        // weapons, eDPS for elemental — leaving the others unfiltered.
        let phys_dominant = item.phys_damage.unwrap_or(0.0) >= item.ele_damage.unwrap_or(0.0);
        let (mut pdps_min, mut edps_min) = (String::new(), String::new());
        match (item.pdps(), item.edps()) {
            (Some(pdps), _) if phys_dominant => pdps_min = fmt_amount(pdps.floor()),
            (_, Some(edps)) => edps_min = fmt_amount(edps.floor()),
            (Some(pdps), None) => pdps_min = fmt_amount(pdps.floor()),
            _ => {}
        }

        let bulk_tag = match item.rarity {
            Some(Rarity::Currency) | Some(Rarity::DivinationCard) => item.trade_tag.clone(),
            _ => None,
        };

        let state = Overlay {
            item,
            config,
            leagues,
            bulk_tag,
            rows,
            sockets_min,
            links_min,
            ilvl_min,
            ilvl_max: String::new(),
            dps_min: String::new(),
            pdps_min,
            edps_min,
            status: Status::InstantBuyout,
            corrupted: Corrupted::Any,
            exact_base: false,
            widen: trade::DEFAULT_WIDEN,
            trade_url: None,
            search: SearchState::Idle,
        };
        (state, Task::batch([surface, leagues_task()]))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        // Any filter edit invalidates the last search: its results priced a
        // different query, and its URL would open that query in the browser
        // rather than what the card now shows.
        if message.edits_filters() {
            self.trade_url = None;
            self.search = SearchState::Idle;
        }
        match message {
            Message::SetEnabled(i, on) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.enabled = on;
                }
                Task::none()
            }
            Message::ToggleModType(i) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.typed = !row.typed;
                }
                Task::none()
            }
            Message::TogglePseudo(i) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.pseudo = !row.pseudo;
                }
                Task::none()
            }
            Message::SetMin(i, value) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.min = value;
                }
                Task::none()
            }
            Message::SetMax(i, value) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.max = value;
                }
                Task::none()
            }
            Message::ToggleExactBase => {
                self.exact_base = !self.exact_base;
                Task::none()
            }
            Message::SetWiden(widen) => {
                self.widen = widen;
                self.reseed_bounds();
                Task::none()
            }
            Message::CycleCorrupted => {
                self.corrupted = self.corrupted.next();
                Task::none()
            }
            Message::CycleStatus => {
                self.status = self.status.next();
                Task::none()
            }
            Message::SetIlvlMin(value) => {
                self.ilvl_min = value;
                Task::none()
            }
            Message::SetIlvlMax(value) => {
                self.ilvl_max = value;
                Task::none()
            }
            Message::Search => {
                self.search = SearchState::Searching;
                if self.bulk_tag.is_some() {
                    bulk_search_task(self.item.base_type.clone(), self.config.clone())
                } else {
                    search_task(
                        self.item.clone(),
                        self.config.clone(),
                        self.specs(),
                        self.misc(),
                    )
                }
            }
            Message::OpenBrowser => {
                if let Some(url) = &self.trade_url {
                    open_url(url);
                }
                Task::none()
            }
            Message::Searched(Ok(result)) => {
                self.trade_url = Some(result.url.clone());
                self.search = SearchState::Done(result);
                Task::none()
            }
            Message::Searched(Err(err)) => {
                self.search = SearchState::Failed(err);
                Task::none()
            }
            Message::SetSocketsMin(value) => {
                self.sockets_min = value;
                Task::none()
            }
            Message::SetLinksMin(value) => {
                self.links_min = value;
                Task::none()
            }
            Message::SetDpsMin(value) => {
                self.dps_min = value;
                Task::none()
            }
            Message::SetPdpsMin(value) => {
                self.pdps_min = value;
                Task::none()
            }
            Message::SetEdpsMin(value) => {
                self.edps_min = value;
                Task::none()
            }
            Message::SetLeague(league) => {
                self.config.league = league;
                if let Err(e) = crate::config::save(&self.config) {
                    tracing::warn!("could not save league to config: {e}");
                }
                Task::none()
            }
            Message::LeaguesLoaded(list) => {
                if !list.is_empty() {
                    self.leagues = list;
                }
                Task::none()
            }
            // Hard-exit rather than iced::exit(): the layer-shell + wayland
            // teardown races with pending async tasks and can segfault. The
            // compositor reclaims the surface when the client disconnects, so
            // no graceful cleanup is needed here.
            Message::Dismiss => std::process::exit(0),
        }
    }

    /// Convert the editable rows into trade filter specs.
    fn specs(&self) -> Vec<FilterSpec> {
        self.rows
            .iter()
            .map(|row| FilterSpec {
                enabled: row.enabled,
                as_explicit: !row.typed,
                use_pseudo: row.pseudo,
                min: row.min.trim().parse().ok(),
                max: row.max.trim().parse().ok(),
            })
            .collect()
    }

    /// The item-level (non-affix) filters from the current controls.
    fn misc(&self) -> MiscFilters {
        MiscFilters {
            status: self.status,
            corrupted: self.corrupted.option(),
            sockets_min: self.sockets_min.trim().parse().ok(),
            links_min: self.links_min.trim().parse().ok(),
            ilvl_min: self.ilvl_min.trim().parse().ok(),
            ilvl_max: self.ilvl_max.trim().parse().ok(),
            dps_min: self.dps_min.trim().parse().ok(),
            pdps_min: self.pdps_min.trim().parse().ok(),
            edps_min: self.edps_min.trim().parse().ok(),
            exact_base: self.exact_base,
        }
    }

    /// How far each seeded roll bound reaches past its roll. Which affixes
    /// matter is the checkboxes' job — a listing has to match every ticked one.
    fn roll_slider(&self) -> Element<'_, Message> {
        let label = match self.widen {
            w if w <= 0.0 => "exact rolls".to_string(),
            w if (w - 1.0).abs() < f64::EPSILON => "tier rolls".to_string(),
            w => format!("tier rolls ×{w:.2}"),
        };
        Row::new()
            .spacing(8)
            .align_y(Vertical::Center)
            .push(text("Rolls").size(11.0).color(SECTION_COLOR).width(Length::Fixed(52.0)))
            .push(
                slider(0.0..=2.0, self.widen, Message::SetWiden)
                    .step(0.25)
                    .width(Length::Fixed(150.0)),
            )
            .push(text(label).size(11.0).color(SECTION_COLOR))
            .into()
    }

    /// Re-seed every row's min/max at the current widening.
    ///
    /// Only the bounds: which rows are ticked, and whether each searches as its
    /// own type or as a pseudo total, are the user's and survive the slider.
    /// Hand-typed bounds do not — re-seeding is what the slider is for.
    fn reseed_bounds(&mut self) {
        for (row, spec) in self
            .rows
            .iter_mut()
            .zip(trade::default_filters(&self.item, self.widen))
        {
            row.min = spec.min.map(fmt_amount).unwrap_or_default();
            row.max = spec.max.map(fmt_amount).unwrap_or_default();
        }
    }

    /// The item's class name, when its class is one the trade site can be
    /// searched by. `None` means the base type is the only search there is.
    fn searchable_class(&self) -> Option<&str> {
        crate::price::category::trade_category(&self.item)?;
        self.item.category.as_deref()
    }

    fn view(&self, _id: window::Id) -> Element<'_, Message> {
        let item = &self.item;
        let title = item.name.clone().unwrap_or_else(|| item.base_type.clone());

        let mut col = Column::new().spacing(4).padding(CARD_PADDING);
        col = col.push(text(title).size(20.0));
        if item.name.is_some() {
            col = col.push(text(item.base_type.clone()).size(14.0));
        }
        col = col.push(text(meta_line(item)).size(12.0));

        col = col.push(
            Row::new()
                .spacing(6)
                .align_y(Vertical::Center)
                .push(text("League:").size(12.0))
                .push(
                    pick_list(
                        self.leagues.clone(),
                        Some(self.config.league.clone()),
                        Message::SetLeague,
                    )
                    .text_size(13.0)
                    .padding(4),
                ),
        );

        let bulk = self.bulk_tag.is_some();
        if !bulk && let Some(dps) = item.total_dps() {
            let mut parts = vec![format!("DPS {}", fmt_amount(dps))];
            if let Some(pdps) = item.pdps() {
                parts.push(format!("pDPS {}", fmt_amount(pdps)));
            }
            if let Some(edps) = item.edps() {
                parts.push(format!("eDPS {}", fmt_amount(edps)));
            }
            if let Some(aps) = item.aps {
                parts.push(format!("{aps} APS"));
            }
            col = col.push(text(parts.join(" · ")).size(12.0).color(PSEUDO_COLOR));
            col = col.push(
                Row::new()
                    .spacing(6)
                    .align_y(Vertical::Center)
                    .push(text("pDPS ≥").size(12.0))
                    .push(
                        text_input("any", &self.pdps_min)
                            .on_input(Message::SetPdpsMin)
                            .size(12.0)
                            .padding(4)
                            .width(Length::Fixed(56.0)),
                    )
                    .push(text("eDPS ≥").size(12.0))
                    .push(
                        text_input("any", &self.edps_min)
                            .on_input(Message::SetEdpsMin)
                            .size(12.0)
                            .padding(4)
                            .width(Length::Fixed(56.0)),
                    )
                    .push(text("DPS ≥").size(12.0))
                    .push(
                        text_input("any", &self.dps_min)
                            .on_input(Message::SetDpsMin)
                            .size(12.0)
                            .padding(4)
                            .width(Length::Fixed(56.0)),
                    ),
            );
        }

        if !bulk && item.item_level.is_some() {
            col = col.push(
                Row::new()
                    .spacing(6)
                    .align_y(Vertical::Center)
                    .push(text("Item Level ≥").size(12.0))
                    .push(
                        text_input("any", &self.ilvl_min)
                            .on_input(Message::SetIlvlMin)
                            .size(12.0)
                            .padding(4)
                            .width(Length::Fixed(44.0)),
                    )
                    .push(text("≤").size(12.0))
                    .push(
                        text_input("any", &self.ilvl_max)
                            .on_input(Message::SetIlvlMax)
                            .size(12.0)
                            .padding(4)
                            .width(Length::Fixed(44.0)),
                    ),
            );
        }

        if !bulk && item.sockets.is_some() {
            col = col.push(
                Row::new()
                    .spacing(6)
                    .align_y(Vertical::Center)
                    .push(text("Sockets ≥").size(12.0))
                    .push(
                        text_input("any", &self.sockets_min)
                            .on_input(Message::SetSocketsMin)
                            .size(12.0)
                            .padding(4)
                            .width(Length::Fixed(44.0)),
                    )
                    .push(text("Links ≥").size(12.0))
                    .push(
                        text_input("any", &self.links_min)
                            .on_input(Message::SetLinksMin)
                            .size(12.0)
                            .padding(4)
                            .width(Length::Fixed(44.0)),
                    ),
            );
        }

        if !bulk {
            for (title, indices) in affix_sections(item) {
                if indices.is_empty() {
                    continue;
                }
                col = col.push(text(title).size(10.0).color(SECTION_COLOR));
                for index in indices {
                    col = col.push(self.affix_row(index));
                }
            }
        }

        let mut controls = Row::new().spacing(8).push(
            button(text("Search").size(14.0))
                .on_press(Message::Search)
                .padding([6, 16]),
        );
        if !bulk {
            controls = controls
                .push(
                    button(text(self.status.label()).size(13.0))
                        .on_press(Message::CycleStatus)
                        .padding([6, 10]),
                )
                .push(
                    button(text(format!("Corrupted: {}", self.corrupted.label())).size(13.0))
                        .on_press(Message::CycleCorrupted)
                        .padding([6, 10]),
                );
        }
        col = col.push(controls);
        // Its own row: "Class: One-Handed Sword" alongside the three controls
        // above overruns the card. Only offered when the item has a class to
        // widen to — for a map the base type is the search, with nothing to toggle.
        if !bulk && let Some(class) = self.searchable_class() {
            let (label, hint) = if self.exact_base {
                (format!("Base: {}", item.base_type), "searching this base only")
            } else {
                (format!("Class: {class}"), "searching the whole class")
            };
            col = col.push(
                Row::new()
                    .spacing(8)
                    .align_y(Vertical::Center)
                    .push(
                        button(text(label).size(13.0))
                            .on_press(Message::ToggleExactBase)
                            .padding([6, 10]),
                    )
                    .push(text(hint).size(11.0).color(SECTION_COLOR)),
            );
        }
        if !bulk {
            col = col.push(self.roll_slider());
        }
        col = col.push(text("────────").size(10.0));
        col = col.push(results_view(&self.search));
        if self.trade_url.is_some() {
            col = col.push(
                button(text("Open in browser ↗").size(13.0))
                    .on_press(Message::OpenBrowser)
                    .padding([6, 10]),
            );
        }
        col = col.push(text("Esc to close").size(10.0));

        container(col)
            .padding(CARD_PADDING)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        input_subscription()
    }

    /// One affix as `[✓] [badge] text [min] [max]`.
    fn affix_row(&self, index: usize) -> Element<'_, Message> {
        let parsed = &self.item.mods[index];
        let state = &self.rows[index];
        let mut row = Row::new()
            .spacing(6)
            .align_y(Vertical::Center)
            .push(checkbox(state.enabled).on_toggle(move |on| Message::SetEnabled(index, on)));
        if let Some((tag, color)) = type_badge(parsed.mod_type) {
            let typed = state.typed;
            row = row.push(
                button(text(tag).size(10.0))
                    .on_press(Message::ToggleModType(index))
                    .padding([2, 6])
                    .style(move |_theme, _status| badge_style(color, typed)),
            );
        }
        if !parsed.pseudo_ids.is_empty() {
            let pseudo_on = state.pseudo;
            row = row.push(
                button(text("pseudo").size(10.0))
                    .on_press(Message::TogglePseudo(index))
                    .padding([2, 6])
                    .style(move |_theme, _status| badge_style(PSEUDO_COLOR, pseudo_on)),
            );
        }
        row = row.push(text(parsed.text.clone()).size(13.0).width(Length::Fill));
        // An option mod (a cluster jewel enchant, an allocated notable) matches
        // by identity, so there is no range to type.
        if parsed.option.is_some() {
            return row.into();
        }
        row.push(
            text_input("min", &state.min)
                .on_input(move |v| Message::SetMin(index, v))
                .size(12.0)
                .padding(4)
                .width(Length::Fixed(56.0)),
        )
        .push(
            text_input("max", &state.max)
                .on_input(move |v| Message::SetMax(index, v))
                .size(12.0)
                .padding(4)
                .width(Length::Fixed(56.0)),
        )
        .into()
    }
}

const SECTION_COLOR: Color = Color::from_rgb(0.55, 0.55, 0.55);
const PSEUDO_COLOR: Color = Color::from_rgb(0.42, 0.76, 0.65);

/// Estimate the surface height from the card's content so tall items get room
/// and small items stay compact.
fn estimated_height(item: &ParsedItem) -> u32 {
    // Title, base line, meta line, league row, padding.
    let mut height: u32 = 165;
    if item.total_dps().is_some() {
        height += 58; // DPS readout + filter row
    }
    if item.item_level.is_some() {
        height += 32; // item level row
    }
    if item.sockets.is_some() {
        height += 32; // sockets/links row
    }
    for (_, indices) in affix_sections(item) {
        if !indices.is_empty() {
            height += 17; // section header
        }
    }
    for parsed in &item.mods {
        height += if parsed.text.len() > ROW_WRAP_CHARS { 46 } else { 28 };
    }
    height += 44; // Search / status / corrupted controls
    height += 26; // roll-widening slider
    if crate::price::category::trade_category(item).is_some() {
        height += 40; // class / base type toggle row
    }
    height += 130; // divider, results area, browser button, footer
    height.clamp(280, CARD_MAX_HEIGHT)
}

/// Bucket mod indices into display sections. Prefix/suffix slots come from the
/// advanced clipboard format; standard copies land in a flat "Mods" section.
fn affix_sections(item: &ParsedItem) -> [(&'static str, Vec<usize>); 6] {
    let mut pseudo = Vec::new();
    let mut enchants = Vec::new();
    let mut implicits = Vec::new();
    let mut prefixes = Vec::new();
    let mut suffixes = Vec::new();
    let mut other = Vec::new();
    for (index, parsed) in item.mods.iter().enumerate() {
        match (parsed.mod_type, parsed.slot) {
            (ModType::Pseudo, _) => pseudo.push(index),
            (ModType::Enchant, _) => enchants.push(index),
            (ModType::Implicit, _) => implicits.push(index),
            (_, Some(Slot::Prefix)) => prefixes.push(index),
            (_, Some(Slot::Suffix)) => suffixes.push(index),
            _ => other.push(index),
        }
    }
    [
        ("Pseudo totals", pseudo),
        ("Enchants", enchants),
        ("Implicits", implicits),
        ("Prefixes", prefixes),
        ("Suffixes", suffixes),
        ("Mods", other),
    ]
}

/// Badge-button style: pressed-in (filled) while the special type must match,
/// outlined once downgraded to plain explicit.
fn badge_style(color: Color, active: bool) -> cosmic::iced::widget::button::Style {
    use cosmic::iced::widget::button::Style;
    if active {
        Style {
            background: Some(iced::Background::Color(color)),
            text_color: Color::from_rgb(0.09, 0.09, 0.11),
            border_radius: 4.0.into(),
            ..Style::default()
        }
    } else {
        Style {
            background: None,
            text_color: SECTION_COLOR,
            border_radius: 4.0.into(),
            border_width: 1.0,
            border_color: SECTION_COLOR,
            ..Style::default()
        }
    }
}

/// A small colored tag for affix provenance that matters when pricing.
fn type_badge(mod_type: ModType) -> Option<(&'static str, Color)> {
    match mod_type {
        ModType::Crafted => Some(("crafted", Color::from_rgb8(0x9c, 0x9c, 0xf0))),
        ModType::Fractured => Some(("fractured", Color::from_rgb8(0xc8, 0xa0, 0x5a))),
        ModType::Veiled => Some(("veiled", Color::from_rgb8(0xb0, 0x6a, 0xd0))),
        ModType::Scourge => Some(("scourge", Color::from_rgb8(0xd0, 0x64, 0x50))),
        ModType::Enchant | ModType::Implicit | ModType::Explicit | ModType::Pseudo => None,
    }
}

/// Look up the poe.ninja price on a worker thread; deliver the result to iced.
///
/// Currency and other exchangeables trade on the in-game currency exchange,
/// whose rates poe.ninja publishes — trade-site bulk listings are leftovers,
/// so they are not queried at all.
fn bulk_search_task(name: String, config: Config) -> Task<Message> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let outcome = match crate::price::ninja::chaos_value(&config.league, &name) {
            Some(ninja) => Ok(PriceResult {
                url: format!(
                    "https://www.pathofexile.com/trade/exchange/{}",
                    config.league.replace(' ', "%20")
                ),
                total: 0,
                quotes: Vec::new(),
                ninja_chaos: Some(ninja),
            }),
            None => Err(format!("poe.ninja has no price for \"{name}\"")),
        };
        let _ = tx.send(outcome);
    });
    Task::perform(
        async move { rx.await.unwrap_or_else(|_| Err("search cancelled".to_string())) },
        Message::Searched,
    )
}

/// Run the trade search on a worker thread; deliver the result to iced.
fn search_task(
    item: ParsedItem,
    config: Config,
    filters: Vec<FilterSpec>,
    misc: MiscFilters,
) -> Task<Message> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let outcome = TradeSource::new(&config)
            .price(&item, &filters, &misc)
            .map_err(|e| e.to_string());
        let _ = tx.send(outcome);
    });
    Task::perform(
        async move { rx.await.unwrap_or_else(|_| Err("search cancelled".to_string())) },
        Message::Searched,
    )
}

/// Open a URL in the default browser.
fn open_url(url: &str) {
    if let Err(e) = std::process::Command::new("xdg-open").arg(url).spawn() {
        tracing::warn!("could not open browser ({e})");
    }
}

/// Refresh the league list from the API on a worker thread.
fn leagues_task() -> Task<Message> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch_leagues().unwrap_or_default());
    });
    Task::perform(
        async move { rx.await.unwrap_or_default() },
        Message::LeaguesLoaded,
    )
}

fn results_view(search: &SearchState) -> Element<'_, Message> {
    match search {
        SearchState::Idle => text("Press Search to price").size(13.0).into(),
        SearchState::Searching => text("Searching…").size(13.0).into(),
        SearchState::Failed(err) => text(format!("Search failed: {err}")).size(12.0).into(),
        SearchState::Done(result) => {
            let mut col = Column::new().spacing(2);
            if let Some(ninja) = result.ninja_chaos {
                col = col.push(text(format!("≈ {} chaos", fmt_amount(ninja))).size(17.0));
                col = col.push(
                    text("poe.ninja · in-game exchange rate")
                        .size(10.0)
                        .color(SECTION_COLOR),
                );
            } else if result.quotes.is_empty() {
                col = col.push(text(format!("No listings ({} matched)", result.total)).size(13.0));
            } else {
                for (label, count) in price_bands(&result.quotes) {
                    col = col.push(text(format!("{label}  ×{count}")).size(16.0));
                }
                col = col.push(text(format!("{} listed", result.total)).size(12.0));
            }
            col.into()
        }
    }
}

/// A one-line summary: rarity, class, item level, links, corrupted, fractured.
fn meta_line(item: &ParsedItem) -> String {
    let mut parts = vec![item.rarity.map_or("Unknown", rarity_label).to_string()];
    if !item.item_class.is_empty() {
        parts.push(item.item_class.clone());
    }
    if let Some(ilvl) = item.item_level {
        parts.push(format!("iLvl {ilvl}"));
    }
    if let Some(links) = item.links
        && links >= 5
    {
        parts.push(format!("{links}L"));
    }
    if item.corrupted {
        parts.push("Corrupted".to_string());
    }
    if item.fractured {
        parts.push("Fractured".to_string());
    }
    parts.join(" · ")
}

/// Collapse the cheapest-first quotes into "amount currency" -> count bands,
/// keeping the cheapest three.
fn price_bands(quotes: &[PriceQuote]) -> Vec<(String, usize)> {
    let mut bands: Vec<(String, usize)> = Vec::new();
    for quote in quotes {
        let label = format!("{} {}", fmt_amount(quote.amount), quote.currency);
        match bands.last_mut() {
            Some(last) if last.0 == label => last.1 += 1,
            _ => bands.push((label, 1)),
        }
    }
    bands.truncate(3);
    bands
}

fn fmt_amount(amount: f64) -> String {
    if amount.fract() == 0.0 {
        format!("{}", amount as i64)
    } else if amount < 1.0 {
        format!("{amount:.2}")
    } else {
        format!("{amount:.1}")
    }
}

fn rarity_label(rarity: Rarity) -> &'static str {
    match rarity {
        Rarity::Normal => "Normal",
        Rarity::Magic => "Magic",
        Rarity::Rare => "Rare",
        Rarity::Unique => "Unique",
        Rarity::Gem => "Gem",
        Rarity::Currency => "Currency",
        Rarity::DivinationCard => "Divination Card",
        Rarity::Quest => "Quest",
        Rarity::Unknown => "Unknown",
    }
}

fn input_subscription() -> Subscription<Message> {
    cosmic::iced::event::listen_with(|event, _status, _id| match &event {
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, .. }) => {
            if let iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) = key {
                return Some(Message::Dismiss);
            }
            None
        }
        iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
            iced::event::wayland::Event::Layer(
                iced::event::wayland::LayerEvent::Unfocused,
                _,
                _,
            ),
        )) => Some(Message::Dismiss),
        _ => None,
    })
}

fn theme_for_overlay(_state: &Overlay, _id: window::Id) -> iced::Theme {
    detect_cosmic_theme()
}

fn detect_cosmic_theme() -> iced::Theme {
    let Ok(mode_config) = cosmic::cosmic_theme::ThemeMode::config() else {
        return iced::Theme::Dark;
    };
    match cosmic::cosmic_theme::ThemeMode::is_dark(&mode_config) {
        Ok(false) => iced::Theme::Light,
        Ok(true) | Err(_) => iced::Theme::Dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_filter_edit_invalidates_the_last_search() {
        // Changing a filter and pressing "Open in browser" must not reopen the
        // previous query — the reported symptom was toggling Instant Buyout and
        // getting the pre-toggle search in the browser.
        let edits = [
            Message::CycleStatus,
            Message::CycleCorrupted,
            Message::ToggleExactBase,
            Message::SetEnabled(0, false),
            Message::ToggleModType(0),
            Message::TogglePseudo(0),
            Message::SetMin(0, "5".into()),
            Message::SetMax(0, "9".into()),
            Message::SetSocketsMin("6".into()),
            Message::SetLinksMin("6".into()),
            Message::SetIlvlMin("84".into()),
            Message::SetIlvlMax("86".into()),
            Message::SetDpsMin("400".into()),
            Message::SetPdpsMin("100".into()),
            Message::SetEdpsMin("300".into()),
            Message::SetLeague("Standard".into()),
        ];
        for message in edits {
            assert!(message.edits_filters(), "{message:?} must invalidate");
        }

        // Messages that don't change the query must leave the results standing.
        let keeps = [
            Message::Search,
            Message::OpenBrowser,
            Message::Dismiss,
            Message::LeaguesLoaded(vec!["Standard".into()]),
            Message::Searched(Err("boom".into())),
        ];
        for message in keeps {
            assert!(!message.edits_filters(), "{message:?} must not invalidate");
        }
    }
}
