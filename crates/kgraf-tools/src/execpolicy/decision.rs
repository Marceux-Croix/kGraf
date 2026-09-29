// Copied from openai/codex codex-rs/execpolicy/src/decision.rs
// at e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0.
// Changes: `Decision::parse`'s error type was codex's own `crate::error::Error`
// (not vendored, see `execpolicy/mod.rs`); it now returns `Result<Self, String>`.

/// One command policy's outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Decision {
    /// Command may run without further approval.
    Allow,
    /// Request explicit user approval; rejected outright when running with `approval_policy="never"`.
    Prompt,
    /// Command is blocked without further consideration.
    Forbidden,
}

impl Decision {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "allow" => Ok(Self::Allow),
            "prompt" => Ok(Self::Prompt),
            "forbidden" => Ok(Self::Forbidden),
            other => Err(format!(
                "invalid decision '{other}': must be one of allow, prompt, forbidden"
            )),
        }
    }
}
