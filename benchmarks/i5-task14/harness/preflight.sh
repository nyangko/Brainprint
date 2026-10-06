#!/bin/bash
# Contamination preflight for the #32 rerun after #73. Prints one line per check; exit 1 on any failure.
R=@T14_ROOT@; B=@T14_ROOT@/clean/repo/target/release; fail=0
chk(){ if eval "$2"; then echo "PASS $1"; else echo "FAIL $1"; fail=1; fi; }
APATH=$R/ab/toolbin:/usr/bin:/bin:/usr/sbin:/sbin
chk "A PATH has no brainprint" "! env -i PATH=$APATH /bin/sh -c 'command -v brainprint brainprintd brainprint-mcp brainprint-agent' >/dev/null"
chk "measured env has no ANTHROPIC_BASE_URL/proxy" "! env -i HOME=/Users/pixel PATH=$APATH /usr/bin/env | grep -qiE 'BASE_URL|PROXY|HEADROOM'"
chk "B binaries are the 0e3f182 build" "[ \"$(git -C $R/clean/repo rev-parse --short HEAD)\" = 0e3f182 ] && [ -z \"$(git -C $R/clean/repo status --porcelain --untracked-files=no)\" ]"
chk "B MCP points at the current build" "grep -q \"$B/brainprint-mcp\" $R/ab/mcp-B.json"
proto14(){ env -i HOME=$R/ab/home XDG_RUNTIME_DIR=$R/ab/run $B/brainprint status --json 2>/dev/null | grep -q protocol_version.:14; }
# protocol 14 of B is checked per answer afterwards (every Brainprint result carries protocol_version); a
# daemon-status check here would fail spuriously after an A session, which stops the daemon.
chk "B build is the protocol-14 source" "grep -q 'PROTOCOL_VERSION: u32 = 14' $R/clean/repo/crates/core/src/build.rs"
chk "isolated HOME holds only the global config" "[ \"$(ls -A $R/ab/home/.brainprint)\" = config.toml ] || [ -z \"$(ls -A $R/ab/home/.brainprint | grep -v -e config.toml -e data -e run)\" ]"
exit $fail
