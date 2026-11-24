use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a tool can do to the machine, which is what policy rules and MCP annotations are written
/// against: a role can allow every `read` tool without naming them, including ones added later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Looks without changing anything.
    Read,
    /// Changes files or configuration.
    Write,
    /// Runs a program.
    Exec,
    /// Removes or overwrites something that cannot be had back.
    Destroy,
    /// Sleeps, restarts or powers off a machine.
    Power,
    /// Changes Legion itself: its tools, its policy, its fleet.
    Admin,
}

impl Effect {
    /// Every effect, from the mildest to the widest.
    pub const ALL: [Effect; 6] = [
        Effect::Read,
        Effect::Write,
        Effect::Exec,
        Effect::Destroy,
        Effect::Power,
        Effect::Admin,
    ];

    /// The effect as policy files and the wire spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Read => "read",
            Effect::Write => "write",
            Effect::Exec => "exec",
            Effect::Destroy => "destroy",
            Effect::Power => "power",
            Effect::Admin => "admin",
        }
    }
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effect {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Effect::ALL
            .into_iter()
            .find(|effect| effect.as_str() == text)
            .ok_or_else(|| {
                let known: Vec<&str> = Effect::ALL.iter().map(|effect| effect.as_str()).collect();
                format!(
                    "`{text}` is not an effect; the effects are {}",
                    known.join(", ")
                )
            })
    }
}

/// How a tool describes itself, which is all a client needs to call it correctly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// `family.verb`, such as `fs.read`.
    pub name: String,
    /// What the tool does and when to reach for it, for a person or an agent choosing a tool.
    pub description: String,
    /// What the tool can do to the machine.
    pub effect: Effect,
    /// A JSON Schema for the one object the tool takes.
    pub input_schema: Value,
}
