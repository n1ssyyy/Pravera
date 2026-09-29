//! The catalogue of tools the agent may call, and the only thing the MCP
//! server knows about features.
//!
//! A registry is deliberately dumb: it stores specs, validates what a spec
//! promised, and dispatches. It cannot enumerate Pravera's capabilities,
//! because the crates that own those capabilities register themselves — that
//! is the whole point. The [`Registry::global`] instance exists for exactly
//! this: a feature module calls `Registry::global().register(...)`, and every
//! server instance the UI later starts serves whatever accumulated.
//!
//! ## The input comes from a machine
//!
//! Everything past the constructor is reachable with arbitrary bytes, so no
//! path here may panic: unknown names, non-object arguments, missing required
//! fields and even a panicking handler come back as a structured
//! [`RegistryError`]. A tool that takes the server down with it would take
//! every other tool down too.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{json, Value};

/// The function that does one tool's work.
///
/// Deliberately synchronous and plain, so a feature can register a closure
/// without knowing anything about executors. Tools that must await something
/// bridge through `crate::wait_for`.
pub type Handler = Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync>;

/// Everything the protocol layer needs to know about one tool.
pub struct ToolSpec {
    /// snake_case verb phrase, e.g. `list_devices`. This is the name an agent
    /// sends in `tools/call`.
    pub name: String,
    /// One sentence saying what the tool does — and, where relevant, what it
    /// cannot reach in this build. Agents read this when choosing what to
    /// call, so an overpromise here becomes a wrong action over there.
    pub description: String,
    /// Coarse grouping ("discovery", "host", "input", "files", "session"),
    /// for a future settings screen that presents tools by area.
    pub category: String,
    /// A JSON-schema-ish description of the arguments: an object with
    /// `properties` and, where any exist, `required`. Served verbatim as the
    /// MCP `inputSchema`.
    pub parameters: Value,
    /// Whether this tool reads or takes credentials, or anything a person
    /// might paste as one. Never used to gate a call — permission enforcement
    /// lives host-side — but the UI marks such tools rather than letting
    /// somebody discover the difference by having their clipboard read.
    pub sensitive: bool,
    pub(crate) handler: Handler,
}

/// Builds a [`ToolSpec`] one field at a time.
///
/// Registration validates completeness, so a half-specified tool fails at the
/// call site that wrote it rather than at `tools/list`, where nobody is left
/// holding the error.
pub struct Tool {
    name: String,
    description: Option<String>,
    category: String,
    parameters: Value,
    sensitive: bool,
    handler: Option<Handler>,
}

impl Tool {
    pub fn new(name: &str, category: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: None,
            category: category.to_string(),
            parameters: Parameters::new().build(),
            sensitive: false,
            handler: None,
        }
    }

    pub fn describes(mut self, description: impl Into<String>) -> Tool {
        self.description = Some(description.into());
        self
    }

    pub fn parameters(mut self, parameters: impl Into<Value>) -> Tool {
        self.parameters = parameters.into();
        self
    }

    pub fn sensitive(mut self, sensitive: bool) -> Tool {
        self.sensitive = sensitive;
        self
    }

    pub fn handles(
        mut self,
        handler: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Tool {
        self.handler = Some(Arc::new(handler));
        self
    }

    fn build(self) -> Result<ToolSpec, RegistryError> {
        let incomplete = |reason: String| RegistryError::InvalidSpec {
            name: self.name.clone(),
            reason,
        };

        if !is_snake_case(&self.name) {
            return Err(incomplete(format!(
                "`{}` is not snake_case, though `tools/call` addresses tools by exactly this \
                 string",
                self.name
            )));
        }
        if self.category.is_empty() {
            return Err(incomplete("the category is empty".to_string()));
        }
        let Some(description) = self.description else {
            return Err(incomplete("no description was given".to_string()));
        };
        if description.trim().is_empty() {
            return Err(incomplete("the description is empty".to_string()));
        }
        let Some(handler) = self.handler else {
            return Err(incomplete("no handler was given".to_string()));
        };

        Ok(ToolSpec {
            name: self.name,
            description,
            category: self.category,
            parameters: self.parameters,
            sensitive: self.sensitive,
            handler,
        })
    }
}

fn is_snake_case(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && !name.starts_with('_')
        && !name.ends_with('_')
        && !name.contains("__")
}

/// Describes a tool's arguments without writing JSON Schema by hand.
///
/// Covers what agents actually act on: property names, types, per-property
/// sentences, required-ness. Full JSON Schema vocabulary — nested objects,
/// enums, `oneOf` — is out of scope; a tool needing more than this probably
/// needs fewer arguments instead.
#[derive(Default)]
pub struct Parameters {
    properties: serde_json::Map<String, Value>,
    required: Vec<String>,
}

impl Parameters {
    pub fn new() -> Parameters {
        Parameters::default()
    }

    pub fn required(mut self, name: &str, kind: &str, description: &str) -> Parameters {
        self.properties.insert(name.to_string(), property(kind, description));
        self.required.push(name.to_string());
        self
    }

    pub fn optional(mut self, name: &str, kind: &str, description: &str) -> Parameters {
        self.properties.insert(name.to_string(), property(kind, description));
        self
    }

    pub fn build(self) -> Value {
        let mut schema = json!({
            "type": "object",
            "properties": Value::Object(self.properties),
        });
        // Absence already means "nothing is required"; saying so with an empty
        // list would be noise on every listing.
        if !self.required.is_empty() {
            schema["required"] = json!(self.required);
        }
        schema
    }
}

fn property(kind: &str, description: &str) -> Value {
    json!({ "type": kind, "description": description })
}

/// A finished schema can go wherever a raw JSON value would.
impl From<Parameters> for Value {
    fn from(parameters: Parameters) -> Value {
        parameters.build()
    }
}

/// Every way interacting with the catalogue can fail, each phrased so whoever
/// reads it — usually an agent — can decide what to do next.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("no tool named `{name}` is registered")]
    UnknownTool { name: String },
    #[error("`{tool}` was called with bad arguments: {reason}")]
    InvalidArguments { tool: String, reason: String },
    #[error("`{tool}` failed: {message}")]
    Execution { tool: String, message: String },
    #[error("`{name}` is already registered; a second registration would silently hide one of two different tools")]
    DuplicateTool { name: String },
    #[error("the tool `{name}` cannot be registered: {reason}")]
    InvalidSpec { name: String, reason: String },
}

/// The collection of registered tools.
///
/// Cheap to clone; every holder sees the same set. Registration is rare
/// (feature startup) while listing happens on every `tools/list`, so readers
/// share one read lock and never block each other.
#[derive(Clone, Default)]
pub struct Registry {
    tools: Arc<RwLock<BTreeMap<String, Arc<ToolSpec>>>>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry::default()
    }

    /// The process-wide registry every feature registers into.
    ///
    /// A global is the honest shape here: the tools are process-level facts
    /// about what this binary can do, and a UI that starts several server
    /// instances is still describing one machine.
    pub fn global() -> &'static Registry {
        static GLOBAL: OnceLock<Registry> = OnceLock::new();
        GLOBAL.get_or_init(Registry::new)
    }

    /// Adds a tool, refusing a duplicate name outright.
    ///
    /// Two features registering the same name means an agent calling
    /// `send_keys` cannot know which of two different handlers it reached.
    /// Failing at registration turns that silent misroute into an obvious
    /// startup bug.
    pub fn register(&self, tool: Tool) -> Result<(), RegistryError> {
        let spec = Arc::new(tool.build()?);
        {
            let mut tools = self.write();
            if tools.contains_key(&spec.name) {
                return Err(RegistryError::DuplicateTool { name: spec.name.clone() });
            }
            tools.insert(spec.name.clone(), spec);
        }
        Ok(())
    }

    /// A snapshot of every registered tool, sorted by name so listings stay
    /// stable from one call to the next.
    pub fn list(&self) -> Vec<Arc<ToolSpec>> {
        self.read().values().cloned().collect()
    }

    /// Dispatches a call, and never panics even if the handler does.
    ///
    /// Argument checking is intentionally shallow: arguments arrive as an
    /// object, and every field the spec marked `required` is present. Deeper
    /// validation belongs inside the handler, which knows what its own values
    /// mean — the registry would need a full JSON Schema engine to be stricter
    /// than the code actually consuming them.
    pub fn call(&self, name: &str, args: Value) -> Result<Value, RegistryError> {
        let spec = self
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| RegistryError::UnknownTool { name: name.to_string() })?;

        let args = match args {
            Value::Null => json!({}),
            args @ Value::Object(_) => args,
            other => {
                return Err(RegistryError::InvalidArguments {
                    tool: spec.name.clone(),
                    reason: format!("arguments must be an object, got {}", type_of(&other)),
                })
            }
        };

        if let Some(missing) = first_missing_required(&spec.parameters, &args) {
            return Err(RegistryError::InvalidArguments {
                tool: spec.name.clone(),
                reason: format!("the required argument `{missing}` is missing"),
            });
        }

        // A panic inside a feature's closure costs that one call, not the
        // server every other tool lives on.
        match catch_unwind(AssertUnwindSafe(|| (spec.handler)(args))) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(RegistryError::Execution {
                tool: spec.name.clone(),
                message,
            }),
            Err(panic) => Err(RegistryError::Execution {
                tool: spec.name.clone(),
                message: panic_message(panic),
            }),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Arc<ToolSpec>>> {
        // A panic while holding the lock poisons it, but the map itself is
        // intact — registration either happened or it did not. Recovering
        // beats locking every tool out forever over a failure that already
        // finished happening.
        self.tools.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, Arc<ToolSpec>>> {
        self.tools.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn first_missing_required(schema: &Value, args: &Value) -> Option<String> {
    schema.get("required")?.as_array()?.iter().find_map(|name| {
        let name = name.as_str()?;
        args.get(name).is_none().then(|| name.to_string())
    })
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast_ref::<&str>() {
        Some(text) => format!("the handler panicked: {text}"),
        None => "the handler panicked".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Tool {
        Tool::new(name, "test")
            .describes("a tool with nothing to do")
            .handles(|_| Ok(json!({})))
    }

    #[test]
    fn a_registered_tool_appears_in_the_listing_sorted_by_name() {
        let registry = Registry::new();
        registry.register(named("second_tool")).unwrap();
        registry.register(named("first_tool")).unwrap();

        let names: Vec<_> = registry.list().into_iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, ["first_tool", "second_tool"]);
    }

    #[test]
    fn a_tool_registered_after_a_clone_was_taken_is_seen_through_the_clone() {
        // The dynamics this crate exists for: the registry is shared, and a
        // late registration must reach holders of older clones.
        let registry = Registry::new();
        let held = registry.clone();
        registry.register(named("early_tool")).unwrap();

        registry.register(named("late_tool")).unwrap();

        let names: Vec<_> = held.list().into_iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"late_tool".to_string()));
    }

    #[test]
    fn calling_an_unknown_tool_is_a_structured_error_rather_than_a_panic() {
        let registry = Registry::new();
        assert_eq!(
            registry.call("no_such_tool", json!({})),
            Err(RegistryError::UnknownTool { name: "no_such_tool".into() })
        );
    }

    #[test]
    fn arguments_that_are_not_an_object_are_refused_before_any_handler_runs() {
        let registry = Registry::new();
        registry.register(named("fussy")).unwrap();

        for bad in [json!("text"), json!([1, 2]), json!(7)] {
            assert!(
                matches!(
                    registry.call("fussy", bad.clone()),
                    Err(RegistryError::InvalidArguments { .. })
                ),
                "{bad} was accepted"
            );
        }
    }

    #[test]
    fn a_missing_required_argument_is_named_in_the_error() {
        let registry = Registry::new();
        registry.register(
            Tool::new("needs_thing", "test")
                .describes("demands an argument")
                .parameters(Parameters::new().required("thing", "string", "what is needed"))
                .handles(|args| Ok(json!({ "got": args }))),
        ).unwrap();

        assert_eq!(
            registry.call("needs_thing", json!({})),
            Err(RegistryError::InvalidArguments {
                tool: "needs_thing".into(),
                reason: "the required argument `thing` is missing".into(),
            })
        );
    }

    #[test]
    fn a_panicking_handler_costs_one_call_and_nothing_else() {
        let registry = Registry::new();
        registry
            .register(
                Tool::new("explodes", "test")
                    .describes("panics on purpose")
                    .handles(|_| panic!("deliberate")),
            )
            .unwrap();
        registry.register(named("survivor")).unwrap();

        assert!(matches!(
            registry.call("explodes", json!({})),
            Err(RegistryError::Execution { .. })
        ));
        assert!(
            registry.call("survivor", json!({})).is_ok(),
            "one panicking tool must not take the registry down"
        );
    }

    #[test]
    fn a_handler_error_travels_to_the_caller_verbatim() {
        let registry = Registry::new();
        registry
            .register(
                Tool::new("honest_failure", "test")
                    .describes("always refuses")
                    .handles(|_| Err("no displays attached".to_string())),
            )
            .unwrap();

        assert_eq!(
            registry.call("honest_failure", json!({})),
            Err(RegistryError::Execution {
                tool: "honest_failure".into(),
                message: "no displays attached".into(),
            })
        );
    }

    #[test]
    fn the_sensitive_flag_round_trips_through_the_registry() {
        let registry = Registry::new();
        registry.register(named("plain_tool")).unwrap();
        registry.register(named("credential_taker").sensitive(true)).unwrap();

        let listed = registry.list();
        let plain = listed.iter().find(|t| t.name == "plain_tool").unwrap();
        let taker = listed.iter().find(|t| t.name == "credential_taker").unwrap();
        assert!(!plain.sensitive);
        assert!(taker.sensitive);
    }

    #[test]
    fn a_duplicate_name_is_refused_rather_than_silently_replaced() {
        let registry = Registry::new();
        registry.register(named("same_name")).unwrap();

        assert!(matches!(
            registry.register(named("same_name")),
            Err(RegistryError::DuplicateTool { .. })
        ));
        assert_eq!(registry.list().len(), 1, "the second registration must not win");
    }

    #[test]
    fn an_incomplete_spec_is_refused_at_registration_where_its_author_can_see_it() {
        let registry = Registry::new();

        let undescribed = registry.register(Tool::new("no_description", "test"));
        assert!(
            matches!(undescribed, Err(RegistryError::InvalidSpec { .. })),
            "a description-less tool was accepted"
        );

        let handlerless =
            registry.register(Tool::new("no_handler", "test").describes("says much, does nothing"));
        assert!(matches!(handlerless, Err(RegistryError::InvalidSpec { .. })));
    }

    #[test]
    fn names_must_be_addressable_as_snake_case() {
        for bad in ["ListDevices", "1st_tool", "_private", "trailing_", "double__underscore"] {
            let registry = Registry::new();
            assert!(
                matches!(registry.register(named(bad)), Err(RegistryError::InvalidSpec { .. })),
                "`{bad}` was accepted"
            );
        }
    }

    #[test]
    fn absent_arguments_become_an_empty_object_for_handlers_that_take_none() {
        let registry = Registry::new();
        registry
            .register(
                Tool::new("takes_nothing", "test")
                    .describes("ignores arguments entirely")
                    .handles(|args| Ok(json!({ "saw": args }))),
            )
            .unwrap();

        let seen = registry.call("takes_nothing", Value::Null).unwrap();
        assert_eq!(seen, json!({ "saw": {} }));
    }

    #[test]
    fn the_schema_builder_lists_properties_and_marks_what_is_required() {
        let schema = Parameters::new()
            .required("text", "string", "what to type")
            .optional("times", "integer", "how many times")
            .build();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["text"]["type"], "string");
        assert_eq!(schema["properties"]["times"]["description"], "how many times");
        assert_eq!(schema["required"], json!(["text"]));
    }

    #[test]
    fn a_schema_with_nothing_required_omits_the_list_rather_than_sending_noise() {
        let schema = Parameters::new()
            .optional("x", "integer", "where across")
            .build();

        assert!(schema.get("required").is_none());
    }
}
