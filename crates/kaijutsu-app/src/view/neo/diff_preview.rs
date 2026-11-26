use std::collections::HashMap;

use kaijutsu_client::ContextMirror;
use kaijutsu_types::{BlockId, ContentType};
use crate::view::diff_view::{content_hash, diff_body};
use crate::text::diff::{DiffLineClass, DiffPreview, PreviewLine, build_diff_preview};
use crate::ui::msdf_panel::truncate;
use crate::ui::quick_context::{LineTone, PanelLine};

pub(super) type Previews = HashMap<BlockId, (u64, DiffPreview)>;

pub(super) fn preview<'a>(mirror: &ContextMirror, id: BlockId, cache: &'a mut Previews) -> Option<&'a DiffPreview> {
    let block = mirror.block(&id)?;
    let body = diff_body(block.kind, block.content_type, &block.content)?;
    let hash = content_hash(body);
    let entry = cache.entry(id).and_modify(|(old, preview)| {
        if *old != hash {
            *old = hash;
            *preview = build_diff_preview(body, ContentType::Diff);
        }
    }).or_insert_with(|| (hash, build_diff_preview(body, ContentType::Diff)));
    Some(&entry.1)
}

pub(super) fn first_file(preview: &DiffPreview) -> Option<&str> {
    let line = preview.lines.iter().find(|line| line.class == DiffLineClass::FileHeader)?;
    Some(preview.plain_text[line.start..line.end].trim_end())
}

pub(super) fn rows(preview: &DiffPreview, columns: usize, budget: usize) -> Vec<PanelLine> {
    if budget == 0 || columns == 0 {
        return Vec::new();
    }
    let row = |line: &PreviewLine| PanelLine {
        text: truncate(preview.plain_text[line.start..line.end].trim_end_matches('\n'), columns),
        tone: LineTone::Diff(line.class),
    };
    let lines = &preview.lines;
    if lines.len() <= budget {
        return lines.iter().map(row).collect();
    }

    let capacity = budget - usize::from(budget >= 4);
    let mut result = Vec::with_capacity(budget);
    if let Some(file) = lines.iter().position(|line| line.class == DiffLineClass::FileHeader) {
        if capacity >= 5 {
            result.extend(lines[..file].iter().take(1).map(row));
        }
        result.push(row(&lines[file]));
        if let Some((hunk, rest)) = lines[file + 1..].split_first()
            && hunk.class == DiffLineClass::HunkHeader
        {
            if capacity >= 4 {
                result.push(row(hunk));
            }
            let body = rest.iter().take_while(|line| matches!(line.class,
                DiffLineClass::Insert | DiffLineClass::Delete | DiffLineClass::Context));
            let change = body.clone().position(|line| line.class != DiffLineClass::Context).unwrap_or(0);
            result.extend(body.skip(change).take(capacity - result.len()).map(row));
        }
    } else {
        result.extend(lines.iter().take(capacity).map(row));
    }
    if budget >= 4 {
        result.push(PanelLine { text: "…".into(), tone: LineTone::Dim });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::diff::build_diff_preview;
    use kaijutsu_types::ContentType;

    #[test]
    fn rows_keep_file_changes_and_omission_with_small_budget() {
        let diff = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n same\n";
        let preview = build_diff_preview(diff, ContentType::Diff);
        let lines = rows(&preview, 80, 4);
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().any(|line| line.text.contains("src/a.rs")));
        assert!(lines.iter().any(|line| line.tone == LineTone::Diff(DiffLineClass::Delete) && line.text == "-old"));
        assert!(lines.iter().any(|line| line.tone == LineTone::Diff(DiffLineClass::Insert) && line.text == "+new"));
        assert!(lines.iter().any(|line| line.text.contains('…')));
    }

    #[test]
    fn compact_rows_do_not_mix_files_or_hunks() {
        let diff = "--- a/a\n+++ b/a\n@@ -1 +0,0 @@\n-first\n@@ -8 +8 @@\n-second\n+two\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-third\n+three\n";
        let preview = build_diff_preview(diff, ContentType::Diff);
        let compact = rows(&preview, 80, 6);
        assert!(compact.iter().any(|line| line.text == "-first"));
        assert!(!compact.iter().any(|line| line.text == "-second" || line.text == "+two" || line.text == "-third" || line.text == "+three"));
        assert!(compact.iter().any(|line| line.text.contains('…')));
        let full = rows(&preview, 80, 100);
        assert_eq!(full.len(), preview.lines.len());
        assert!(full.iter().any(|line| line.text == "+three"));
    }

    #[test]
    fn compact_excerpt_does_not_join_changes_separated_by_context() {
        let diff = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n-removed\n same\n context\n+later";
        let preview = build_diff_preview(diff, ContentType::Diff);
        let compact = rows(&preview, 80, 4);
        assert!(compact.iter().any(|line| line.text == "-removed"));
        assert!(!compact.iter().any(|line| line.text == "+later"));
    }

    #[test]
    fn rows_obey_budget_and_width() {
        let preview = build_diff_preview("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-long old line\n+long new line\n", ContentType::Diff);
        assert!(rows(&preview, 10, 0).is_empty());
        for budget in 1..8 {
            let lines = rows(&preview, 10, budget);
            assert!(lines.len() <= budget);
            assert!(lines.iter().all(|line| line.text.chars().count() <= 10));
        }
    }
}
