//! #70: the shared human-surface presentation contract -- what the TUI
//! (and the later local Web UI) say about a canonical fact, in which
//! words. Framework-free on purpose: no terminal or HTML type lives here.
//!
//! Canonical values never change with the locale. A surface maps a wire
//! fact to a [`Msg`] with the functions below and asks [`text`] for the
//! words; the fact itself (an id, a revision, a reason string from the
//! daemon) is shown as delivered. Every message is declared once with
//! both its English and Korean text, so a missing translation does not
//! compile.

use crate::protocol::{
    maintenance::{
        BackendStateWire, CheckWire, DatabaseStateWire, IndexCheckWire, RuntimeCheckWire,
        SchemaCheckWire, WatcherCheckWire,
    },
    query::{
        AnswerStateWire, ChangeKindWire, CoverageWire, CurrentnessWire, ImpactIntentWire,
        NotCurrentReasonWire, SupportWire, TargetResolutionWire, WorkItemStatusWire,
    },
};

/// A UI locale. 0.1.0 ships English and Korean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    En,
    Ko,
}

impl Locale {
    pub const ALL: [Self; 2] = [Self::En, Self::Ko];

    /// Any tag (`ko`, `ko-KR`, `ko_KR.UTF-8`); anything unsupported is
    /// English, never an error.
    #[must_use]
    pub fn from_tag(tag: &str) -> Self {
        let language = tag
            .split(['-', '_', '.'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match language.as_str() {
            "ko" => Self::Ko,
            _ => Self::En,
        }
    }

    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ko => "ko",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::En => Self::Ko,
            Self::Ko => Self::En,
        }
    }
}

macro_rules! messages {
    ($($name:ident => $key:literal, $en:literal, $ko:literal;)*) => {
        /// One human-facing message, identified by a stable key.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Msg {
            $($name,)*
        }

        impl Msg {
            pub const ALL: &'static [Self] = &[$(Self::$name,)*];

            /// The stable message key a catalogue or a test refers to.
            #[must_use]
            pub const fn key(self) -> &'static str {
                match self {
                    $(Self::$name => $key,)*
                }
            }
        }

        /// The words for `msg` in `locale`.
        #[must_use]
        pub const fn text(locale: Locale, msg: Msg) -> &'static str {
            match locale {
                Locale::En => match msg {
                    $(Msg::$name => $en,)*
                },
                Locale::Ko => match msg {
                    $(Msg::$name => $ko,)*
                },
            }
        }
    };
}

messages! {
    TabOverview => "tab.overview", "Overview", "개요";
    TabInspect => "tab.inspect", "Inspect", "살펴보기";
    TabRelations => "tab.relations", "Relations", "관계";
    TabImpact => "tab.impact", "Impact", "영향";
    TabOperations => "tab.operations", "Operations", "작업";

    ConnectionConnected => "connection.connected", "Connected", "연결됨";
    ConnectionNotYet => "connection.not_yet", "Not connected yet", "아직 연결 안 됨";
    ConnectionDisconnected => "connection.disconnected", "Disconnected", "연결 끊김";
    ConnectionIncompatible => "connection.incompatible", "Incompatible", "호환 안 됨";
    ConnectionCleared => "connection.cleared",
        "Earlier answers were cleared: nothing on screen is claimed current.",
        "이전 응답은 지웠습니다. 화면의 어떤 값도 최신이라고 주장하지 않습니다.";
    ConnectionReconnectHint => "connection.reconnect_hint",
        "Press r to reconnect.", "r을 눌러 다시 연결하세요.";

    LabelDaemon => "label.daemon", "Daemon", "데몬";
    LabelProtocol => "label.protocol", "Protocol", "프로토콜";
    LabelClientProtocol => "label.client_protocol", "This client", "이 클라이언트";
    LabelUptime => "label.uptime", "Uptime (s)", "가동 시간(초)";
    LabelProject => "label.project", "Project", "프로젝트";
    LabelWorkspace => "label.workspace", "Workspace", "워크스페이스";
    LabelRoot => "label.root", "Root", "루트";
    LabelProjectHome => "label.project_home", "Project home", "프로젝트 홈";
    LabelRevision => "label.revision", "Revision", "리비전";
    LabelGeneration => "label.generation", "Generation", "세대";
    LabelRuntime => "label.runtime", "Runtime", "런타임";
    LabelWatcher => "label.watcher", "Watcher", "감시자";
    LabelIndex => "label.index", "Index", "인덱스";
    LabelBinding => "label.binding", "Identity binding", "식별자 바인딩";
    LabelDatabases => "label.databases", "Databases", "데이터베이스";
    LabelSemantic => "label.semantic", "Semantic backends", "시맨틱 백엔드";
    LabelWorkingState => "label.working_state", "Working State", "작업 상태";
    LabelTarget => "label.target", "Target", "대상";
    LabelCurrentness => "label.currentness", "Currentness", "최신성";
    LabelCoverage => "label.coverage", "Coverage", "커버리지";
    LabelResolution => "label.resolution", "Resolution", "대상 해석";
    LabelSearch => "label.search", "Search", "검색";
    LabelCandidates => "label.candidates", "Candidates", "후보";
    LabelChangeKind => "label.change_kind", "Change", "변경 종류";
    LabelIncoming => "label.incoming", "Incoming", "들어오는";
    LabelOutgoing => "label.outgoing", "Outgoing", "나가는";
    LabelBoth => "label.both", "Both directions", "양방향";
    LabelConfirmed => "label.confirmed", "Confirmed", "확인됨";
    LabelGaps => "label.gaps", "Gaps", "공백";
    LabelBefore => "label.before", "Before", "이전";
    LabelAfter => "label.after", "After", "이후";
    LabelCreated => "label.created", "Created", "생성";
    LabelUpdated => "label.updated", "Updated", "수정";
    LabelDeleted => "label.deleted", "Deleted", "삭제";
    LabelResources => "label.resources", "Resources", "리소스";
    LabelPage => "label.page", "Pages loaded", "불러온 페이지";
    LabelLocale => "label.locale", "Language", "언어";
    LabelBasisRevision => "label.basis_revision", "basis revision", "기준 리비전";
    LabelIncarnation => "label.incarnation", "Index incarnation", "인덱스 인카네이션";
    LabelCapabilities => "label.capabilities", "Capabilities", "기능";

    CapabilityFiles => "capability.files", "Files", "파일";
    CapabilityStructure => "capability.structure", "Structure", "구조";
    CapabilityRelations => "capability.relations", "Relations", "관계";
    CapabilityImpact => "capability.impact", "Impact", "영향";
    CapabilityPerQuery => "capability.per_query",
        "Not measured here -- each answer states its own currentness and coverage",
        "여기서는 측정 안 됨 -- 각 응답이 자신의 최신성과 커버리지를 밝힘";

    BasisNeverPublished => "basis.never_published",
        "No generation published yet", "아직 게시된 세대 없음";
    BasisUnreadable => "basis.unreadable", "Unreadable", "읽을 수 없음";

    WorkspaceCurrent => "workspace.status.current", "Current", "최신";
    WorkspaceNotCurrentDirty => "workspace.status.not_current_dirty",
        "Not current (changes pending)", "최신 아님 (반영 대기 중인 변경)";
    WorkspaceNotCurrentNeverPublished => "workspace.status.not_current_never_published",
        "Not current (never published)", "최신 아님 (아직 게시된 적 없음)";
    WorkspaceNotInitialized => "workspace.status.not_initialized",
        "Not initialized", "초기화되지 않음";
    WorkspaceAmbiguous => "workspace.status.ambiguous",
        "Ambiguous Workspace", "워크스페이스가 모호함";

    CheckOk => "check.ok", "OK", "정상";
    CheckFailed => "check.failed", "Failed", "실패";
    MetricNotMeasured => "metric.not_measured", "Not measured", "측정 안 됨";
    MetricNotReported => "metric.not_reported",
        "Not reported by the daemon", "데몬이 보고하지 않음";

    RuntimeInactive => "runtime.inactive",
        "Inactive (the next query activates it)", "비활성 (다음 질의 때 활성화)";
    RuntimeActive => "runtime.active", "Active", "활성";
    RuntimeUnavailable => "runtime.unavailable", "Unavailable", "사용 불가";
    WatcherAttached => "watcher.attached", "Attached", "연결됨";
    WatcherUnavailable => "watcher.unavailable",
        "Unavailable (currentness proven at query time)", "사용 불가 (질의 시점에 최신성 확인)";
    WatcherNotStarted => "watcher.not_started", "Not started", "시작 안 됨";

    CapabilityRegistered => "capability.registered",
        "Registered (starts on demand)", "등록됨 (필요할 때 시작)";
    CapabilityUnavailable => "capability.unavailable", "Unavailable", "사용 불가";

    SchemaCurrent => "schema.current", "Current", "최신";
    SchemaMigrationPending => "schema.migration_pending",
        "Migration pending", "마이그레이션 대기";
    SchemaNewer => "schema.newer", "Newer than this binary", "이 바이너리보다 새 버전";
    SchemaLedgerMismatch => "schema.ledger_mismatch",
        "Migration ledger mismatch", "마이그레이션 기록 불일치";
    DatabaseMissing => "database.missing", "Missing", "없음";
    DatabaseUnreadable => "database.unreadable", "Unreadable", "읽을 수 없음";

    CoverageComplete => "coverage.complete", "Complete", "완전";
    CoveragePartial => "coverage.partial", "Partial", "부분";
    CoverageUnsupported => "coverage.unsupported", "Unsupported", "지원 안 됨";

    AnswerConfirmed => "answer.confirmed", "Confirmed", "확인됨";
    AnswerNoneComplete => "answer.none_complete",
        "None (complete coverage)", "없음 (커버리지 완전)";
    AnswerNoneIncomplete => "answer.none_incomplete",
        "None found -- coverage incomplete", "찾지 못함 -- 커버리지 불완전";

    ResolutionResolved => "resolution.resolved", "Resolved", "확정";
    ResolutionAmbiguous => "resolution.ambiguous",
        "Several candidates", "여러 후보";
    ResolutionSingleNonExact => "resolution.single_non_exact",
        "One non-exact candidate", "정확하지 않은 후보 하나";
    ResolutionNotFound => "resolution.not_found", "Not found", "찾지 못함";
    ResolutionNotFoundIncomplete => "resolution.not_found_incomplete",
        "Not found -- coverage incomplete", "찾지 못함 -- 커버리지 불완전";
    ResolutionNotCurrent => "resolution.not_current",
        "Not current", "최신 아님";
    ResolutionNoTarget => "resolution.no_target", "No target", "대상 없음";

    DeliveryMoreAvailable => "delivery.more_available",
        "More available -- press n for the next page", "더 있음 -- n으로 다음 페이지";
    DeliveryComplete => "delivery.complete", "All delivered", "모두 전달됨";

    ImpactPublicSignature => "impact.public_signature_change",
        "Public signature change", "공개 시그니처 변경";
    ImpactRename => "impact.rename", "Rename", "이름 변경";
    ImpactModuleMove => "impact.module_move", "Module move", "모듈 이동";
    ImpactBaseInterface => "impact.base_interface_change",
        "Base class / interface change", "기반 클래스/인터페이스 변경";
    ImpactDelete => "impact.delete", "Delete", "삭제";
    ImpactDomainContract => "impact.domain_contract_change",
        "Domain contract change", "도메인 계약 변경";

    WorkOpen => "work.status.open", "Open", "열림";
    WorkActive => "work.status.active", "Active", "진행 중";
    WorkBlocked => "work.status.blocked", "Blocked", "막힘";
    WorkPaused => "work.status.paused", "Paused", "일시 중지";
    WorkCompleted => "work.status.completed", "Completed", "완료";
    WorkAbandoned => "work.status.abandoned", "Abandoned", "포기";
    WorkNone => "work.none", "No open WorkItems", "열린 WorkItem 없음";
    WorkTruncated => "work.truncated", "More WorkItems not shown", "표시하지 않은 WorkItem 더 있음";

    ActionDoctor => "action.doctor", "Doctor", "진단";
    ActionSync => "action.sync", "Sync", "동기화";
    ActionRebuild => "action.rebuild", "Rebuild", "재구축";
    ActionUninit => "action.uninit", "Uninit", "관리 해제";
    ActionDoctorDescription => "action.doctor.description",
        "Read-only diagnosis; changes nothing.", "읽기 전용 진단. 아무것도 바꾸지 않습니다.";
    ActionSyncDescription => "action.sync.description",
        "Reconcile with the filesystem now.", "지금 파일 시스템과 맞춥니다.";
    ActionRebuildDescription => "action.rebuild.description",
        "Rebuild the rebuildable index from current source.",
        "현재 소스로 재구축 가능한 인덱스를 다시 만듭니다.";
    ActionUninitDescription => "action.uninit.description",
        "Detach this Workspace from active Brainprint management.",
        "이 워크스페이스를 Brainprint 관리에서 분리합니다.";
    ConfirmRebuild => "confirm.rebuild",
        "Rebuild Brainprint's rebuildable index?", "Brainprint의 재구축 가능한 인덱스를 다시 만들까요?";
    ConfirmRebuildDetail => "confirm.rebuild.detail",
        "Source and durable knowledge are not deleted.",
        "소스와 영구 지식은 삭제되지 않습니다.";
    ConfirmUninit => "confirm.uninit",
        "Detach this Workspace from active Brainprint management?",
        "이 워크스페이스를 Brainprint 관리에서 분리할까요?";
    ConfirmUninitDetail => "confirm.uninit.detail",
        "Source, .brainprint/, durable knowledge and the index are kept; `brainprint init` attaches it again.",
        "소스, .brainprint/, 영구 지식, 인덱스는 유지됩니다. `brainprint init`으로 다시 연결합니다.";
    ConfirmPrompt => "confirm.prompt", "y: confirm    n / Esc: cancel", "y: 실행    n / Esc: 취소";

    ResultError => "result.error", "Error", "오류";
    ResultSynced => "result.synced", "Synced", "동기화됨";
    ResultRebuilt => "result.rebuilt", "Rebuilt", "재구축됨";
    ResultDetached => "result.detached", "Detached", "분리됨";
    ResultAlreadyDetached => "result.already_detached",
        "Already detached: nothing changed", "이미 분리됨: 바뀐 것 없음";
    ResultRuntimeStopped => "result.runtime_stopped",
        "Runtime stopped", "런타임 중지됨";

    LocaleSaved => "locale.saved", "Language saved", "언어 저장됨";
    LocaleNotSaved => "locale.not_saved",
        "Language changed for this session only", "이번 세션에만 언어 변경됨";

    HintSearch => "hint.search",
        "Press / to search for a symbol name or a path.", "/를 눌러 심볼 이름이나 경로를 검색하세요.";
    HintSelectTarget => "hint.select_target",
        "Select a target in Inspect first.", "먼저 살펴보기에서 대상을 선택하세요.";
    HintKeys => "hint.keys",
        "Tab: view  ↑↓: move  Enter: run  /: search  r: refresh  L: language  ?: help  q: quit",
        "Tab: 화면  ↑↓: 이동  Enter: 실행  /: 검색  r: 새로고침  L: 언어  ?: 도움말  q: 종료";
    HelpTitle => "help.title", "Keys", "키";
    HelpTabs => "help.tabs", "Tab / Shift+Tab, 1-5   switch view", "Tab / Shift+Tab, 1-5   화면 전환";
    HelpMove => "help.move", "↑ ↓ PgUp PgDn   move / scroll", "↑ ↓ PgUp PgDn   이동 / 스크롤";
    HelpEnter => "help.enter", "Enter   run the selected item", "Enter   선택 항목 실행";
    HelpSearch => "help.search", "/   search (Inspect); Esc cancels", "/   검색 (살펴보기); Esc로 취소";
    HelpMore => "help.more", "n   next page when more is available", "n   더 있을 때 다음 페이지";
    HelpChange => "help.change", "c   next change kind (Impact)", "c   다음 변경 종류 (영향)";
    HelpRefresh => "help.refresh", "r   refresh / reconnect", "r   새로고침 / 다시 연결";
    HelpLocale => "help.locale", "L   switch language", "L   언어 전환";
    HelpQuit => "help.quit",
        "q   quit (the daemon keeps running)", "q   종료 (데몬은 계속 실행)";
}

// ------------------------------------------------- fact -> message maps

#[must_use]
pub const fn currentness(value: &CurrentnessWire) -> Msg {
    match value {
        CurrentnessWire::Current => Msg::WorkspaceCurrent,
        CurrentnessWire::NotCurrent(NotCurrentReasonWire::ResourceIndexDirty) => {
            Msg::WorkspaceNotCurrentDirty
        }
        CurrentnessWire::NotCurrent(NotCurrentReasonWire::ResourceIndexNeverPublished) => {
            Msg::WorkspaceNotCurrentNeverPublished
        }
    }
}

#[must_use]
pub const fn resolution(value: &TargetResolutionWire) -> Msg {
    match value {
        TargetResolutionWire::Resolved(_) => Msg::ResolutionResolved,
        TargetResolutionWire::MultipleCandidates => Msg::ResolutionAmbiguous,
        TargetResolutionWire::SingleNonExactCandidate => Msg::ResolutionSingleNonExact,
        TargetResolutionWire::NotFound => Msg::ResolutionNotFound,
        TargetResolutionWire::NotFoundIncompleteCoverage => Msg::ResolutionNotFoundIncomplete,
        TargetResolutionWire::NotCurrent => Msg::ResolutionNotCurrent,
        TargetResolutionWire::NoTarget => Msg::ResolutionNoTarget,
    }
}

#[must_use]
pub const fn answer_state(value: AnswerStateWire) -> Msg {
    match value {
        AnswerStateWire::Confirmed => Msg::AnswerConfirmed,
        AnswerStateWire::NoneUnderCompleteCoverage => Msg::AnswerNoneComplete,
        AnswerStateWire::NoneWithIncompleteCoverage => Msg::AnswerNoneIncomplete,
    }
}

/// A direct relation answer's coverage, read off the counts the daemon
/// sent: an Unsupported scope is Unsupported (never "0 relations"), any
/// recorded gap or unattributed reach makes it Partial.
#[must_use]
pub const fn relation_coverage(coverage: &CoverageWire) -> Msg {
    if let Some(scope) = &coverage.scope
        && matches!(scope.support, SupportWire::Unsupported)
    {
        return Msg::CoverageUnsupported;
    }
    let partial_scope = match &coverage.scope {
        Some(scope) => matches!(scope.support, SupportWire::Partial),
        None => false,
    };
    if partial_scope
        || coverage.gaps > 0
        || coverage.ambiguous > 0
        || coverage.requires_semantics > 0
        || coverage.unsupported_construct > 0
        || coverage.truncated > 0
        || coverage.unattributed > 0
        || coverage.unconfirmed_owners > 0
        || coverage.semantic.not_current
    {
        return Msg::CoveragePartial;
    }
    Msg::CoverageComplete
}

#[must_use]
pub const fn check(value: &CheckWire) -> Msg {
    match value {
        CheckWire::Ok => Msg::CheckOk,
        CheckWire::Failed { .. } => Msg::CheckFailed,
        CheckWire::NotMeasured { .. } => Msg::MetricNotMeasured,
    }
}

#[must_use]
pub const fn index(value: &IndexCheckWire) -> Msg {
    match value {
        IndexCheckWire::Current => Msg::WorkspaceCurrent,
        IndexCheckWire::NotCurrent { .. } => Msg::ResolutionNotCurrent,
        IndexCheckWire::NotMeasured { .. } => Msg::MetricNotMeasured,
    }
}

#[must_use]
pub const fn runtime(value: &RuntimeCheckWire) -> Msg {
    match value {
        RuntimeCheckWire::Inactive => Msg::RuntimeInactive,
        RuntimeCheckWire::Active { .. } => Msg::RuntimeActive,
        RuntimeCheckWire::Unavailable { .. } => Msg::RuntimeUnavailable,
    }
}

#[must_use]
pub const fn watcher(value: &WatcherCheckWire) -> Msg {
    match value {
        WatcherCheckWire::Attached => Msg::WatcherAttached,
        WatcherCheckWire::Unavailable { .. } => Msg::WatcherUnavailable,
        WatcherCheckWire::NotStarted => Msg::WatcherNotStarted,
    }
}

#[must_use]
pub const fn backend(value: &BackendStateWire) -> Msg {
    match value {
        BackendStateWire::Registered => Msg::CapabilityRegistered,
        BackendStateWire::Unavailable { .. } => Msg::CapabilityUnavailable,
    }
}

#[must_use]
pub const fn database(value: &DatabaseStateWire) -> Msg {
    match value {
        DatabaseStateWire::Missing => Msg::DatabaseMissing,
        DatabaseStateWire::Unreadable { .. } => Msg::DatabaseUnreadable,
        DatabaseStateWire::Opened { schema, .. } => match schema {
            SchemaCheckWire::Current { .. } => Msg::SchemaCurrent,
            SchemaCheckWire::MigrationPending { .. } => Msg::SchemaMigrationPending,
            SchemaCheckWire::Newer { .. } => Msg::SchemaNewer,
            SchemaCheckWire::LedgerMismatch { .. } => Msg::SchemaLedgerMismatch,
        },
    }
}

#[must_use]
pub const fn work_status(value: WorkItemStatusWire) -> Msg {
    match value {
        WorkItemStatusWire::Open => Msg::WorkOpen,
        WorkItemStatusWire::Active => Msg::WorkActive,
        WorkItemStatusWire::Blocked => Msg::WorkBlocked,
        WorkItemStatusWire::Paused => Msg::WorkPaused,
        WorkItemStatusWire::Completed => Msg::WorkCompleted,
        WorkItemStatusWire::Abandoned => Msg::WorkAbandoned,
    }
}

#[must_use]
pub const fn change_kind(value: ChangeKindWire) -> Msg {
    match value {
        ChangeKindWire::Structural(ImpactIntentWire::PublicSignatureChange) => {
            Msg::ImpactPublicSignature
        }
        ChangeKindWire::Structural(ImpactIntentWire::Rename) => Msg::ImpactRename,
        ChangeKindWire::Structural(ImpactIntentWire::ModuleMove) => Msg::ImpactModuleMove,
        ChangeKindWire::Structural(ImpactIntentWire::BaseInterfaceChange) => {
            Msg::ImpactBaseInterface
        }
        ChangeKindWire::Delete => Msg::ImpactDelete,
        ChangeKindWire::DomainContractChange => Msg::ImpactDomainContract,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::protocol::query::{
        FreshnessWire, GapAttributionWire, ScopeStateWire, SemanticScopeWire,
    };

    #[test]
    fn every_message_has_a_unique_key_and_non_empty_text_in_every_locale() {
        let mut keys = HashSet::new();
        for &msg in Msg::ALL {
            assert!(keys.insert(msg.key()), "duplicate key {}", msg.key());
            assert!(
                msg.key().contains('.')
                    && msg
                        .key()
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "key style: {}",
                msg.key()
            );
            for locale in Locale::ALL {
                assert!(!text(locale, msg).trim().is_empty(), "{locale:?} {msg:?}");
            }
        }
    }

    #[test]
    fn korean_is_a_translation_not_a_copy_of_the_english_labels() {
        for msg in [
            Msg::WorkspaceCurrent,
            Msg::MetricNotMeasured,
            Msg::CoverageUnsupported,
            Msg::CapabilityUnavailable,
            Msg::ActionSync,
            Msg::ActionRebuild,
        ] {
            assert_ne!(text(Locale::En, msg), text(Locale::Ko, msg), "{msg:?}");
        }
    }

    #[test]
    fn an_unsupported_locale_is_english() {
        for tag in ["en", "en-US", "xx", "", "fr_FR.UTF-8", "KO-kr-weird"] {
            let expected = if tag.to_ascii_lowercase().starts_with("ko") {
                Locale::Ko
            } else {
                Locale::En
            };
            assert_eq!(Locale::from_tag(tag), expected, "{tag:?}");
        }
        assert_eq!(Locale::from_tag("ko_KR.UTF-8"), Locale::Ko);
        assert_eq!(Locale::from_tag(Locale::Ko.tag()), Locale::Ko);
    }

    fn coverage(support: Option<SupportWire>, gaps: usize) -> CoverageWire {
        CoverageWire {
            attribution: GapAttributionWire::SourceScoped,
            gaps,
            ambiguous: 0,
            requires_semantics: 0,
            unsupported_construct: 0,
            truncated: 0,
            unattributed: 0,
            unconfirmed_owners: 0,
            scope: support.map(|support| ScopeStateWire {
                resource: crate::ResourceId::generate(),
                support,
                freshness: FreshnessWire::Fresh,
            }),
            semantic: SemanticScopeWire {
                contexts: 0,
                conflicts: 0,
                not_current: false,
            },
        }
    }

    #[test]
    fn an_unsupported_scope_is_never_a_complete_zero() {
        assert_eq!(
            relation_coverage(&coverage(Some(SupportWire::Unsupported), 0)),
            Msg::CoverageUnsupported
        );
        assert_eq!(
            relation_coverage(&coverage(Some(SupportWire::Supported), 2)),
            Msg::CoveragePartial
        );
        assert_eq!(relation_coverage(&coverage(None, 0)), Msg::CoverageComplete);
    }

    #[test]
    fn the_locale_changes_words_never_the_fact() {
        let fact = CurrentnessWire::NotCurrent(NotCurrentReasonWire::ResourceIndexDirty);
        let msg = currentness(&fact);
        assert_ne!(text(Locale::En, msg), text(Locale::Ko, msg));
        assert_eq!(currentness(&fact), msg, "mapping is locale-free");
        assert_eq!(
            fact,
            CurrentnessWire::NotCurrent(NotCurrentReasonWire::ResourceIndexDirty)
        );
    }
}
