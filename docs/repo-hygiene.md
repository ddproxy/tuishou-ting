# Repository hygiene

`tests/repo_hygiene.rs` scans the text files in a checkout (excluding the
`.git`, `target` and `dist` directories) for names that do not belong in a
public repository, so every tracked file is covered. It runs as part of
`cargo test`.

The check reports:

- `target`-style fixture names built from a lower-case tool word;
- repository slugs taken from public source URLs or markdown link text;
- URL hostnames;
- email addresses;
- copyright holders that are not approved;
- prose naming an earlier source repository;
- short uppercase tool labels.

Every reported token must have an entry in `repo-hygiene-allowlist.txt` for the
file it occurs in. Each entry is `<relative-path>:<token>` followed by an
optional `#` comment explaining the exception. Blank lines and comment lines
are ignored.

Current exceptions cover canonical public project, badge and registry URLs, the
MIT copyright notice, and the neutral fixture names used by the collector
tests. The canonical URLs keep their existing owner and host segments because
they are public project addresses; the registry index is fixed by Cargo.
`NOTES.md` records the values kept deliberately and why.

To add an exception, justify it in a comment and add the narrowest possible
`path:token` entry; do not allow a whole tool name. To remove an exception,
remove both the occurrence and its entry.
