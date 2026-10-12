//! Every built-in tool's declaration converts for Vertex.
//!
//! Vertex takes function parameters as an OpenAPI subset, so each schema goes
//! through a converter that refuses what the subset cannot hold. A tool whose
//! schema is refused would fail every request that offers it, so each real
//! definition is converted here, not a hand-written sample.

use std::sync::{Arc, Mutex};

use kgraf_graph::GraphStore;
use kgraf_provider_google::vertex_schema;
use kgraf_tools::execpolicy::{Decision, ExecPolicy};
use kgraf_tools::{
    ApplyPatchTool, GraphSliceTool, ListDirectoryTool, ReadFileTool, ReplaceInFileTool,
    SearchFilesTool, ShellTool, Tool, ToolDefinition,
};

/// A parameterless tool sends no `parameters`, so its empty object schema is
/// never converted.
fn is_empty_object(schema: &serde_json::Value) -> bool {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .is_none_or(|p| p.is_empty())
}

pub fn assert_converts(definitions: &[ToolDefinition]) {
    assert!(!definitions.is_empty());
    for definition in definitions {
        if is_empty_object(&definition.input_schema) {
            continue;
        }
        if let Err(why) = vertex_schema(&definition.input_schema) {
            panic!(
                "the schema of `{}` is refused for Vertex: {why}",
                definition.name
            );
        }
    }
}

#[test]
fn every_built_in_tool_schema_converts_for_vertex() {
    let dir = tempfile::tempdir().expect("dir");
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    let (shell, _hook) = ShellTool::new(ExecPolicy::new(Decision::Allow));
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadFileTool::default()),
        Arc::new(ListDirectoryTool),
        Arc::new(SearchFilesTool::default()),
        Arc::new(ApplyPatchTool),
        Arc::new(ReplaceInFileTool::default()),
        Arc::new(shell),
        Arc::new(GraphSliceTool::new(Arc::new(Mutex::new(store)))),
    ];
    let definitions: Vec<ToolDefinition> = tools.iter().map(|t| t.definition()).collect();
    assert_eq!(definitions.len(), 7);
    assert_converts(&definitions);
}
