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

CRATEDIR=$(realpath $(dirname $0)/..)
PROFRAWDIR=$CRATEDIR/target/tmp
echo $PROFRAWDIR
export LLVM_PROFILE_FILE="$PROFRAWDIR/xfuse-%p-%m.profraw"
export RUSTFLAGS="-Cinstrument-coverage"
TOOLCHAIN=nightly
cargo +$TOOLCHAIN build --all-features
cargo +$TOOLCHAIN test --all-features -- --test-threads=1

grcov . --binary-path $CRATEDIR/target/debug -s . -t html --branch \
	--ignore-not-existing \
	--ignore "tests/*" \
	--excl-line "#\[derive\(" \
	-o ./coverage/ \
	$PROFRAWDIR
