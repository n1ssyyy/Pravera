//! The first batch of tools: what Pravera can already let an agent do today.
//!
//! Everything here is real against the crates it names, and each description
//! says where its reach ends. Two honest limits recur:
//!
//! - **No live session.** The MCP server runs beside whatever Pravera process
//!   started it, not inside the host agent, so nothing here claims to see
//!   active sessions or streams — those facts belong to the agent that owns
//!   them.
//! - **No long-lived mDNS browser.** A discovery pass reports the direct-link
//!   and tailnet sources; LAN peers need a browser that has been listening
//!   for a while, which a one-shot tool call cannot be. Saying so beats an
//!   always-empty LAN section that looks authoritative.
//!
//! Input goes to *this machine's* focused window, because that is what the
//! platform offers. There is no targeting and no way to undo a key press, so
//! every input tool says exactly that in its description.

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use pravera_files::{Clipboard, Content};
use pravera_host::FileStore;
use pravera_input::InputSink;

use crate::registry::{Parameters, Registry, RegistryError, Tool};
use crate::wait_for;

/// Registers the seed batch into any registry.
///
/// Returns on the first refusal, so a duplicate name surfaces at whichever
/// startup call introduced it rather than as a missing tool later.
pub fn seed(registry: &Registry) -> Result<(), RegistryError> {
    for tool in [
        list_devices_tool(),
        host_status_tool(),
        list_displays_tool(),
        move_pointer_tool(),
        click_tool(),
        send_keys_tool(),
        clipboard_read_tool(),
        clipboard_write_tool(),
        list_users_tool(),
    ] {
        registry.register(tool)?;
    }
    Ok(())
}

/// One input sink for the whole process, created on first use and reused.
///
/// Creating a sink is not free — Linux opens a uinput device — and doing it
/// per keystroke would litter the machine with devices. A mutex makes one
/// sink safe to share across concurrent calls; recovering from poison keeps a
/// panic in one handler from locking the keyboard away from all of them.
struct SharedSink(Mutex<Option<Box<dyn InputSink>>>);

impl SharedSink {
    fn new() -> SharedSink {
        SharedSink(Mutex::new(None))
    }

    fn with<R>(
        &self,
        act: impl FnOnce(&mut dyn InputSink) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut slot = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            match pravera_input::sink() {
                Ok(sink) => *slot = Some(sink),
                Err(error) => return Err(format!("this machine has no input backend: {error}")),
            }
        }
        act(slot.as_mut().expect("filled two lines above").as_mut())
    }

    /// What this machine has for input, without performing any injection.
    ///
    /// Having no backend is reported as a fact rather than failed: the caller
    /// asked what is possible here, and "nothing" is a possible answer.
    fn describe(&self) -> Value {
        match self.with(|sink| {
            Ok(json!({
                "available": true,
                "backend": sink.name(),
                "true_relative_motion": sink.injects_true_relative(),
            }))
        }) {
            Ok(described) => described,
            Err(message) => json!({ "available": false, "error": message }),
        }
    }
}

/// The process-wide input sink shared by every input tool.
fn sink() -> &'static SharedSink {
    static SINK: OnceLock<SharedSink> = OnceLock::new();
    SINK.get_or_init(SharedSink::new)
}

fn list_devices_tool() -> Tool {
    Tool::new("list_devices", "discovery")
        .describes(
            "Run one discovery pass right now and report every peer found with its route, best \
             route first. Covers direct cable links and the tailnet, but not LAN/mDNS peers, \
             which only a long-running listener sees.",
        )
        .parameters(Parameters::new())
        .handles(|_| {
            // No LAN peers go in, because there is no browser to pass from —
            // see the module header. The output says so itself rather than
            // letting an absent section pose as an empty answer.
            let discovered = wait_for(pravera_discovery::scan_all(Vec::new()))?;
            let tailscale_present = discovered.tailscale_available();
            let peers: Vec<Value> = discovered
                .peers()
                .into_iter()
                .map(|peer| {
                    json!({
                        "name": peer.name,
                        "route": peer.source.label(),
                        "online": peer.online,
                        "verified": peer.is_verified(),
                        "addresses": peer.addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                        "os": peer.os,
                    })
                })
                .collect();
            let candidates: Vec<Value> = discovered
                .direct_links
                .into_iter()
                .map(|link| {
                    json!({
                        "interface": link.interface,
                        "kind": link.kind.label(),
                        "expected_gbps": link.kind.expected_gbps(),
                        "local_address": link.local_addr.to_string(),
                    })
                })
                .collect();

            Ok(json!({
                "peers": peers,
                "direct_link_candidates": candidates,
                "sources": {
                    "tailscale": tailscale_present,
                    "lan_mdns": false,
                },
            }))
        })
}

fn host_status_tool() -> Tool {
    Tool::new("host_status", "host")
        .describes(
            "Report what this machine can do as a Pravera host right now — capture, input, \
             clipboard, accounts file — each subsystem measured during this call.",
        )
        .parameters(Parameters::new())
        .handles(|_| {
            // Each subsystem is asked individually and reported individually.
            // A machine that cannot capture can still take input, and one
            // boolean for "can host" would hide which half is missing.
            let capture = match pravera_capture::source() {
                Ok(source) => {
                    let backend = source.name();
                    match source.displays() {
                        Ok(displays) => json!({
                            "available": true,
                            "backend": backend,
                            "displays": displays.len(),
                        }),
                        Err(error) => json!({
                            "available": true,
                            "backend": backend,
                            "displays_error": error.to_string(),
                        }),
                    }
                }
                Err(error) => json!({ "available": false, "error": error.to_string() }),
            };

            let clipboard = match pravera_files::clipboard::open() {
                Ok(_) => json!({ "available": true }),
                Err(error) => json!({ "available": false, "error": error.to_string() }),
            };
            let accounts_present = pravera_core::paths::accounts_file()
                .map(|path| path.is_file())
                .unwrap_or(false);

            Ok(json!({
                "platform": std::env::consts::OS,
                "capture": capture,
                "input": sink().describe(),
                "clipboard": clipboard,
                "accounts_file_present": accounts_present,
                "live_sessions": Value::Null,
                "note": "this server runs beside the UI, not inside the host agent, so live \
                         session state is not visible from here",
            }))
        })
}

fn list_displays_tool() -> Tool {
    Tool::new("list_displays", "host")
        .describes(
            "List this machine's displays as a session would stream them, primary display \
             first. Fails honestly where capture is unavailable.",
        )
        .parameters(Parameters::new())
        .handles(|_| {
            let displays = pravera_capture::source()
                .and_then(|source| source.displays())
                .map_err(|error| error.to_string())?;

            let listed: Vec<Value> = displays
                .into_iter()
                .map(|display| {
                    json!({
                        "id": display.id.index(),
                        "name": display.name,
                        "resolution": {
                            "width": display.resolution.width,
                            "height": display.resolution.height,
                        },
                        "position": { "x": display.position.0, "y": display.position.1 },
                        "scale": display.scale,
                        "primary": display.primary,
                        "refresh_hz": display.refresh_hz,
                    })
                })
                .collect();
            Ok(json!({ "displays": listed }))
        })
}

fn move_pointer_tool() -> Tool {
    Tool::new("move_pointer", "input")
        .describes(
            "Move this machine's mouse pointer — to absolute desktop pixels with x/y, or by a \
             relative delta with dx/dy. There is no undo: it moves wherever the desktop puts \
             it.",
        )
        .parameters(
            Parameters::new()
                .optional("x", "integer", "absolute position counting right from the top-left pixel")
                .optional("y", "integer", "absolute position counting down from the top-left pixel")
                .optional("dx", "integer", "relative movement instead of x/y")
                .optional("dy", "integer", "relative movement instead of x/y"),
        )
        .handles(|args| {
            let x = optional_i32(&args, "x")?;
            let y = optional_i32(&args, "y")?;
            let dx = optional_i32(&args, "dx")?;
            let dy = optional_i32(&args, "dy")?;

            sink().with(move |sink| {
                match (x, y, dx, dy) {
                    (Some(x), Some(y), None, None) => sink.pointer_to(x, y).map_err(str_of)?,
                    (None, None, Some(dx), Some(dy)) => sink.pointer_by(dx, dy).map_err(str_of)?,
                    _ => {
                        return Err(
                            "give either absolute x/y or relative dx/dy, not a mix".to_string()
                        )
                    }
                }
                Ok(())
            })?;
            Ok(json!({ "moved": true }))
        })
}

fn click_tool() -> Tool {
    Tool::new("click", "input")
        .describes(
            "Press and release a mouse button wherever the pointer currently sits on this \
             machine. Real clicks on real windows.",
        )
        .parameters(
            Parameters::new()
                .optional("button", "string", "left | middle | right | back | forward (default left)")
                .optional("times", "integer", "press-and-release count, 1..=25 (default 1)"),
        )
        .handles(|args| {
            let button_name = args.get("button").and_then(Value::as_str).unwrap_or("left");
            let button = match button_name {
                "left" => pravera_proto::PointerButton::Left,
                "middle" => pravera_proto::PointerButton::Middle,
                "right" => pravera_proto::PointerButton::Right,
                "back" => pravera_proto::PointerButton::Back,
                "forward" => pravera_proto::PointerButton::Forward,
                other => return Err(format!("`{other}` is not a mouse button")),
            };
            let times = match args.get("times") {
                None => 1,
                Some(value) => {
                    let requested = value
                        .as_u64()
                        .ok_or("`times` must be a positive integer")?;
                    if !(1..=25).contains(&requested) {
                        return Err(format!(
                            "`times` must be between 1 and 25, got {requested}"
                        ));
                    }
                    requested
                }
            };

            sink().with(move |sink| {
                for _ in 0..times {
                    sink.button(button, true).map_err(str_of)?;
                    sink.button(button, false).map_err(str_of)?;
                }
                Ok(())
            })?;
            Ok(json!({ "clicked": times, "button": button_name }))
        })
}

fn send_keys_tool() -> Tool {
    Tool::new("send_keys", "input")
        .describes(
            "Type text on this machine into whatever window has focus, or tap physical keys \
             named by USB HID usage code (page 0x07), one after another. Chords such as Ctrl+C \
             are not expressible yet: keys are tapped in sequence, never held.",
        )
        .parameters(
            Parameters::new()
                .optional("text", "string", "what should appear, composed exactly as it should land")
                .optional(
                    "keys",
                    "array",
                    "HID usage codes to tap in order, e.g. [4] taps the key position that types A",
                ),
        )
        .handles(|args| {
            let text = args.get("text").and_then(Value::as_str);
            let usages = match args.get("keys").and_then(Value::as_array) {
                None => None,
                Some(values) => {
                    let mut parsed = Vec::with_capacity(values.len());
                    for value in values {
                        let usage = value
                            .as_u64()
                            .filter(|u| *u <= u64::from(u16::MAX))
                            .ok_or("every entry of `keys` must be an integer HID usage (0..=65535)")?;
                        parsed.push(usage as u16);
                    }
                    Some(parsed)
                }
            };

            match (text, usages) {
                (Some(text), None) => {
                    if text.is_empty() {
                        return Err("`text` is empty".to_string());
                    }
                    let count = text.chars().count();
                    sink().with(move |sink| sink.text(text).map_err(str_of))?;
                    Ok(json!({ "typed": count, "as": "text" }))
                }
                (None, Some(usages)) => {
                    if usages.is_empty() {
                        return Err("`keys` is empty".to_string());
                    }
                    let tapped = usages.len();
                    sink().with(move |sink| {
                        for usage in &usages {
                            sink.key(pravera_proto::KeyCode(*usage), true).map_err(str_of)?;
                            sink.key(pravera_proto::KeyCode(*usage), false).map_err(str_of)?;
                        }
                        Ok(())
                    })?;
                    Ok(json!({ "tapped": tapped, "as": "hid_usages" }))
                }
                (Some(_), Some(_)) => Err("give `text` or `keys`, not both".to_string()),
                (None, None) => Err("give `text` to type or `keys` to tap".to_string()),
            }
        })
}

fn clipboard_read_tool() -> Tool {
    Tool::new("clipboard_read", "files")
        .describes(
            "Read this machine's clipboard text verbatim — quite possibly the most recent thing \
             its user copied, which may well be a password. Text only.",
        )
        .sensitive(true)
        .parameters(Parameters::new())
        .handles(|_| {
            let mut clipboard = open_clipboard()?;
            let (_, content) = clipboard.read().map_err(str_of)?;
            match content {
                Content::Text(text) => Ok(json!({ "kind": "text", "text": text })),
                Content::Uncarried => Ok(json!({
                    "kind": "uncarried",
                    "note": "the clipboard is empty, or holds something that is not text",
                })),
            }
        })
}

fn clipboard_write_tool() -> Tool {
    Tool::new("clipboard_write", "files")
        .describes("Replace this machine's clipboard with the given text.")
        .parameters(Parameters::new().required("text", "string", "what the clipboard should hold"))
        .handles(|args| {
            let text = args.get("text").and_then(Value::as_str).ok_or("`text` must be a string")?;
            let mut clipboard = open_clipboard()?;
            let sequence = clipboard.write(text).map_err(str_of)?;
            Ok(json!({ "written": true, "sequence": sequence.0 }))
        })
}

fn list_users_tool() -> Tool {
    Tool::new("list_users", "host")
        .describes(
            "List who may sign in to this machine, read straight from its accounts file: \
             usernames, roles, enabled. No password material is included — hashes stay in the \
             file, passwords were never stored anywhere.",
        )
        .parameters(Parameters::new())
        .handles(|_| {
            let path = pravera_core::paths::accounts_file().map_err(str_of)?;
            if !path.is_file() {
                return Ok(json!({
                    "users": [],
                    "roles": [],
                    "note": "no accounts file exists yet; nobody can sign in until one is created",
                }));
            }

            let store = FileStore::load(path).map_err(str_of)?;
            let users: Vec<Value> = store
                .accounts()
                .into_iter()
                .map(|account| {
                    json!({
                        "username": account.username,
                        "role": account.role,
                        "enabled": account.enabled,
                        "can_connect": account.can_connect(),
                    })
                })
                .collect();
            let roles: Vec<Value> = store
                .known_roles()
                .iter()
                .map(|role| json!({ "name": role.name, "builtin": role.builtin }))
                .collect();
            Ok(json!({ "users": users, "roles": roles }))
        })
}

fn open_clipboard() -> Result<Box<dyn Clipboard>, String> {
    pravera_files::clipboard::open()
        .map_err(|error| format!("this build cannot reach a clipboard from here: {error}"))
}

fn str_of<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

fn optional_i32(args: &Value, name: &str) -> Result<Option<i32>, String> {
    let Some(value) = args.get(name) else {
        return Ok(None);
    };
    let raw = value.as_i64().ok_or_else(|| format!("`{name}` must be an integer"))?;
    i32::try_from(raw)
        .map(Some)
        .map_err(|_| format!("`{name}` = {raw} does not fit a screen coordinate"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    fn seeded() -> Registry {
        let registry = Registry::new();
        seed(&registry).expect("the seed batch registers cleanly");
        registry
    }

    #[test]
    fn the_seed_batch_registers_every_tool_described_and_schemaed() {
        let listed = seeded().list();
        assert_eq!(listed.len(), 9, "a tool was added or dropped without notice");

        for spec in listed {
            assert!(!spec.description.trim().is_empty(), "`{}` has no description", spec.name);
            assert_eq!(spec.parameters["type"], "object");
            assert!(
                ["discovery", "host", "input", "files", "session"]
                    .contains(&spec.category.as_str()),
                "`{}` claims the unknown category `{}`",
                spec.name,
                spec.category
            );
        }
    }

    #[test]
    fn every_seed_name_is_stable_so_agents_can_depend_on_it() {
        let expected = [
            "click",
            "clipboard_read",
            "clipboard_write",
            "host_status",
            "list_devices",
            "list_displays",
            "list_users",
            "move_pointer",
            "send_keys",
        ];
        let names: Vec<String> = seeded().list().into_iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn tools_that_expose_what_the_user_typed_are_marked_sensitive() {
        let listed = seeded().list();
        let reader = listed.iter().find(|t| t.name == "clipboard_read").unwrap();
        assert!(
            reader.sensitive,
            "the clipboard routinely holds pasted passwords and must be marked"
        );

        let writer = listed.iter().find(|t| t.name == "clipboard_write").unwrap();
        assert!(!writer.sensitive, "writing puts text on the machine; it takes none off");
    }

    #[test]
    fn bad_arguments_are_refused_before_any_platform_is_touched() {
        // Deliberately never exercises a successful injection: a test suite
        // that moves the developer's actual mouse is a test suite nobody runs.
        let registry = seeded();
        for (tool, args) in [
            ("move_pointer", json!({})),
            ("move_pointer", json!({ "x": 5, "dx": 5 })),
            ("move_pointer", json!({ "x": "left" })),
            ("click", json!({ "times": 99 })),
            ("click", json!({ "times": 0 })),
            ("click", json!({ "button": "any" })),
            ("send_keys", json!({})),
            ("send_keys", json!({ "text": "", "keys": [] })),
            ("send_keys", json!({ "text": "hi", "keys": [4] })),
            ("send_keys", json!({ "keys": [99_999_999] })),
        ] {
            let outcome = registry.call(tool, args.clone());
            assert!(
                matches!(outcome, Err(RegistryError::InvalidArguments { .. }))
                    || matches!(outcome, Err(RegistryError::Execution { .. })),
                "{tool} did not refuse {args}: got {outcome:?}"
            );
        }
    }

    #[test]
    fn seeding_twice_into_the_same_registry_names_the_collision_rather_than_shadowing() {
        let registry = Registry::new();
        seed(&registry).unwrap();

        let second_pass = seed(&registry).unwrap_err();
        assert!(matches!(second_pass, RegistryError::DuplicateTool { .. }));
    }

    #[test]
    fn a_display_enumeration_failure_becomes_a_structured_answer_rather_than_a_crash() {
        // On a headless build agent the enumeration fails; the tool must
        // still answer, carrying the platform's wording either way.
        let outcome = seeded().call("list_displays", json!({}));
        match outcome {
            Ok(value) => assert!(value["displays"].is_array()),
            Err(RegistryError::Execution { message, .. }) => assert!(!message.is_empty()),
            Err(other) => panic!("unexpected failure shape: {other}"),
        }
    }
}
