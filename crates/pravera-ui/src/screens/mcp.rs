//! The agent bus: a loopback MCP server that lets a local AI drive this machine.
//!
//! The registry lives in `pravera-mcp` and is dynamic — `tools/list` serves
//! whatever is registered at the moment it is asked. This screen is the only
//! place the bus is toggled.
//!
//! ## Layout
//!
//! Two cards under the header. On the left, narrow and fixed, everything
//! about the connection: whether it is on, where it listens, and the snippet
//! a client needs. On the right, filling the rest, what an agent can do once
//! it is connected: every tool, grouped by what it touches, with the ones
//! that reach past Pravera itself marked. Each card scrolls by itself, so the
//! switch never scrolls away while the tool list is being read.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use iced::widget::{column, container, row, text};
use iced::{Alignment, Element, Length};

use crate::components::{self, stat, Tone};
use crate::icon;
use crate::motion::{self, HoverTracker};
use crate::net::mcp::Mcp;
use crate::theme::{self, tokens as t};
use pravera_mcp::ToolSpec;

/// Hover slots for this screen.
const SLOT_TOGGLE: usize = 0;
const HOVER_SLOTS: usize = 1;

/// The connection card's width. Enough for the snippet's longest line and a
/// fact column beside its label; the tools take everything else.
const CONNECTION_WIDTH: f32 = 400.0;

/// The order tool groups are listed in. Anything the registry adds later
/// lands after these, alphabetically.
const GROUP_ORDER: [&str; 5] = ["session", "host", "files", "discovery", "input"];

#[derive(Debug, Clone)]
pub enum Message {
    /// The switch was flipped; `true` means "start listening".
    Toggle(bool),
    Hover(usize, bool),
    CopySnippet,
}

pub struct State {
    glue: Mcp,
    hover: HoverTracker,
    /// The switch's travel. Synced from the bus after every update rather
    /// than adopted while drawing, so the frame subscription sees it move.
    switches: HashMap<usize, iced::Animation<bool>>,
    error: Option<String>,
    copied: bool,
    /// When the page last arrived; its panels cascade in from here.
    arrived: Instant,
}

impl State {
    pub fn new() -> State {
        let mut glue = Mcp::new();
        let error = glue.seed().err();
        State {
            glue,
            hover: HoverTracker::new(HOVER_SLOTS),
            switches: HashMap::new(),
            error,
            copied: false,
            arrived: Instant::now(),
        }
    }

    /// Start the page's entrance at `at`.
    pub fn replay(&mut self, at: Instant) {
        self.arrived = at;
    }

    /// Point the switch at whether the bus is listening now.
    pub fn sync_switch(&mut self, now: Instant) {
        let on = self.glue.is_running();
        let animation = self
            .switches
            .entry(SLOT_TOGGLE)
            .or_insert_with(|| motion::standard(on));
        if animation.value() != on {
            animation.go_mut(on, now);
        }
    }

    pub fn is_running(&self) -> bool {
        self.glue.is_running()
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.glue.local_addr()
    }

    pub fn tools(&self) -> Vec<Arc<ToolSpec>> {
        self.glue.tools()
    }

    pub fn glue(&self) -> &Mcp {
        &self.glue
    }

    pub fn glue_mut(&mut self) -> &mut Mcp {
        &mut self.glue
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    pub fn set_copied(&mut self, copied: bool) {
        self.copied = copied;
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.hover.is_animating(now)
            || self.switches.values().any(|a| a.is_animating(now))
            || motion::cascading(self.arrived, now)
    }

    fn switch_travel(&self, slot: usize, on: bool, now: Instant) -> f32 {
        match self.switches.get(&slot) {
            Some(animation) if animation.value() == on => animation.interpolate(0.0, 1.0, now),
            _ => {
                if on {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

impl Default for State {
    fn default() -> Self {
        State::new()
    }
}

/// Handle a UI message. Returns `Some` when the application must act
/// (start/stop or copy), `None` for pure hover.
pub fn update(state: &mut State, message: Message, now: Instant) -> Option<Message> {
    match message {
        Message::Hover(slot, entering) => {
            state.hover.set(slot, entering, now);
            // The confirmation belongs to the moment of copying; the pointer
            // moving on is the moment passing.
            state.copied = false;
            None
        }
        Message::Toggle(on) => {
            state.error = None;
            state.copied = false;
            Some(Message::Toggle(on))
        }
        Message::CopySnippet => Some(Message::CopySnippet),
    }
}

pub fn view<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    let since = state.arrived;
    let running = state.is_running();

    let mut header = components::header("Agent").meta(if running {
        components::pill("Listening", Tone::Success)
    } else {
        components::pill("Off", Tone::Neutral)
    });
    header = header.meta(
        text(match state.local_addr() {
            Some(addr) => addr.to_string(),
            None => "MCP over loopback".to_string(),
        })
        .size(t::TEXT_XS)
        .font(t::FONT_MONO)
        .wrapping(text::Wrapping::None)
        .style(theme::muted),
    );

    components::page_split(
        motion::settle(header, motion::cascade(since, now, 0)),
        motion::settle(components::body(connection(state, now)), motion::cascade(since, now, 1)),
        CONNECTION_WIDTH,
        motion::settle(components::body(toolbox(state)), motion::cascade(since, now, 2)),
    )
}

/// Whether the bus is on, where it listens, and how a client finds it.
fn connection<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    let running = state.is_running();
    let addr = state.local_addr();

    let hero = if running {
        components::hero(
            icon::AGENT,
            t::LIME,
            "Listening on loopback",
            "Agents on this machine can connect now.",
        )
    } else {
        components::hero(
            icon::AGENT,
            t::MUTED_FOREGROUND,
            "Not listening",
            "Nothing can connect until the bus is on.",
        )
    };

    // A row that is the switch. It draws nothing until the pointer is on it.
    let switch = components::switch_row(
        "Let AI agents control this machine",
        "Opens the tool bus on 127.0.0.1. Nothing on the network can reach it.",
        state.switch_travel(SLOT_TOGGLE, running, now),
        state.hover.amount(SLOT_TOGGLE, now),
        Message::Toggle(!running),
        Message::Hover(SLOT_TOGGLE, true),
        Message::Hover(SLOT_TOGGLE, false),
    );

    let mut body = column![hero, switch]
    .spacing(t::SPACE_4)
    .width(Length::Fill);

    if let Some(error) = state.error() {
        body = body.push(components::callout(icon::ALERT, error, Tone::Danger));
    }

    let address = match addr {
        Some(addr) => addr.to_string(),
        None => format!("127.0.0.1:{}", pravera_mcp::DEFAULT_PORT),
    };
    body = body.push(components::section(
        "Endpoint",
        column![
            components::fact("Protocol", value("JSON-RPC 2.0, a line each")),
            components::fact(
                "Address",
                row![
                    text(address)
                        .size(t::TEXT_XS)
                        .font(t::FONT_MONO_STRONG)
                        .wrapping(text::Wrapping::None)
                        .style(theme::tinted(if running { t::FOREGROUND } else { t::NEUTRAL_300 })),
                    text(if running { "" } else { "when on" })
                        .size(t::TEXT_XS)
                        .wrapping(text::Wrapping::None)
                        .style(theme::subtle),
                ]
                .spacing(t::SPACE_2)
                .align_y(Alignment::Center),
            ),
            components::fact("Reach", value("This machine only")),
        ]
        .spacing(t::SPACE_2 + 2.0),
    ));

    body = body.push(components::section(
        "Client config",
        column![
            components::code_block(
                pretty(&snippet_for(addr)),
                Some(components::copy_button(state.copied, Message::CopySnippet)),
            ),
            components::note(if running {
                "Point any MCP client that speaks the TCP line transport at this address."
            } else {
                "If the port is taken when the bus starts, another is chosen and this updates."
            }),
        ]
        .spacing(t::SPACE_2),
    ));

    body.push(components::callout(
        icon::SHIELD,
        "This is full control. A connected agent can move the pointer, type into the focused window, read the clipboard and list files. Only turn it on for agents on this machine that you trust.",
        Tone::Warning,
    ))
    .into()
}

/// Everything an agent can call, grouped by what it touches.
fn toolbox<'a>(state: &'a State) -> Element<'a, Message> {
    let tools = state.tools();
    if tools.is_empty() {
        return container(components::empty(
            icon::AGENT,
            "No tools registered",
            "The bus seeds nine tools at startup. Restart Pravera, and check the log if this persists.",
        ))
        .center_x(Length::Fill)
        .padding([t::SPACE_16, 0.0])
        .into();
    }

    let total = tools.len();
    let sensitive = tools.iter().filter(|tool| tool.sensitive).count();
    let groups = grouped(tools);

    // One line of figures, not a strip of boxes: what is on the bus, and how
    // much of it needs care.
    let mut figures = row![stat::figure(total, "tools", t::FOREGROUND)]
        .spacing(t::SPACE_2)
        .align_y(Alignment::Center);
    figures = figures.push(stat::separator());
    figures = figures.push(stat::figure(
        sensitive,
        "sensitive",
        if sensitive > 0 { t::DESTRUCTIVE_TEXT } else { t::FOREGROUND },
    ));
    figures = figures.push(stat::separator());
    figures = figures.push(stat::figure(groups.len(), "groups", t::FOREGROUND));

    let mut body = column![figures].spacing(t::SPACE_6).width(Length::Fill);

    for (group, tools) in groups {
        let count = match tools.len() {
            1 => "1 tool".to_string(),
            n => format!("{n} tools"),
        };
        // The rows are split by hairlines and have no card around them: the
        // label and the count say where a group starts.
        let mut list = column![].width(Length::Fill);
        for (index, tool) in tools.iter().enumerate() {
            if index > 0 {
                list = list.push(components::hairline());
            }
            list = list.push(tool_row(tool));
        }
        body = body.push(
            column![
                components::section_head(
                    &group,
                    text(count).size(t::TEXT_XS).wrapping(text::Wrapping::None).style(theme::subtle),
                ),
                components::hairline(),
                list,
            ]
            .spacing(0.0),
        );
    }

    body.into()
}

/// One tool: its name, what it does, and what it takes.
fn tool_row<'a>(tool: &ToolSpec) -> Element<'a, Message> {
    let mut title = row![text(tool.name.clone())
        .size(t::TEXT_SM)
        .font(t::FONT_MONO_STRONG)
        .wrapping(text::Wrapping::None)
        .style(theme::heading)]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center);
    if tool.sensitive {
        title = title.push(components::pill("sensitive", Tone::Danger));
    }

    let params = params_of(&tool.parameters);
    let takes: Element<'a, Message> = if params.is_empty() {
        text("no parameters").size(t::TEXT_XS).style(theme::subtle).into()
    } else {
        // Required parameters are filled; optional ones are outlined and
        // carry the question mark a TypeScript signature would.
        row(params.into_iter().map(|(name, required)| {
            if required {
                components::pill(name, Tone::Neutral)
            } else {
                components::pill(format!("{name}?"), Tone::Outline)
            }
        }))
        .spacing(t::SPACE_1)
        .wrap()
        .vertical_spacing(t::SPACE_1)
        .into()
    };

    column![
        title,
        text(tool.description.clone())
            .size(t::TEXT_XS)
            .width(Length::Fill)
            .style(theme::muted),
        takes,
    ]
    .spacing(t::SPACE_1_5)
    .padding([t::SPACE_3, 0.0])
    .width(Length::Fill)
    .into()
}

/// A plain value in a fact row.
fn value<'a>(words: &'a str) -> Element<'a, Message> {
    text(words)
        .size(t::TEXT_XS)
        .style(theme::tinted(t::NEUTRAL_200))
        .into()
}

/// The tools, grouped by category in [`GROUP_ORDER`].
fn grouped(tools: Vec<Arc<ToolSpec>>) -> Vec<(String, Vec<Arc<ToolSpec>>)> {
    let mut by_group: BTreeMap<String, Vec<Arc<ToolSpec>>> = BTreeMap::new();
    for tool in tools {
        by_group.entry(tool.category.clone()).or_default().push(tool);
    }
    let mut groups = Vec::new();
    for key in GROUP_ORDER {
        if let Some(tools) = by_group.remove(key) {
            groups.push((key.to_string(), tools));
        }
    }
    groups.extend(by_group);
    groups
}

/// Each parameter a tool's schema declares, and whether it is required, in
/// the schema's order.
fn params_of(schema: &serde_json::Value) -> Vec<(String, bool)> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    schema
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.keys()
                .map(|name| (name.clone(), required.contains(&name.as_str())))
                .collect()
        })
        .unwrap_or_default()
}

/// The parameters as one line, required ones starred.
#[cfg(test)]
fn summarize_params(schema: &serde_json::Value) -> String {
    let params = params_of(schema);
    if params.is_empty() {
        return "no parameters".to_string();
    }
    let parts: Vec<String> = params
        .into_iter()
        .map(|(name, required)| if required { format!("{name}*") } else { name })
        .collect();
    format!("params: {}", parts.join(", "))
}

/// The client-config snippet for the current listen state, shown on screen
/// and written to the clipboard by [`Message::CopySnippet`].
pub fn snippet_for(addr: Option<SocketAddr>) -> String {
    match addr {
        Some(a) => {
            // TCP line protocol on loopback. Clients that speak the MCP
            // line protocol connect directly; HTTP/SSE clients would need a
            // different transport. The snippet shows the concrete address the
            // listener actually bound to, and an example JSON for a TCP client.
            format!(
                r#"{{"mcpServers":{{"pravera":{{"type":"tcp","host":"127.0.0.1","port":{}}}}}}}"#,
                a.port()
            )
        }
        None => r#"{"mcpServers":{"pravera":{"command":"pravera","args":["--mcp"]}}}"#.to_string(),
    }
}

/// The snippet laid out for reading. The clipboard gets the compact form;
/// every client accepts either.
fn pretty(snippet: &str) -> String {
    serde_json::from_str::<serde_json::Value>(snippet)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| snippet.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_state_seeds_nine_tools() {
        let state = State::new();
        assert_eq!(state.tools().len(), 9);
    }

    #[test]
    fn a_new_state_is_not_running() {
        let state = State::new();
        assert!(!state.is_running());
        assert!(state.local_addr().is_none());
    }

    #[test]
    fn hover_does_not_produce_an_action() {
        let mut state = State::new();
        let now = Instant::now();
        assert!(update(&mut state, Message::Hover(SLOT_TOGGLE, true), now).is_none());
    }

    #[test]
    fn toggle_produces_an_action() {
        let mut state = State::new();
        let now = Instant::now();
        let out = update(&mut state, Message::Toggle(true), now);
        assert!(matches!(out, Some(Message::Toggle(true))));
    }

    #[test]
    fn params_summary_is_human_readable() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "x": { "type": "integer", "description": "where" },
                "y": { "type": "integer", "description": "where" }
            },
            "required": ["x"]
        });
        let s = summarize_params(&schema);
        assert!(s.contains("x*"), "{s}");
        assert!(s.contains("y"), "{s}");
        assert!(!s.contains("y*"), "{s}");
    }

    #[test]
    fn snippet_changes_when_running() {
        let off = snippet_for(None);
        assert!(off.contains("pravera"), "{off}");
        let addr: SocketAddr = "127.0.0.1:47615".parse().unwrap();
        let on = snippet_for(Some(addr));
        assert!(on.contains("47615"), "{on}");
        assert_ne!(off, on);
    }

    #[test]
    fn the_snippet_on_screen_is_the_one_on_the_clipboard() {
        let addr: SocketAddr = "127.0.0.1:47615".parse().unwrap();
        let compact = snippet_for(Some(addr));
        let shown = pretty(&compact);
        assert!(shown.lines().count() > 3, "{shown}");
        let a: serde_json::Value = serde_json::from_str(&compact).unwrap();
        let b: serde_json::Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn every_seeded_tool_lands_in_a_known_group() {
        let state = State::new();
        let groups = grouped(state.tools());
        let listed: usize = groups.iter().map(|(_, tools)| tools.len()).sum();
        assert_eq!(listed, 9);
        for (group, _) in &groups {
            assert!(GROUP_ORDER.contains(&group.as_str()), "{group} is not in the order");
        }
    }
}
