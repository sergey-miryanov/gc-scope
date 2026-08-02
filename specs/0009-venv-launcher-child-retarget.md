# 0009 — Re-target `attach` from a venv launcher to its real interpreter child

**Status:** Not started
**Kind:** feature — ergonomics
**Effort:** M
**Origin:** deferred plan of 2026-07-20, follow-up to
[ADR 0002](../docs/adr/0002-version-split-runtime-finding.md), which records this as a
known blind spot.
**Respects:** [ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md),
[ADR 0002](../docs/adr/0002-version-split-runtime-finding.md)

## 1. Problem statement

On Windows, a virtualenv's `Scripts\python.exe` in redirector form is a thin launcher: it
reads `pyvenv.cfg`, starts the **base** interpreter as a child process, and waits. It never
loads `python3XX.dll` into its own address space.

So an operator who does the obvious thing — take the PID of the `python.exe` they started,
run `gcscope gc-stats <pid>` — gets a failure, on a machine where gcscope works fine. The
interpreter they want is one row down in `list-pids`, and nothing in the error says so:

```
26412  ...  N  N  -        .3.8\Scripts\python.exe        ← launcher (shim)
30244  ...  Y  Y  3.8.10   C:\python\python38\python.exe  ← real interpreter (child)
```

## 2. Solution

Pointing gcscope at a venv launcher just works. `attach` notices that the PID holds no
interpreter, finds the child that does, and rebuilds the session against it — then says so,
once, so the operator knows which process they are actually looking at. Pointing gcscope at
a launcher with no resolvable child still fails, promptly and with a clear reason.

## 3. User stories

1. As a Windows developer who started `.venv\Scripts\python.exe`, I want `gc-stats` on that
   PID to show me my script's GC activity, so that I do not have to know that my venv uses
   a redirector.
2. As an operator, I want to be told when gcscope re-targeted to a child PID, so that the
   number in my notes matches the process I measured.
3. As an operator pointing at a genuinely dead end, I want a prompt, clear failure, so that
   a launcher with no interpreter under it does not look like a hang.
4. As an operator on Linux or macOS, I want no behavior change at all, so that a
   Windows-shaped fix cannot cost me anything.
5. As an operator attaching to a launcher that has not spawned its child yet, I want the
   existing startup-timeout window to cover the child appearing, so that `run` and a fast
   `attach` do not race the shim.
6. As a maintainer, I want the happy path untouched, so that adding this cannot slow down
   the ordinary attach every command performs.
7. As a maintainer, I want the runtime finder to stop being able to locate a runtime it
   cannot read, so that the inconsistency behind this gap does not resurface elsewhere.

## 4. Implementation decisions

### The blocker this has to solve

Every read goes through the attached PID, and the child's runtime is in a **separate
address space** — unreachable through the launcher's handle. Finding the child's runtime
*address* is not enough: the entire session (handle, `runtime_addr`, every subsequent read)
must be rebuilt against the child PID. This is why the current version-first dispatch fails
early and cleanly at detection rather than limping into a cross-address-space read that
would fail *open*, in the way ADR 0004 warns about.

### The change

On the initial `PySession::attach(pid)` resolve failure — no python module mapped, or
`version::detect` finds nothing — walk `memory::process::get_child_pids`, attempt the full
version-first resolve against each child, and **rebuild the whole session** against the
first child that resolves. Not its address: the session.

The walk runs only on the failure path, so the happy path is untouched — which is what
keeps this from being a cost every command pays for a Windows-only case.

### The inconsistency to reconcile at the same time

`memory::process::search_pid_and_children` recurses into children but returns only
`(addr, path)` — it **drops the child PID** — while `attach` keeps reading from the parent.
The recursive search can therefore locate a runtime it can never read. Either have it
return the resolving PID, or delete the recursion in favour of this attach-level walk.
Leaving both is the worst option: two child-walks with different contracts.

### Open questions to settle when picked up

- **Depth and breadth.** How deep to walk (a launcher spawns one child; nested shims could
  go deeper), and how to choose among multiple resolving children — first-wins, or prefer
  the one whose cmdline matches the launcher's.
- **Reporting.** Story 2 asks for visibility. Decide the surface: a one-line note to stderr
  on re-target, or a field in the session that each command reports in its own voice.
- **`WaitPolicy` interaction.** The child walk should compose with `StartupTimeoutPolicy`'s
  retry window (story 5) rather than fail permanently on the first miss.

## 5. Seams and testing decisions

- **Seam:** `PySession::attach` plus the honest signal it already carries. The re-target
  outcome should be observable the way `layout_source()` makes the layout-cache hit
  observable — a public field or accessor saying which PID the session actually bound to.
  That is the highest seam, needs no fixture beyond a real launcher, and follows ADR 0005's
  choice of an honest signal over a `test-hooks` gate.
- **New seam needed:** the accessor above, if `PySession` does not already expose the bound
  PID. It doubles as the mechanism for story 2, so it is not test-only scaffolding.
- **Fixture problem — be honest about it.** A redirector-style venv exists only on Windows,
  and `tests/common::SpawnedPython` spawns an interpreter directly. Either the Windows CI
  leg creates a venv and spawns through `Scripts\python.exe` (real, and worth it since this
  is a Windows-only feature), or the test is `#[ignore]`d and run manually. Choose the
  former if the leg can create a venv; a Windows-only feature with no Windows test is how
  this gap appeared in the first place.
- **What makes a good test here:** assert the **bound PID and the decoded shape** — that
  attaching to the launcher yields stats identical to attaching to the child directly. "It
  attached" is not enough; a re-target to the *wrong* child would attach cleanly and report
  another process's GC activity.
- **Prior art:** `tests/lifecycle.rs` for observing session state through a public signal;
  `tests/memory.rs` for the child-tree functions (`get_child_pids`) against a live process;
  `tests/live_smoke.rs` for the shape assertion to compare against.
- **Cases:**
  1. `gc-stats <launcher-pid>` on a Windows redirector venv: succeeds, and its output
     matches `gc-stats <child-pid>` directly.
  2. `find-runtime <launcher-pid>` prints the child's `PyRuntime` address; `tui` on a
     launcher PID renders the child's data.
  3. A launcher with no resolvable child fails promptly — bounded walk, no hang. Assert the
     time bound, since "prompt" is the story.
  4. A non-launcher PID resolves without entering the child walk at all — the happy-path
     guard for story 6.
  5. Linux and macOS legs unchanged (story 4): those venvs symlink or exec in place, so the
     venv `python` *is* the interpreter and no child exists to follow.

## 6. Out of scope

- **`monitor` and `run`.** Already handled: the poll loop re-discovers children and
  grandchildren of every alive PID each tick, so they reach the real interpreter regardless
  of the starting PID. This change must not disturb that path.
- **`list-pids`.** No information is lost there today — the launcher shows `N N -` and the
  child sits directly below it with the correct version. Collapsing the two rows into one
  is a display decision, not part of this.
- **Non-Windows venv layouts**, which need nothing.

## 7. Further notes

Quality-of-life, not a correctness blocker: no functionality is unreachable today, only
less ergonomic, and `list-pids` surfaces the child PID that works. That is why it sits below
the defects in the queue despite being the most user-visible item in the folder.
