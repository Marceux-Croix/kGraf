//! The tool set, and the one entry point that runs a tool.
//!
//! **One snapshot per vendor and model.** Tool definitions sit in the shared prefix
//! (decision 0043), which must be byte-identical for every worker on the same
//! vendor and model. So every such worker is given the **same registry snapshot**
//! and the full prefix definitions. A skill's `allowed-tools` never removes a
//! definition: that would give the restricted worker a different prefix and cost a
//! cache write. It is enforced at approval instead (`CallPolicy`), where a refused
//! call is denied and recorded.
//!
//! **Fixed order.** Keyed by wire name in a `BTreeMap`, so definitions come out in
//! one order regardless of registration order.
//!
//! **Deferred partition.** A tool registered [`Exposure::Deferred`] is
//! kept out of the prefix. A worker finds it through tool search; the definitions it
//! discovers join that worker's **private remainder**, in discovery order, never the
//! prefix. Calling a deferred tool the worker has not discovered is refused.
//!
//! **Definitions are captured at registration.** A changed MCP tool list is applied
//! by re-registering the server between dispatches (never mid-run), and
//! [`ToolRegistry::prefix_definitions_digest`] changes, which the runtime records as
//! a prefix invalidation. That digest detects change; the prefix digest proper is
//! computed over the adapter's wire bytes (`kgraf-runtime`'s `SharedPrefix`).

use crate::approval::ApprovedCall;
use crate::error::{RegistryError, ToolError};
use crate::name::QualifiedName;
use crate::output::ToolOutput;
use crate::{CallPermit, Tool, ToolContext};
use kgraf_provider::message::ToolDefinition;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Where a tool's definition goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exposure {
    /// In the shared prefix, for every worker.
    Prefix,
    /// Withheld; found through tool search into one worker's private remainder.
    Deferred,
}

/// One registered tool.
#[derive(Clone)]
pub struct RegisteredTool {
    name: QualifiedName,
    definition: ToolDefinition,
    exposure: Exposure,
    tool: Arc<dyn Tool>,
}

impl RegisteredTool {
    /// The tool's qualified name.
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The definition as sent to the model, under the wire name.
    pub fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Where the definition goes.
    pub fn exposure(&self) -> Exposure {
        self.exposure
    }

    /// The tool, for reading its declarations. Running it goes through
    /// [`ToolRegistry::execute`].
    pub fn tool(&self) -> &Arc<dyn Tool> {
        &self.tool
    }
}

impl fmt::Debug for RegisteredTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredTool")
            .field("name", &self.name)
            .field("exposure", &self.exposure)
            .finish_non_exhaustive()
    }
}

/// The registered tools, by wire name.
#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a built-in tool under its own name.
    pub fn register_builtin(
        &mut self,
        tool: Arc<dyn Tool>,
        exposure: Exposure,
    ) -> Result<QualifiedName, RegistryError> {
        let name = QualifiedName::builtin(tool.name())?;
        // Built-in schemas are this project's own, so an untyped node is a
        // defect here, refused before any request carries it. An MCP server's
        // schema is its author's and is registered as given; a vendor that
        // refuses it fails the call with its reason.
        let untyped = crate::untyped_schema_nodes(&tool.definition().input_schema);
        if !untyped.is_empty() {
            return Err(RegistryError::UntypedSchema {
                tool: tool.name().to_owned(),
                paths: untyped,
            });
        }
        self.insert(name, tool, exposure)
    }

    /// Register an MCP server's tool under `mcp__<server>__<tool>`.
    pub fn register_mcp(
        &mut self,
        server: &str,
        tool: Arc<dyn Tool>,
        exposure: Exposure,
    ) -> Result<QualifiedName, RegistryError> {
        let name = QualifiedName::mcp(server, tool.name())?;
        self.insert(name, tool, exposure)
    }

    fn insert(
        &mut self,
        name: QualifiedName,
        tool: Arc<dyn Tool>,
        exposure: Exposure,
    ) -> Result<QualifiedName, RegistryError> {
        let mut definition = tool.definition();
        if definition.name != tool.name() {
            return Err(RegistryError::DefinitionMismatch {
                tool: tool.name().to_owned(),
                definition: definition.name,
            });
        }
        let wire = name.wire();
        if self.tools.contains_key(&wire) {
            return Err(RegistryError::Duplicate { name: wire });
        }
        definition.name = wire.clone();
        self.tools.insert(
            wire,
            RegisteredTool {
                name: name.clone(),
                definition,
                exposure,
                tool,
            },
        );
        Ok(name)
    }

    /// Remove every tool of one MCP server, returning how many were removed.
    pub fn remove_server(&mut self, server: &str) -> usize {
        let before = self.tools.len();
        self.tools.retain(|_, t| t.name.server() != Some(server));
        before - self.tools.len()
    }

    /// A tool by wire name, deferred or not.
    pub fn get(&self, wire: &str) -> Option<&RegisteredTool> {
        self.tools.get(wire)
    }

    /// The definitions in the shared prefix, in wire-name order. Every worker on
    /// one vendor and model gets exactly these, whatever its skills allow.
    pub fn prefix_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .filter(|t| t.exposure == Exposure::Prefix)
            .map(|t| t.definition.clone())
            .collect()
    }

    /// blake3 over the prefix definitions' serialized bytes, in order. A change
    /// detector for the tool list, not the prefix digest.
    pub fn prefix_definitions_digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for def in self.prefix_definitions() {
            // serde_json::to_vec cannot fail for this type (string keys, no maps
            // with non-string keys); an empty encoding would still change the
            // digest from a non-empty one, so nothing is hidden.
            let bytes = serde_json::to_vec(&def).unwrap_or_default();
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        hasher.finalize().to_hex().to_string()
    }

    /// Deferred tools whose wire name or description contains `query`, ignoring
    /// case, in wire-name order.
    pub fn search_deferred(&self, query: &str) -> Vec<&RegisteredTool> {
        let query = query.to_lowercase();
        self.tools
            .values()
            .filter(|t| t.exposure == Exposure::Deferred)
            .filter(|t| {
                t.definition.name.to_lowercase().contains(&query)
                    || t.definition.description.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// Qualified names, in wire-name order.
    pub fn qualified_names(&self) -> impl Iterator<Item = &QualifiedName> {
        self.tools.values().map(RegisteredTool::name)
    }

    /// Wire names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Number of tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Run an approved call. The only place a [`CallPermit`] is made.
    pub async fn execute(
        &self,
        approved: ApprovedCall,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let registered = self
            .get(approved.tool())
            .ok_or_else(|| ToolError::UnknownTool {
                name: approved.tool().to_owned(),
            })?;
        let permit = CallPermit::new();
        registered
            .tool
            .call(approved.into_input(), ctx, &permit)
            .await
    }
}

/// The deferred tools one worker has discovered, in discovery order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveredTools {
    order: Vec<String>,
}

impl DiscoveredTools {
    /// Nothing discovered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Search the deferred partition and record new matches. Returns the newly
    /// discovered definitions, to append to the worker's private remainder.
    pub fn discover(&mut self, registry: &ToolRegistry, query: &str) -> Vec<ToolDefinition> {
        let mut added = Vec::new();
        for tool in registry.search_deferred(query) {
            let wire = tool.definition.name.clone();
            if !self.order.contains(&wire) {
                self.order.push(wire);
                added.push(tool.definition.clone());
            }
        }
        added
    }

    /// Whether a wire name has been discovered.
    pub fn contains(&self, wire: &str) -> bool {
        self.order.iter().any(|w| w == wire)
    }

    /// The discovered definitions, in discovery order, for the private remainder.
    pub fn definitions(&self, registry: &ToolRegistry) -> Vec<ToolDefinition> {
        self.order
            .iter()
            .filter_map(|w| registry.get(w).map(|t| t.definition.clone()))
            .collect()
    }
}

/// What one worker can call: the shared registry plus its own discoveries.
#[derive(Clone, Copy, Debug)]
pub struct ToolView<'a> {
    pub registry: &'a ToolRegistry,
    pub discovered: &'a DiscoveredTools,
}

impl<'a> ToolView<'a> {
    /// A tool the worker may call by this wire name.
    pub fn resolve(&self, wire: &str) -> Result<&'a RegisteredTool, ToolError> {
        let tool = self
            .registry
            .get(wire)
            .ok_or_else(|| ToolError::UnknownTool {
                name: wire.to_owned(),
            })?;
        if tool.exposure == Exposure::Deferred && !self.discovered.contains(wire) {
            return Err(ToolError::NotDiscovered {
                name: wire.to_owned(),
            });
        }
        Ok(tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Access;
    use serde_json::{Value, json};

    struct Named {
        name: &'static str,
        definition_name: &'static str,
        description: &'static str,
    }

    fn named(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(Named {
            name,
            definition_name: name,
            description: "d",
        })
    }

    #[async_trait::async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.name
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.definition_name.into(),
                description: self.description.into(),
                input_schema: json!({"type": "object"}),
            }
        }
        fn access(&self, _input: &Value) -> Result<Access, ToolError> {
            Ok(Access::none())
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(self.name))
        }
    }

    /// A tool that does not override `access`.
    struct Undeclared;

    #[async_trait::async_trait]
    impl Tool for Undeclared {
        fn name(&self) -> &str {
            "undeclared"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "undeclared".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(""))
        }
    }

    const P: Exposure = Exposure::Prefix;
    const D: Exposure = Exposure::Deferred;

    #[test]
    fn a_tool_that_does_not_declare_access_is_exclusive() {
        assert!(
            Undeclared
                .access(&json!({}))
                .expect("access")
                .is_exclusive()
        );
    }

    #[test]
    fn a_duplicate_name_is_refused_not_replaced() {
        let mut reg = ToolRegistry::new();
        reg.register_mcp("github", named("search"), P)
            .expect("first");
        let err = reg
            .register_mcp("github", named("search"), D)
            .expect_err("second must be refused");
        assert_eq!(
            err,
            RegistryError::Duplicate {
                name: "mcp__github__search".into()
            }
        );
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn two_servers_with_the_same_tool_name_do_not_collide() {
        let mut reg = ToolRegistry::new();
        reg.register_mcp("github", named("search"), P)
            .expect("github");
        reg.register_mcp("gitlab", named("search"), P)
            .expect("gitlab");
        reg.register_builtin(named("search"), P).expect("builtin");
        let names: Vec<&str> = reg.names().collect();
        assert_eq!(
            names,
            vec!["mcp__github__search", "mcp__gitlab__search", "search"]
        );
    }

    #[test]
    fn prefix_definitions_carry_the_wire_name_in_fixed_order() {
        let mut a = ToolRegistry::new();
        a.register_builtin(named("zeta"), P).expect("z");
        a.register_mcp("fs", named("read"), P).expect("r");
        a.register_builtin(named("alpha"), P).expect("a");
        let mut b = ToolRegistry::new();
        b.register_builtin(named("alpha"), P).expect("a");
        b.register_builtin(named("zeta"), P).expect("z");
        b.register_mcp("fs", named("read"), P).expect("r");

        let names: Vec<String> = a.prefix_definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["alpha", "mcp__fs__read", "zeta"]);
        assert_eq!(a.prefix_definitions(), b.prefix_definitions());
        assert_eq!(a.prefix_definitions_digest(), b.prefix_definitions_digest());
    }

    #[test]
    fn deferred_tools_stay_out_of_the_prefix_and_join_only_the_discoverer() {
        let mut reg = ToolRegistry::new();
        reg.register_builtin(named("edit"), P).expect("edit");
        reg.register_mcp(
            "jira",
            Arc::new(Named {
                name: "create_ticket",
                definition_name: "create_ticket",
                description: "Open a Jira ticket",
            }),
            D,
        )
        .expect("jira");
        let prefix_before = reg.prefix_definitions_digest();
        let names: Vec<String> = reg
            .prefix_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["edit"]);

        let mut w1 = DiscoveredTools::new();
        let w2 = DiscoveredTools::new();
        let added = w1.discover(&reg, "JIRA");
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].name, "mcp__jira__create_ticket");
        assert!(
            w1.discover(&reg, "ticket").is_empty(),
            "no double discovery"
        );
        assert_eq!(w1.definitions(&reg), added);

        // The prefix did not change for anyone.
        assert_eq!(reg.prefix_definitions_digest(), prefix_before);

        let view1 = ToolView {
            registry: &reg,
            discovered: &w1,
        };
        let view2 = ToolView {
            registry: &reg,
            discovered: &w2,
        };
        assert!(view1.resolve("mcp__jira__create_ticket").is_ok());
        assert!(matches!(
            view2.resolve("mcp__jira__create_ticket"),
            Err(ToolError::NotDiscovered { .. })
        ));
        assert!(matches!(
            view2.resolve("missing"),
            Err(ToolError::UnknownTool { .. })
        ));
        assert!(view2.resolve("edit").is_ok());
    }

    #[test]
    fn a_changed_tool_list_changes_the_digest() {
        let mut reg = ToolRegistry::new();
        reg.register_mcp("fs", named("read"), P).expect("r");
        let before = reg.prefix_definitions_digest();
        assert_eq!(reg.remove_server("fs"), 1);
        reg.register_mcp(
            "fs",
            Arc::new(Named {
                name: "read",
                definition_name: "read",
                description: "changed",
            }),
            P,
        )
        .expect("re-register");
        assert_ne!(reg.prefix_definitions_digest(), before);
    }

    #[test]
    fn a_definition_naming_another_tool_is_refused() {
        let mut reg = ToolRegistry::new();
        let err = reg
            .register_builtin(
                Arc::new(Named {
                    name: "edit",
                    definition_name: "shell",
                    description: "d",
                }),
                P,
            )
            .expect_err("mismatch");
        assert!(matches!(err, RegistryError::DefinitionMismatch { .. }));
        assert!(reg.is_empty());
    }

    /// A tool whose schema has an `enum` with no `type`.
    struct Untyped;

    #[async_trait::async_trait]
    impl Tool for Untyped {
        fn name(&self) -> &str {
            "untyped"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "untyped".into(),
                description: String::new(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"kind": {"enum": ["a", "b"]}}
                }),
            }
        }
        fn access(&self, _input: &Value) -> Result<Access, ToolError> {
            Ok(Access::none())
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(""))
        }
    }

    #[test]
    fn a_builtin_with_an_untyped_schema_node_is_refused_an_mcp_tool_is_not() {
        let mut reg = ToolRegistry::new();
        let err = reg
            .register_builtin(Arc::new(Untyped), P)
            .expect_err("untyped");
        match err {
            RegistryError::UntypedSchema { tool, paths } => {
                assert_eq!(tool, "untyped");
                assert_eq!(paths, vec!["properties.kind".to_owned()]);
            }
            other => panic!("{other:?}"),
        }
        assert!(reg.is_empty());
        reg.register_mcp("s", Arc::new(Untyped), P)
            .expect("an MCP schema is its server's");
    }

    #[test]
    fn remove_server_leaves_other_servers_and_builtins() {
        let mut reg = ToolRegistry::new();
        reg.register_mcp("a", named("x"), P).expect("a");
        reg.register_mcp("b", named("x"), P).expect("b");
        reg.register_builtin(named("x"), P).expect("builtin");
        assert_eq!(reg.remove_server("a"), 1);
        assert_eq!(reg.names().collect::<Vec<_>>(), vec!["mcp__b__x", "x"]);
    }
}
