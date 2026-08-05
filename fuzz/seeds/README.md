# Fuzz seeds

Tracked starting inputs for `cargo fuzz`, one directory per target. CI copies them into
`fuzz/corpus/<target>/` before each run.

`fuzz/corpus/` and `fuzz/artifacts/` are gitignored — they are libFuzzer's scratch space,
and the corpus is restored from CI cache rather than from git. This directory is the part
that lives in the repo, so it holds only inputs worth keeping forever.

## What belongs here

**Every input that has ever crashed or hung a target.** A fuzzer starting from an empty
corpus is not guaranteed to rediscover a bug it once found: the CI leg is a 120s smoke
gate, and coverage-guided search over structured formats can take many generations to
reassemble a valid header. Without the seed, a reintroduced bug may sit green for weeks.

Name the file for the defect, not the hash libFuzzer generated
(`goblin-entry-point-underflow`, not `crash-e48ef8ee…`).

**A crash seed is not a substitute for a unit test.** The seed keeps the fuzzer honest on
Linux CI; the unit test states what the correct behaviour is, runs on every platform, and
is what actually fails informatively. Add both. See
[`docs/testing-policy.md`](../../docs/testing-policy.md).

## Current seeds

### `scan_image_for_version/goblin-entry-point-underflow`

A thin 64-bit Mach-O whose `__TEXT` segment has `fileoff` above `vmaddr`, plus an LC_MAIN.
`goblin-0.10.7` computes `vmaddr - fileoff + entryoff` unchecked while parsing
(`src/mach/mod.rs:280`), so this panicked from inside `MachO::parse`. Guarded by
`macho_entry_point_math_is_safe` in `src/memory/binary.rs`.
