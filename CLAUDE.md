# Ai-Brainprint

## 코드 탐색: CodeGraph 우선

이 저장소는 `.codegraph/`로 인덱싱되어 있다. 코드를 찾거나 이해해야 하면
**grep / find / 파일 통독보다 CodeGraph를 먼저** 쓴다.

- `mcp__codegraph__codegraph_explore` (MCP 툴, 우선) — 질문이나 심볼/파일 이름을 넣으면
  해당 심볼의 줄번호 달린 원본 소스 + 호출 경로 + blast radius를 한 번에 준다. Read 대체 가능.
- `codegraph explore "<심볼 또는 질문>"` — 셸 폴백.

읽기 전에 한 번 호출한다. 수정할 때도 마찬가지 — 영향 범위를 보고 편집한다.
전체 파일을 정말 통독해야 할 때만 Read로 내려간다.

## 셸 명령: rtk 경유

`rtk`(Rust Token Killer)가 설치되어 있다. 개발 명령은 rtk로 감싸서 실행한다
(hook이 자동 재작성하지만, 직접 쓸 때도 `rtk cargo test`, `rtk git status` 형태를 쓴다).

- `rtk gain` — 절감량 확인
- `rtk proxy <cmd>` — 필터 없이 원본 출력이 필요할 때만

출력이 긴 명령(cargo build/test/clippy, git diff/log, ls -R)일수록 rtk를 쓴다.
