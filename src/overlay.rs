//! The layer-shell overlay surface (libcosmic's iced fork).
//!
//! [`show`] runs an `Overlay`-layer surface that renders the price-check card
//! and exits on Escape/Enter or when it loses focus. [`run_from_file`] is the
//! same, reading the result from a JSON file — used for testing and, later, for
//! when the daemon spawns the overlay as a child process.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context;
use cosmic::iced::core::layout::Limits;
use cosmic::iced::platform_specific::runtime::wayland::layer_surface::SctkLayerSurfaceSettings;
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    self, KeyboardInteractivity, Layer, get_layer_surface,
};
use cosmic::iced::widget::{Column, container, text};
use cosmic::iced::window;
use cosmic::iced::{self, Element, Length, Subscription, Task};

use crate::item::{ParsedItem, Rarity};
use crate::price::PriceQuote;
use crate::result::PriceCheckResult;

const CARD_WIDTH: u32 = 420;
const CARD_HEIGHT: u32 = 460;
const CARD_PADDING: u16 = 12;

/// The result to render, set once by [`show`] before the iced runtime starts.
static RESULT: OnceLock<PriceCheckResult> = OnceLock::new();

/// Show the overlay for a price-check result. Blocks until it is dismissed.
pub fn show(result: PriceCheckResult) -> anyhow::Result<()> {
    RESULT
        .set(result)
        .map_err(|_| anyhow::anyhow!("overlay already initialized in this process"))?;
    iced::daemon(Overlay::new, Overlay::update, Overlay::view)
        .subscription(Overlay::subscription)
        .theme(theme_for_overlay)
        .run()
        .map_err(|e| anyhow::anyhow!("overlay failed: {e}"))
}

/// Show the overlay for a result previously written to `path` as JSON.
pub fn run_from_file(path: &Path) -> anyhow::Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let result: PriceCheckResult =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    show(result)
}

struct Overlay {
    result: PriceCheckResult,
}

#[derive(Debug, Clone)]
enum Message {
    Dismiss,
}

impl Overlay {
    fn new() -> (Self, Task<Message>) {
        let result = RESULT
            .get()
            .cloned()
            .expect("overlay result must be set before show()");

        let init = get_layer_surface(SctkLayerSurfaceSettings {
            id: window::Id::unique(),
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            anchor: layer_surface::Anchor::empty(),
            namespace: "poechk".into(),
            size: Some((Some(CARD_WIDTH), Some(CARD_HEIGHT))),
            size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
            ..Default::default()
        });

        (Self { result }, init)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Dismiss => iced::exit(),
        }
    }

    fn view(&self, _id: window::Id) -> Element<'_, Message> {
        let item = &self.result.item;
        let title = item.name.clone().unwrap_or_else(|| item.base_type.clone());

        let mut col = Column::new().spacing(4).padding(CARD_PADDING);
        col = col.push(text(title).size(20.0));
        if item.name.is_some() {
            col = col.push(text(item.base_type.clone()).size(14.0));
        }
        col = col.push(text(meta_line(item)).size(12.0));

        for parsed in &item.mods {
            col = col.push(text(format!("• {}", parsed.text)).size(13.0));
        }
        if !item.unknown_mods.is_empty() {
            col = col.push(
                text(format!("({} unrecognized line(s))", item.unknown_mods.len())).size(11.0),
            );
        }

        if self.result.quotes.is_empty() {
            col = col.push(text("No listings found").size(12.0));
        } else {
            for (label, count) in price_bands(&self.result.quotes) {
                col = col.push(text(format!("{label}  ×{count}")).size(15.0));
            }
            if self.result.total > 0 {
                col = col.push(text(format!("{} listed", self.result.total)).size(12.0));
            }
        }
        col = col.push(text("Esc to close").size(11.0));

        container(col)
            .padding(CARD_PADDING)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        input_subscription()
    }
}

/// A one-line summary: rarity, class, item level, links, corrupted.
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
            if let iced::keyboard::Key::Named(named) = key {
                use iced::keyboard::key::Named;
                if matches!(named, Named::Escape | Named::Enter) {
                    return Some(Message::Dismiss);
                }
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
