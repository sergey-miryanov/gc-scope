# Fuzz seeds

Tracked starting inputs for `cargo fuzz`, one directory per target. CI copies them into
`fuzz/corpus/<target>/` before each run.

`fuzz/corpus/` and `fuzz/artifacts/` are gitignored — libFuzzer's scratch space, restored
from CI cache rather than from git. This directory holds the inputs worth keeping forever.

## What belongs here

**Every input that has ever crashed or hung a target.** A fuzzer starting cold may not
rediscover a bug it once found: the CI leg is a 120s smoke gate, and reassembling a valid
header takes coverage-guided search many generations. Without the seed, a reintroduced bug
can sit green for weeks.

Name the file for the defect rather than libFuzzer's hash — `goblin-entry-point-underflow`,
not `crash-e48ef8ee…`.

**A seed does not replace a unit test.** The seed keeps the fuzzer honest on Linux CI; the
test states the correct behaviour, runs everywhere, and fails informatively. Add both. See
[`docs/testing-policy.md`](../../docs/testing-policy.md).

## Current seeds

### `scan_image_for_version/goblin-entry-point-underflow`

A thin 64-bit Mach-O whose `__TEXT` has `fileoff` above `vmaddr`, plus an LC_MAIN.
`goblin-0.10.7` computes `vmaddr - fileoff + entryoff` unchecked while parsing
(`src/mach/mod.rs:280`), so this panicked from inside `MachO::parse`. Guarded by
`macho_entry_point_math_is_safe` in `src/memory/binary.rs`.
