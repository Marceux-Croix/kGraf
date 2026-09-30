//! The orchestrator's tool declarations convert for Vertex.
//!
//! Vertex takes function parameters as an OpenAPI subset, so each schema goes
//! through a converter that refuses what the subset cannot hold. A tool whose
//! schema is refused would fail every request that offers it, so each real
//! definition is converted here, not a hand-written sample.

use kgraf_orchestrator::{ChallengeTool, ContractLine, SessionLine, session_tools};
use kgraf_provider_google::vertex_schema;
use kgraf_tools::Tool;

#[test]
fn every_orchestrator_tool_schema_converts_for_vertex() {
    let mut tools = session_tools(&SessionLine::new());
    tools
        .push(std::sync::Arc::new(ChallengeTool::new(ContractLine::new()))
            as std::sync::Arc<dyn Tool>);
    assert!(tools.len() >= 5);
    for tool in tools {
        let definition = tool.definition();
        let empty = definition
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .is_none_or(|p| p.is_empty());
        if empty {
            continue;
        }
        if let Err(why) = vertex_schema(&definition.input_schema) {
            panic!("`{}` is refused for Vertex: {why}", definition.name);
        }
    }
}
