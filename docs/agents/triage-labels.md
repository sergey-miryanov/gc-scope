# Triage labels

The skills speak in terms of five canonical triage roles. This file maps each role to the
string this repo actually writes.

| Role in the skills | String we write   | Meaning                                      |
| ------------------ | ----------------- | -------------------------------------------- |
| `needs-triage`     | `needs-triage`    | Maintainer needs to evaluate this issue      |
| `needs-info`       | `needs-info`      | Waiting on the reporter for more information |
| `ready-for-agent`  | `ready-for-agent` | Fully specified, ready for an AFK agent      |
| `ready-for-human`  | `ready-for-human` | Requires human implementation                |
| `wontfix`          | `wontfix`         | Will not be actioned                         |

The tracker is local markdown (see `issue-tracker.md`), so these are values on the `Status:`
line inside an issue file, not labels applied through a CLI. Nothing creates them ahead of
time and nothing validates the spelling, so write them exactly as above.

When a skill names a role ("apply the AFK-ready triage label"), write the matching string
from this table. To change the vocabulary, edit the middle column.
