#!/bin/bash
# Correctness/currentness gate (gate.py, unchanged) on a fresh 6775dac clone with the 0e3f182 product build.
R=@T14_ROOT@; B=$R/clean/repo/target/release
E="env -i SP=$R R=$B HOME=$R/ab/home XDG_RUNTIME_DIR=$R/ab/run PATH=$B:$R/ab/toolbin:/usr/bin:/bin:/usr/sbin:/sbin RUSTUP_HOME=/Users/pixel/.rustup CARGO_HOME=/Users/pixel/.cargo"
W=$R/gate_ws; rm -rf $W; git clone -q --no-hardlinks $R/clean/repo $W && git -C $W checkout -q 6775dac && git -C $W rm -rq benchmarks/i5-task14 && git -C $W -c user.name=t14 -c user.email=t14@example.invalid commit -qm "measured clone"
cd $R; kill $(cat ab/daemon.pid 2>/dev/null) 2>/dev/null; sleep 1.2
find $R/ab/home/.brainprint -mindepth 1 -maxdepth 1 ! -name config.toml -exec rm -rf {} +
$E python3 ab/startd.py; $E brainprint install >/dev/null; $E brainprint init $W >/dev/null
printf 'format_version = 1\nextra_excluded_directory_names = []\nproject_execution_trust = "Trusted"\n' > $W/.brainprint/config.toml
kill $(cat ab/daemon.pid); sleep 1.2; $E python3 ab/startd.py
$E python3 ab/gate.py $W $R/gate.json
