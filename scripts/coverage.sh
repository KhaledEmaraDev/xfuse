#! /bin/sh -e
#
# Generate a XFUSE code coverage report
#
# Requirements:
# sudo pkg install grcov
# cargo install grcov
#
# Usage:
# scripts/coverage.sh

export LLVM_PROFILE_FILE="xfuse-%p-%m.profraw"
export RUSTFLAGS="-Cinstrument-coverage"
TOOLCHAIN=nightly
cargo +$TOOLCHAIN build --all-features
cargo +$TOOLCHAIN test --all-features

grcov . --binary-path $PWD/target/debug -s . -t html --branch \
	--ignore-not-existing \
	--ignore "tests/*" \
	--excl-line "#\[derive\(" \
	-o ./coverage/
