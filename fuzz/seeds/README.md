# Fuzz seed corpus

Each `fuzz/seeds/<target>/` directory is the checked-in input corpus for that
target. `scripts/fuzz-smoke.sh` replays it before fuzzing, so a seed that makes
a target crash, hang, or exhaust memory fails the lane in seconds.

Today only `load_and_instantiate/` carries a checked-in corpus: it is the
imported and minimised engine corpus (480 inputs, reduced with `cmin` from the
488 `module_probing` seeds that previously lived downstream in Selium). The
other five targets start from an empty checked-in corpus **by design**: they
had no prior engine corpus to import, and an empty directory is not committed
here. Their coverage grows in the gitignored `fuzz/corpus/<target>/` working
corpus, which the nightly workflow keeps in the Actions cache and uploads as
the `fuzz-corpus-<target>-*` artifact. Promote coverage-adding inputs from
those artifacts into `fuzz/seeds/<target>/` as a deliberate corpus refresh; do
not copy an artifact straight in without minimising it first (`cmin`).

Regression seeds for fixed crashes always get a descriptive name
(`table_min_overflow.wasm`); corpus inputs keep the SHA-1 content-hash names
libFuzzer produces.
