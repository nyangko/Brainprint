//! Knowledge applicability CLI grammar (#24 §18): explicit JSON flags for
//! scope layers/directives rather than a large convenience grammar (#24
//! §28). Task 10 validates actual applicability; the CLI implements no
//! precedence of its own.

use brainprint_core::{BlueprintApplicationId, protocol::query::*};
use clap::Args;

#[derive(Debug, Clone, Default, Args)]
pub struct KnowledgeArgs {
    /// One applicability layer (a JSON array of `KnowledgeScope`), least
    /// specific first. Repeatable.
    #[arg(long = "scope-layer-json")]
    pub scope_layer_json: Vec<String>,
    /// One request-local directive, as JSON. Repeatable.
    #[arg(long = "directive-json")]
    pub directive_json: Vec<String>,
    #[arg(long = "decision-topic")]
    pub decision_topic: Vec<String>,
    #[arg(long = "preference-key")]
    pub preference_key: Vec<String>,
    #[arg(long = "state-key")]
    pub state_key: Vec<String>,
    #[arg(long = "blueprint-application")]
    pub blueprint_application: Vec<BlueprintApplicationId>,
}

#[derive(Debug)]
pub struct InvalidKnowledgeJson(pub serde_json::Error);

pub struct ConvertedKnowledge {
    pub scope_layers: Vec<Vec<KnowledgeScopeWire>>,
    pub directives: Vec<RequestDirectiveWire>,
    pub knowledge_refs: ProjectionKnowledgeRefsWire,
}

impl KnowledgeArgs {
    pub fn into_wire(self) -> Result<ConvertedKnowledge, InvalidKnowledgeJson> {
        let scope_layers = self
            .scope_layer_json
            .iter()
            .map(|json| serde_json::from_str(json))
            .collect::<Result<Vec<_>, _>>()
            .map_err(InvalidKnowledgeJson)?;
        let directives = self
            .directive_json
            .iter()
            .map(|json| serde_json::from_str(json))
            .collect::<Result<Vec<_>, _>>()
            .map_err(InvalidKnowledgeJson)?;
        Ok(ConvertedKnowledge {
            scope_layers,
            directives,
            knowledge_refs: ProjectionKnowledgeRefsWire {
                decision_topics: self.decision_topic,
                preference_keys: self.preference_key,
                state_keys: self.state_key,
                blueprint_applications: self.blueprint_application,
            },
        })
    }
}
