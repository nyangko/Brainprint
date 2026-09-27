# Ai-Brainprint

## 프로젝트 이해: Brainprint 우선

현재/완전한 프로젝트 사실은 **Brainprint를 먼저** 쓴다 (`integrations/brainprint/SKILL.md`).

- `brainprint.find` — 위치 / 파일 목록 / 텍스트
- `brainprint.inspect` — 정확한 현재 소스 / 이해
- `brainprint.relations` — 의존 / 영향
- `brainprint.context` — 변경 / 재개 / 규칙 / 이력 / 구조 / 상태

Brainprint가 current/complete로 이미 돌려준 사실은 다시 탐색하지 않는다.

Native 탐색(Read/Grep/Glob/셸)은 Brainprint가 partial / stale / unsupported /
ambiguous / truncated를 보고했을 때, 또는 원본 검증이 명시적으로 필요할 때 쓴다.
CodeGraph 등 사용자가 따로 설치한 도구도 그때의 외부 fallback일 뿐이며,
Brainprint가 대신 호출하지 않는다.

## 셸 명령 출력: rtk 경유

`rtk`(Rust Token Killer)가 설치되어 있다. 출력이 긴 개발 명령(cargo build/test/clippy,
git diff/log)은 `rtk cargo test`, `rtk git status` 형태로 실행한다.
`rtk proxy <cmd>` — 필터 없는 원본 출력이 필요할 때만. 프로젝트 이해 경로가 아니다.
