# Golden diff fixtures

These files are the **shared dialect authority**. `kaijutsu-diff` parses them in
`tests/fixtures.rs`; kernel and app tests reach them through
`kaijutsu_diff::fixtures::path(..)` rather than pasting diff text into their own
tests, so the two sides cannot drift into divergent dialects.

Add a fixture here *and* to the inventory constants in `src/fixtures.rs` — the
fixture test walks those constants and fails on any file that is present in one
place and missing from the other.

## `canonical/`

Byte-for-byte what `kaijutsu_diff::format` emits. The contract is identity:
`format(parse(text)) == text`.

| file | what it pins |
|---|---|
| `single_file_modify.diff` | the baseline shape: `diff --git`, `---`/`+++`, one hunk |
| `multi_file.diff` | three sections in one patch: modify, add, delete |
| `add_file.diff` | creation via `--- /dev/null`, `@@ -0,0 +1,N @@` |
| `delete_file.diff` | deletion via `+++ /dev/null`, `@@ -1,N +0,0 @@` |
| `rename_with_edits.diff` | `rename from`/`rename to` beside real hunks |
| `rename_pure.diff` | a rename with **zero** hunks — headers only |
| `no_newline.diff` | `\ No newline at end of file` on both sides |
| `quoted_path.diff` | a path with a space, quoted in all three headers |
| `empty_context_line.diff` | an unchanged empty line: the body line is `" "` |
| `section_heading.diff` | text after the closing `@@` survives a round trip |
| `truncated.diff` | the leading `#!kaijutsu-diff truncated:` marker |

## `external/`

Valid input in the accepted dialect that is **not** canonical. The contract is
idempotence, not identity: `format(parse(t))` may differ from `t`, but running
it again changes nothing.

| file | what it pins |
|---|---|
| `git_index_headers.diff` | real `git diff` output: `index`, `new file mode`, `deleted file mode` accepted then dropped |
| `plain_diff_u.diff` | `diff -u` output: no `diff --git`, no `a/`/`b/` prefixes, tab timestamps |
| `explicit_single_counts.diff` | `@@ -1,1 +1,1 @@` → canonicalizes to `@@ -1 +1 @@` |
| `git_octal_quoted_path.diff` | git's `core.quotePath` octal escapes → decoded, re-emitted unquoted |
| `stripped_trailing_space.diff` | an empty context line whose trailing space a mail transport ate |

## `invalid/`

Every one of these must produce a typed `DiffError`, never a partial model.

| file | error |
|---|---|
| `binary_git.diff` | `BinaryPatch` |
| `binary_files_differ.diff` | `BinaryPatch` |
| `malformed_hunk_header.diff` | `MalformedHunkHeader` |
| `hunk_count_mismatch.diff` | `HunkCountMismatch` |
| `unknown_extension.diff` | `UnsupportedExtension` (`mode change` is not modelled) |
| `copy_headers.diff` | `UnsupportedExtension` (copies are not modelled) |
| `garbage_preamble.diff` | `ExpectedFileHeader` (this is a `format-patch` mailbox, not a diff) |
| `stray_no_newline.diff` | `StrayNoNewline` |
| `bad_no_newline_marker.diff` | `UnexpectedHunkLine` (a `\` line that is not the no-newline marker) |
| `missing_post_image.diff` | `MissingPostImageHeader` |
