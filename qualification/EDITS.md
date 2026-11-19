# Frozen edit scripts

Recorded on 2026-11-19 before any candidate timing sample. Each patch changes one function body and keeps the program's results. Apply with `patch -p1` from the workload root, time the rebuild, then reverse the patch. Do not retarget these hunks after seeing a candidate sample.

| Workload | Class | Patch | Function |
| --- | --- | --- | --- |
| ripgrep | leaf | `edits/ripgrep-leaf.patch` | `search_path` |
| ripgrep | wide | `edits/ripgrep-wide.patch` | `Searcher::search_path` |
| syn | leaf | `edits/syn-leaf.patch` | `Error::new` |
| syn | wide | `edits/syn-wide.patch` | `parse_str` |
| regex | leaf | `edits/regex-leaf.patch` | `Regex::is_match` |
| regex | wide | `edits/regex-wide.patch` | `Regex::new` |
| fd | holdout | `edits/fd-holdout.patch` | `ensure_single_search_pattern_is_not_a_path` |

Each patch applies to the clean tree on its own. Cold uses the leaf source with an empty target directory. No-op repeats the leaf command with the patch absent.
