<p align="center">
  <img src="../assets/brainprint-hero.svg" alt="Brainprint" width="100%" />
</p>

<p align="center">
  <strong>AI 코딩 에이전트가 덜 읽고, 덜 재탐색하고, 실제 판단이 필요한 작업에 추론을 쓰게 하는 로컬 우선 컨텍스트 런타임.</strong>
</p>

<p align="center">
  <a href="../README.md">English</a> · 한국어 · <a href="./README.ja.md">日本語</a>
</p>

# Brainprint

AI 코딩 에이전트는 추론에는 강하지만, 일반 소프트웨어가 훨씬 싸게 처리할 수 있는 작업에도 반복해서 컨텍스트와 도구 호출을 소비합니다.

- 저장소 구조를 다시 탐색하고,
- 바뀌지 않은 소스를 다시 읽고,
- 같은 caller/import 관계를 다시 추적하고,
- 새 세션이나 compaction 이후 프로젝트 상태를 다시 구성하고,
- raw tool output을 다시 정렬·중복 제거·그룹화·필터링하고,
- 한 번 준비해 둘 수 있었던 Git/test/build 탐색을 반복합니다.

Brainprint는 프로젝트와 에이전트 사이에 위치하는 **지속형 로컬 우선 컨텍스트 런타임**으로 개발되고 있습니다.

Brainprint의 역할은 에이전트의 판단을 대신하는 것이 아닙니다. 에이전트가 현재 작업에 필요한 **최소 충분하고, 최신이며, 구조화된 프로젝트 정보**를 받도록 해서 구현·trade-off·검증에 추론을 집중하게 하는 것이 목적입니다.

> **Brainprint가 어떤 사실을 알고 있거나, 결정론적으로 계산·정렬·중복 제거·준비·검증할 수 있다면 에이전트가 추론 토큰을 써서 다시 할 필요가 없어야 합니다.**

## 핵심 아이디어

기존 에이전트 워크플로우는 흔히 다음처럼 흘러갑니다.

```text
Agent
  → ls / find
  → rg
  → 파일 read
  → import / caller 관계 재구성
  → raw 결과 sort / dedupe / filter
  → Git 상태 확인
  → 프로젝트 규칙 재탐색
  → 그제야 실제 변경 판단 시작
```

Brainprint는 이런 보조 작업을 모델 밖으로 옮기는 것을 목표로 합니다.

```text
Project / Workspace
        ↓
    brainprintd
        ↓
   fresh structured truth
        ↓
  prepare / dedupe / bound
        ↓
      MCP / CLI
        ↓
      Agent
        ↓
 판단 / 수정 / 검증
```

프로젝트 자체가 Source of Truth입니다. Brainprint는 소스 백업, Git 대체재, 자율 프로젝트 관리자, 범용 대화 아카이브가 아닙니다.

## 제품 원칙

### 1. 추가가 아니라 대체 — Substitution, not addition

Brainprint를 호출한 뒤 에이전트가 똑같은 목적의 `ls/find/rg/read/git`을 다시 실행한다면 의미가 없습니다.

Brainprint는 반복 탐색 앞에 도구 하나를 더 추가하는 것이 아니라, 반복 탐색 자체를 제거해야 합니다.

### 2. 바로 작업할 수 있을 만큼 준비 — Prepare enough to work

Brainprint가 exact current source range, 직접 consumer, 관련 test, 결과의 불확실성까지 알고 있는데 파일명이나 라인 번호만 주는 것은 부족합니다.

너무 작은 projection 때문에 에이전트가 다시 read/rg를 해야 한다면 실패입니다.

### 3. 결정론적 작업은 Brainprint가 처리 — Deterministic work belongs in Brainprint

런타임은 다음과 같은 값싸고 반복 가능한 연산을 직접 처리해야 합니다.

```text
sort
dedupe
filter
group
count
set intersection
range merge
overlap removal
stable ordering
candidate bounding
pagination
continuation
diff
known-state comparison
budget enforcement
deterministic output compaction
```

수백 개의 raw item을 모델에 넘긴 뒤 정렬하고 줄이라고 하는 것은 토큰 최적화가 아닙니다. 일반적인 계산을 가장 비싼 레이어로 옮기는 것뿐입니다.

### 4. 거대한 공유 컨텍스트가 아니라 공유된 truth — Shared truth, not shared giant context

여러 에이전트는 같은 Project/Workspace의 최신 truth를 공유할 수 있어야 하지만, 각 worker가 거대한 parent transcript를 복제해서 들고 다니면 안 됩니다.

### 5. 절감보다 정확성 우선 — Correctness before savings

`STALE`, `PARTIAL`, `UNRESOLVED`, `UNSUPPORTED`, truncation 같은 상태는 명시적으로 유지해야 합니다.

토큰을 줄이기 위해 불확실성을 숨기는 것은 실패입니다.

### 6. 숙련 판단은 에이전트가 맡는다

Brainprint는 fact, evidence, state, rule, verification target을 준비합니다.

에이전트가 담당하는 것은 다음입니다.

- 문제 해석
- 변경 전략 선택
- 코드 작성
- trade-off 평가
- 불확실성 해석
- 최종 검증 판단

## 아키텍처

Brainprint는 하나의 global local daemon과 Workspace별 runtime을 중심으로 설계합니다.

```text
Codex / Claude / Gemini / other agents
                │
          MCP / thin Skill
                │
                ▼
          global brainprintd
                │
        ┌───────┼────────┐
        ▼       ▼        ▼
   Workspace A  B        C
        │
        ├─ Resource / Symbol / Occurrence
        ├─ Relation / unresolved / candidates
        ├─ freshness / revision / generation
        ├─ current source preparation
        ├─ project / working state
        └─ command intelligence
```

기본 모델은 다음과 같습니다.

- **Project truth는 공유합니다.**
- **Task context는 공유하지 않습니다.**
- 같은 Workspace의 비싼 parsing/semantic 결과는 여러 Agent가 재사용해야 합니다.
- WorkItem / Role / Persona metadata는 이후 projection을 조정할 수 있지만 source fact, Relation, freshness 자체를 바꾸면 안 됩니다.

## Brainprint 0.1.0 → 0.5.0 → 1.0.0

Brainprint는 다섯 개의 pre-1.0 제품 단계로 발전한 뒤, 별도의 stabilization/hardening 단계를 거쳐 1.0.0으로 승격합니다.

각 버전은 해당 릴리스에서 **가장 강하게 개선할 가치 축**을 의미합니다. 뒤 버전의 주제라고 해서 실사용에 필요한 최소 기능이 그 전에는 없다는 뜻이 아닙니다.

| Version | 중심 목표 | 핵심 질문 |
| --- | --- | --- |
| **0.1.0** | **Token & Context Economy** | 정확성을 유지하면서 반복 read/search/context/support work를 얼마나 제거할 수 있는가? |
| **0.2.0** | **Deterministic Work Offload** | 일반 소프트웨어가 할 수 있는 계산을 LLM에게 시키지 않을 수 있는가? |
| **0.3.0** | **Rules & Project Intelligence** | 현재 작업에 필요한 규칙·결정·작업 상태만 정확히 전달할 수 있는가? |
| **0.4.0** | **Language & Ecosystem Expansion** | 실제 개발 시스템을 얼마나 더 넓고 정확하게 이해할 수 있는가? |
| **0.5.0** | **Persona & Role Awareness** | project truth를 바꾸지 않고 작업자에 맞게 projection만 조정할 수 있는가? |
| **1.0.0** | **Stable Release / Hardening** | Brainprint를 stable이라고 부를 만큼 정확하고 안정적이며 최적화·유지보수·복구·문서화가 되어 있는가? |

상세 릴리스 목표는 [#18 — Brainprint 0.1.0~0.5.0 → 1.0.0 제품 로드맵](https://github.com/nyangko/Brainprint/issues/18)에서 관리합니다.

### 0.1.0 — Token & Context Economy

첫 실용 릴리스는 단순 indexing demo가 아니라 실제 프로젝트에서 dogfooding할 수 있어야 합니다.

0.1.0 baseline에는 다음이 포함됩니다.

- local daemon과 Workspace identity
- structural code intelligence
- Relation Graph와 impact evidence
- current-source preparation
- semantic backend baseline
- project rules / decisions / Working State baseline
- request-shaped Context Projection
- Git/test/lint/typecheck/build intelligence
- high-level MCP + thin Skill
- deterministic sort/dedupe/filter/bounding baseline
- multi-Agent shared truth
- real-project benchmark와 dogfooding

### 0.2.0 — Deterministic Work Offload

0.1.0의 baseline을 확장해 모델이 해야 하는 기계적 작업을 더 줄입니다.

result shaping, 반복 diagnostics 축약, deterministic diff/state comparison, delivery reuse, verification delta 등을 강화하고, 효과가 실제 benchmark로 확인되는 adaptive optimization만 추가합니다.

### 0.3.0 — Rules & Project Intelligence

Project Policy, Decision, Blueprint, Working State lineage, precedence, conflict 처리, handoff/resume, applicable-rule projection을 강화합니다.

목표는 새 Agent가 매번 **“이 프로젝트를 어떤 방식으로 작업해야 하는지”** 다시 알아내게 하지 않는 것입니다.

### 0.4.0 — Language & Ecosystem Expansion

0.1.0부터 Python, TypeScript/JavaScript, React, Svelte, C#, Rust의 실용 baseline을 목표로 합니다.

0.4.0에서는 additional language, framework adapter, ORM/database relation, route, event, queue, cache/config semantics, cross-project relation 등 semantic depth와 생태계 이해 범위를 확장합니다.

### 0.5.0 — Persona & Role Awareness

Persona는 의도적으로 가장 마지막 단계입니다.

Role/Persona는 **어떤 evidence를 더 먼저 보여줄지, 어느 정도 상세하게 projection할지**는 바꿀 수 있지만 project truth 자체를 바꾸면 안 됩니다.

```text
Truth
  → task/context selection
  → optional Role/Persona adjustment
```

다음 구조는 금지합니다.

```text
Persona
  → truth interpretation
```

Persona가 실제 측정 가능한 가치를 거의 만들지 못한다면 최소 기능으로 유지합니다.

### 1.0.0 — Stable Release

1.0.0은 0.5.0 다음의 또 다른 기능 묶음이 아닙니다.

별도의 stabilization/hardening을 거친 뒤에만 Brainprint를 stable이라고 부릅니다.

1.0.0 승격 전에는 최소한 다음을 검증해야 합니다.

- 실제 프로젝트에서 correctness와 주요 bug가 닫혔는지
- 핵심 workflow regression coverage가 충분한지
- CPU/RAM/I/O/index size와 장시간 background cost가 허용 가능한지
- token/context/tool-call 개선이 실제로 측정되는지
- multi-Agent 안정성과 recovery가 검증되었는지
- migration/upgrade path가 안전한지
- 핵심 모듈의 코드 품질과 유지보수성이 충분한지
- CLI/MCP/public contract가 stable release 수준으로 정리되었는지
- README/Wiki/API 실제 동작이 서로 일치하는지
- language/feature coverage와 limitation을 과장하지 않았는지

0.5.0이 끝났다는 이유만으로 **1.0.0을 태그하지 않습니다.**

## 현재 구현 상태

현재 Brainprint는 **0.1.0을 구현 중**입니다.

구현 로드맵은 [#12 — P0 구현 로드맵](https://github.com/nyangko/Brainprint/issues/12)에서 관리합니다.

| Workstream | 상태 |
| --- | --- |
| I0 — Benchmark Harness / Skeleton | 완료 |
| I1 — Core Runtime Foundation | 완료 |
| I2 — Structural Intelligence | 완료 |
| I3 — Relation Graph | 진행 중 |
| I4 — Semantic Backends | 예정 |
| I5 — Project Intelligence + Projection + MCP/Skill | 예정 |
| I6 — Command Intelligence | 예정 |
| I7 — UX / Recovery / Acceptance | 예정 |

구현은 의도적으로 아래부터 쌓아 올립니다. 먼저 신뢰할 수 있는 project fact와 freshness를 만들고, 그 위에 relation과 semantic을 올린 뒤, projection과 Agent-facing interface를 연결합니다.

## 0.1.0에서 기대하는 사용감

예를 들어 다음 요청이 있다고 가정합니다.

```text
"이 메서드의 시그니처를 바꾸고 영향받는 곳을 전부 수정해."
```

목표하는 경험은 다음이 아닙니다.

```text
Agent → search → read → search → import 확인 → caller 검색
      → test 검색 → source 재읽기 → state 재구성 → 수정
```

대신 다음에 가까워야 합니다.

```text
Brainprint
  → current target source
  → confirmed callers / references / type consumers
  → 관련 current source ranges
  → related tests
  → applicable project rules
  → unresolved / unsupported gaps
  → current workspace state

Agent
  → 판단
  → 수정
  → 검증
```

## 성공을 어떻게 측정하는가

기능 개수는 가장 중요한 성공 지표가 아닙니다.

Brainprint는 실제로 다음을 줄였는지 측정해야 합니다.

- Agent tool calls
- 변경되지 않은 source의 반복 read
- 전달되는 raw source bytes
- context/token volume
- 여러 Agent 사이의 중복 분석
- Agent가 직접 수행하는 불필요한 sort/dedupe/filter/group/count
- 장시간 작업에서 support-work ratio

동시에 다음도 함께 측정합니다.

- correctness와 missed dependency
- false-positive Relation
- stale/partial 오류
- latency
- CPU/RAM/I/O
- index size

불확실성을 숨겨서 만든 token 절감은 성공으로 보지 않습니다.

## 데이터 모델과 저장 철학

Brainprint는 프로젝트의 두 번째 복사본이 아니라 **구조화된 이해**를 저장합니다.

예를 들어 다음을 유지합니다.

- stable Project/Workspace/Resource identity
- Symbol과 Occurrence
- canonical Relation
- unresolved/candidate evidence
- fingerprint와 revision state
- project decision과 Working State

원본 source, image, audio, video 등의 project asset은 외부 resource로 남고 identity/location/fingerprint로 참조합니다. rebuildable index는 언제든 다시 만들 수 있어야 합니다.

## Agent 연동

0.1.0에서 의도하는 연동 구조는 다음과 같습니다.

```text
Agent plugin / extension
        │
   thin Skill + MCP
        │
        ▼
   brainprintd
```

Agent별 packaging 방식은 달라도 Core는 하나여야 합니다. Codex, Claude, Gemini 등의 각 client가 같은 Workspace를 위해 별도의 project intelligence engine을 다시 띄우면 안 됩니다.

Marketplace별 packaging이나 더 풍부한 one-click distribution은 0.1.0 integration contract가 검증된 뒤 발전시킬 수 있습니다.

## 문서화

pre-1.0 개발 중에도 문서는 계속 갱신하지만, **1.0.0 승격 전에는 실제 acceptance된 구현을 기준으로 README/Wiki를 최종 감사**합니다.

Wiki는 다음 범위를 다루게 됩니다.

- architecture와 identity model
- freshness/revision/generation
- structural/semantic intelligence
- Relation Graph
- project/Working State
- Context Projection과 Task Packet
- MCP / Skill / Agent integration
- CLI
- storage/data policy
- multi-Agent concurrency
- recovery/troubleshooting
- benchmark/acceptance
- language coverage
- design decisions와 known limitations
- developer/contributor guide

## 설계 및 구현 추적

- [#12 — P0 구현 로드맵](https://github.com/nyangko/Brainprint/issues/12)
- [#18 — Brainprint 0.1.0~0.5.0 → 1.0.0 제품 로드맵](https://github.com/nyangko/Brainprint/issues/18)

설계 이슈는 architecture decision의 Source of Truth로 유지하고, 구현 이슈는 실제 실행 작업을 추적합니다.

## 현재 성숙도

**Active pre-1.0 development. 현재 목표: 0.1.0.**

0.x 개발 중에는 interface, storage detail, integration contract가 변경될 수 있습니다.

계획된 기능 단계가 모두 끝났다는 이유만으로 1.0.0으로 승격하지 않습니다. correctness, bug, performance, resource usage, code quality, upgrade/recovery, public contract, documentation을 별도 stabilization/hardening review에서 검증해야 합니다.
