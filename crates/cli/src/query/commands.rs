//! The seven Task 10 operation families' exact CLI grammar (#24 §16,
//! §18).

use std::num::NonZeroUsize;

use brainprint_core::{WorkItemId, protocol::query::*};
use clap::{Args, Parser, Subcommand};

use super::{
    delivery::{DeliveryArgs, SearchBudgetArgs},
    knowledge::KnowledgeArgs,
    target::TargetArgs,
    vocab::{
        ChangeArg, DirectionArg, LanguageArg, RelationKindArg, ResourceKindArg, RoleArg, StatusArg,
    },
};

/// Common flags every query command accepts (#24 §16).
#[derive(Debug, Clone, Args)]
pub struct CommonArgs {
    /// Workspace locator; the CLI canonicalizes this to an absolute path
    /// before it crosses IPC. Defaults to the current working directory.
    #[arg(long, default_value = ".")]
    pub workspace: String,
    /// Machine-readable output: one `QueryResponse` JSON on stdout.
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub client_id: Option<String>,
    #[arg(long)]
    pub session_id: Option<String>,
}

impl CommonArgs {
    #[must_use]
    pub fn has_correlation(&self) -> bool {
        self.client_id.is_some() || self.session_id.is_some()
    }

    #[must_use]
    pub fn correlation(&self) -> Option<CorrelationWire> {
        self.has_correlation().then(|| CorrelationWire {
            client_id: self.client_id.clone(),
            session_id: self.session_id.clone(),
            ..CorrelationWire::default()
        })
    }
}

#[derive(Debug, Parser)]
#[command(name = "brainprint", disable_help_subcommand = true)]
pub enum Cli {
    Install,
    Status,
    Init {
        path: Option<String>,
    },
    Find {
        #[command(subcommand)]
        mode: FindCommand,
    },
    Inspect(InspectArgs),
    Relations(RelationsArgs),
    Impact(ImpactArgs),
    Context {
        #[command(subcommand)]
        mode: ContextCommand,
    },
    Knowledge {
        #[command(subcommand)]
        mode: KnowledgeCommand,
    },
    Structure(StructureArgs),
}

#[derive(Debug, Subcommand)]
pub enum FindCommand {
    /// Candidates for a selector. Reads no source.
    Target(FindTargetArgs),
    /// Resource inventory from the index. Reads no source.
    Files(FindFilesArgs),
    /// Explicit source-text search; never an automatic fallback.
    Text(FindTextArgs),
}

#[derive(Debug, Args)]
pub struct FindTargetArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub delivery: DeliveryArgs,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct FindFilesArgs {
    #[arg(long, default_value_t = NonZeroUsize::new(200).expect("200 != 0"))]
    pub limit: NonZeroUsize,
    #[arg(long)]
    pub directory: Option<String>,
    #[arg(long)]
    pub recursive: bool,
    #[arg(long = "path-prefix")]
    pub path_prefix: Option<String>,
    #[arg(long, value_enum)]
    pub role: Option<RoleArg>,
    #[arg(long, value_enum)]
    pub language: Option<LanguageArg>,
    #[arg(long, value_enum)]
    pub kind: Option<ResourceKindArg>,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct FindTextArgs {
    #[arg(long)]
    pub literal: Option<String>,
    #[arg(long)]
    pub regex: Option<String>,
    #[command(flatten)]
    pub search_budget: SearchBudgetArgs,
    #[arg(long = "case-insensitive")]
    pub case_insensitive: bool,
    #[arg(long = "path-prefix")]
    pub path_prefix: Option<String>,
    #[arg(long)]
    pub preview: bool,
    #[command(flatten)]
    pub common: CommonArgs,
}

/// The resolved target's exact current declaration source and both
/// directions of its direct relations.
#[derive(Debug, Args)]
pub struct InspectArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub delivery: DeliveryArgs,
    #[command(flatten)]
    pub common: CommonArgs,
}

/// The resolved target's direct confirmed relations (one hop, unpaged).
#[derive(Debug, Args)]
pub struct RelationsArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long, value_enum)]
    pub direction: DirectionArg,
    #[arg(long = "kind", value_enum)]
    pub kind: Vec<RelationKindArg>,
    #[command(flatten)]
    pub common: CommonArgs,
}

/// What a declared change to the resolved target would affect.
#[derive(Debug, Args)]
pub struct ImpactArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long, value_enum)]
    pub change: ChangeArg,
    #[command(flatten)]
    pub delivery: DeliveryArgs,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Subcommand)]
pub enum ContextCommand {
    /// Context for an explicit or plain edit.
    Change(ContextChangeArgs),
    /// Context to resume a WorkItem's handoff.
    Resume(ContextResumeArgs),
}

#[derive(Debug, Args)]
pub struct ContextChangeArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long, value_enum)]
    pub change: Option<ChangeArg>,
    #[arg(long = "work-item")]
    pub work_item: Option<WorkItemId>,
    #[command(flatten)]
    pub knowledge: KnowledgeArgs,
    #[command(flatten)]
    pub delivery: DeliveryArgs,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct ContextResumeArgs {
    #[arg(long = "work-item")]
    pub work_item: WorkItemId,
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub knowledge: KnowledgeArgs,
    #[command(flatten)]
    pub delivery: DeliveryArgs,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Subcommand)]
pub enum KnowledgeCommand {
    /// Current applicable rules without a target or WorkItem.
    Rules(KnowledgeRulesArgs),
    /// Current WorkItems by explicit status.
    WorkItems(KnowledgeWorkItemsArgs),
    /// One-hop lineage of an exact Policy/Decision id.
    Lineage(KnowledgeLineageArgs),
    /// Handoff history of a WorkItem, newest first.
    Handoffs(KnowledgeHandoffsArgs),
}

#[derive(Debug, Args)]
pub struct KnowledgeRulesArgs {
    #[command(flatten)]
    pub knowledge: KnowledgeArgs,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct KnowledgeWorkItemsArgs {
    #[arg(long = "status", value_enum, required = true)]
    pub status: Vec<StatusArg>,
    #[arg(long, default_value_t = NonZeroUsize::new(50).expect("50 != 0"))]
    pub limit: NonZeroUsize,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct KnowledgeLineageArgs {
    #[arg(long = "project-policy")]
    pub project_policy: Option<brainprint_core::PolicyId>,
    #[arg(long = "user-policy")]
    pub user_policy: Option<brainprint_core::PolicyId>,
    #[arg(long = "decision")]
    pub decision: Option<brainprint_core::DecisionId>,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct KnowledgeHandoffsArgs {
    #[arg(long = "work-item")]
    pub work_item: WorkItemId,
    #[arg(long, default_value_t = NonZeroUsize::new(20).expect("20 != 0"))]
    pub limit: NonZeroUsize,
    #[command(flatten)]
    pub common: CommonArgs,
}

/// `LABEL=PREFIX` for `--group-path`.
pub fn parse_label_prefix(raw: &str) -> Result<(String, String), String> {
    raw.split_once('=')
        .map(|(label, prefix)| (label.to_owned(), prefix.to_owned()))
        .ok_or_else(|| "expected LABEL=PREFIX".to_owned())
}

/// A Task 9 structural summary, grouped by exactly one dimension.
#[derive(Debug, Args)]
pub struct StructureArgs {
    #[arg(long = "group-path", value_parser = parse_label_prefix)]
    pub group_path: Vec<(String, String)>,
    #[arg(long = "group-directory")]
    pub group_directory: bool,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub depth: Option<usize>,
    #[arg(long = "group-resource-role")]
    pub group_resource_role: bool,
    #[arg(long = "group-resource-language")]
    pub group_resource_language: bool,
    #[arg(long = "group-resource-kind")]
    pub group_resource_kind: bool,
    #[arg(long = "scope-path-prefix")]
    pub scope_path_prefix: Option<String>,
    #[arg(long = "scope-role", value_enum)]
    pub scope_role: Option<RoleArg>,
    #[arg(long = "scope-language", value_enum)]
    pub scope_language: Option<LanguageArg>,
    #[arg(long = "scope-kind", value_enum)]
    pub scope_kind: Option<ResourceKindArg>,
    #[arg(long = "relation-kind", value_enum)]
    pub relation_kind: Vec<RelationKindArg>,
    #[arg(long = "exclude-ungrouped")]
    pub exclude_ungrouped: bool,
    #[arg(long = "include-cycles")]
    pub include_cycles: bool,
    #[arg(long = "member-sample-limit")]
    pub member_sample_limit: Option<NonZeroUsize>,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug)]
pub enum StructureArgsError {
    NoGroupingMode,
    MultipleGroupingModes,
    DirectoryModeMissingRootOrDepth,
}

impl StructureArgs {
    pub fn grouping(&self) -> Result<GroupingSpecWire, StructureArgsError> {
        let mut modes = 0_u8;
        if !self.group_path.is_empty() {
            modes += 1;
        }
        if self.group_directory {
            modes += 1;
        }
        if self.group_resource_role {
            modes += 1;
        }
        if self.group_resource_language {
            modes += 1;
        }
        if self.group_resource_kind {
            modes += 1;
        }
        if modes == 0 {
            return Err(StructureArgsError::NoGroupingMode);
        }
        if modes > 1 {
            return Err(StructureArgsError::MultipleGroupingModes);
        }
        if !self.group_path.is_empty() {
            return Ok(GroupingSpecWire::PathPrefixes(
                self.group_path
                    .iter()
                    .map(|(label, prefix)| PathGroupRuleWire {
                        label: label.clone(),
                        prefix: prefix.clone(),
                    })
                    .collect(),
            ));
        }
        if self.group_directory {
            let (Some(root), Some(depth)) = (self.root.clone(), self.depth) else {
                return Err(StructureArgsError::DirectoryModeMissingRootOrDepth);
            };
            return Ok(GroupingSpecWire::DirectoryDepth { root, depth });
        }
        if self.group_resource_role {
            return Ok(GroupingSpecWire::ResourceRole);
        }
        if self.group_resource_language {
            return Ok(GroupingSpecWire::ResourceLanguage);
        }
        Ok(GroupingSpecWire::ResourceKind)
    }

    #[must_use]
    pub fn resource_scope(&self) -> SummaryResourceScopeWire {
        SummaryResourceScopeWire {
            path_prefix: self.scope_path_prefix.clone(),
            role: self.scope_role.map(Into::into),
            language: self.scope_language.map(Into::into),
            kind: self.scope_kind.map(Into::into),
        }
    }
}
