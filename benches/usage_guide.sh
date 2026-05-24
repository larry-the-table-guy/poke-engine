# List of commands to produce performance metrics
# this file is not intended to be ran as a shell script

#### Type sizes
# uses bin crate <https://crates.io/crates/top-type-sizes>
cargo clean
RUSTFLAGS=-Zprint-type-sizes cargo +nightly build --no-default-features --features=gen9 -j1 > type-sizes.txt
# filter by type name. These examples filter by specific modules
top-type-sizes -f "^state::" < type-sizes.txt | less
top-type-sizes -f "^mcts::" < type-sizes.txt | less
top-type-sizes -f "^instruction::" < type-sizes.txt | less
top-type-sizes -f "^choices::" < type-sizes.txt | less

#### Allocations
# heaptrack from <https://github.com/KDE/heaptrack>
# Locate code that frequently allocates.
# NOTE: This seems to have significant overhead when the allocation rate is high.
# that should not pose an issue after a few optimizations are made
heaptrack -- ./target/release/deps/mcts_bench-SOME_HASH bench --stats=none < ./benches/states/example.txt

#### bench
# Terminology:
# - a Sample is an MCTS search on a State
# - a Report is one bench run with potentially many Samples
#
# `./benches/mcts_bench.rs` takes in line-separated serialized states from stdin.
# Runs 1 search on each state, collects stats.
# Prints a report about time and memory usage, as well as metrics about the core data structures
#
# It's advisable to organize your states by similarity.
# Separate by gen, of course, but also consider early-game vs mid-game vs late-game, or other properties you might wish to compare
#
# Quick reminder on ways to pass files via stdin in shell/bash:
# `command args args < ./some/file`
# `cat ./some/file1 ./some/file2 ./dir/* | command args args`
# https://unix.stackexchange.com/questions/292253/how-to-use-cat-command-on-find-commands-output
# `find ./dir ...flags... -exec cat {} + | command args args`

## Bench for fixed time (default 5 seconds), report iters and tree stats
# Defaults to a markdown report, can output a binary format with --binary
cargo bench --bench mcts_bench --features gen9 -- bench --time=5 < ./benches/states/example.txt
# add  --stats=short  for greatly reduced output

## bench "fixed" amount of iterations w/ perf stat
# uses a high time limit so that we hit the iteration cap.
# skip stats to avoid adding noise. (probably minor but better not to)
# you can find the path of the bench binary in the output of `cargo bench ...`
# very important to use the binary and not just wrap `cargo bench`, even cached builds add a lot of noise
perf stat -- ./target/release/deps/mcts_bench-SOME_HASH bench --stats=none --time=100 --threads=0 < ./benches/states/example.txt

## Pretty print a binary report
# input via stdin, outputs to stdout
cargo bench --bench mcts_bench --features gen9 -- print < ./report

## Diff one or more binary reports
cargo bench --bench mcts_bench --features gen9 -- diff ./report1 ./report2
