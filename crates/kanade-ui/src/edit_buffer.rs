// SPDX-License-Identifier: Apache-2.0
// Native UTF-16/marked-text handling adapted from GPUI's input example.
// The same buffer is used by the rendered text field and these contracts.
use std::ops::Range;

pub(crate) struct EditBuffer {
    pub content: String,
    pub selected_range: Range<usize>,
    pub selection_reversed: bool,
    pub marked_range: Option<Range<usize>>,
    pub enabled: bool,
}
impl EditBuffer {
    pub fn new(content: String) -> Self {
        Self {
            content,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            enabled: true,
        }
    }
    pub fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut bytes = 0;
        let mut units = 0;
        for ch in self.content.chars() {
            if units >= offset {
                break;
            }
            units += ch.len_utf16();
            bytes += ch.len_utf8();
        }
        bytes
    }
    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut bytes = 0;
        let mut units = 0;
        for ch in self.content.chars() {
            if bytes >= offset {
                break;
            }
            bytes += ch.len_utf8();
            units += ch.len_utf16();
        }
        units
    }
    pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }
    pub fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let a = self.offset_from_utf16(range.start);
        let b = self.offset_from_utf16(range.end);
        a.min(b)..a.max(b)
    }
    pub fn replace(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        marked: Option<Option<Range<usize>>>,
    ) -> bool {
        let range = range
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        if !self.enabled || self.content.len() - range.len() + text.len() > 4096 {
            return false;
        }
        self.content.replace_range(range.clone(), text);
        let end = range.start + text.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = if marked.is_some() && !text.is_empty() {
            Some(range.start..end)
        } else {
            None
        };
        if let Some(Some(selection)) = marked {
            let start = self.offset_to_utf16(range.start);
            let length = text.encode_utf16().count();
            self.selected_range = self.range_from_utf16(
                &(start + selection.start.min(length)..start + selection.end.min(length)),
            );
        }
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_marked_text_is_replaced_once_when_committed() {
        let mut b = EditBuffer::new(String::new());
        assert!(b.replace(None, "か", Some(None)));
        assert!(b.replace(None, "かな", Some(Some(2..2))));
        assert_eq!(b.content, "かな");
        assert_eq!(b.marked_range, Some(0..6));
        assert!(b.replace(None, "仮名", None));
        assert_eq!(b.content, "仮名");
        assert_eq!(b.marked_range, None);
        assert_eq!(b.range_to_utf16(&b.selected_range), 2..2);
    }
    #[test]
    fn utf16_replacement_preserves_adjacent_supplementary_and_combining_characters() {
        let mut b = EditBuffer::new("A🙂e\u{301}Z".into());
        assert!(b.replace(Some(1..3), "奏", None));
        assert_eq!(b.content, "A奏e\u{301}Z");
        assert!(b.replace(Some(2..4), "é", None));
        assert_eq!(b.content, "A奏éZ");
        assert_eq!(b.offset_to_utf16(b.content.len()), 4);
    }
    #[test]
    fn marked_selection_is_relative_and_empty_marking_cancels_only_the_mark() {
        let mut b = EditBuffer::new("A🙂Z".into());
        assert!(b.replace(Some(1..3), "é🙂", Some(Some(1..3))));
        assert_eq!(b.content, "Aé🙂Z");
        assert_eq!(b.range_to_utf16(&b.selected_range), 2..4);
        assert!(b.replace(None, "", Some(None)));
        assert_eq!(b.content, "AZ");
        assert_eq!(b.marked_range, None);
    }
    #[test]
    fn sent_draft_is_read_only_and_oversize_insertion_keeps_the_original_selection() {
        let mut b = EditBuffer::new("仮名".into());
        b.selected_range = 0..6;
        assert!(!b.replace(None, &"a".repeat(4097), None));
        assert_eq!(b.content, "仮名");
        assert_eq!(b.selected_range, 0..6);
        b.enabled = false;
        assert!(!b.replace(None, "別の単語", None));
        assert!(!b.replace(None, "かな", Some(None)));
        assert_eq!(b.content, "仮名");
        assert_eq!(b.marked_range, None);
    }
    #[test]
    fn invalid_utf16_offsets_are_bounded_and_never_split_utf8() {
        let mut b = EditBuffer::new("A🙂Z".into());
        assert_eq!(b.range_from_utf16(&(2..99)), 5..6);
        assert!(b.replace(Some(Range { start: 99, end: 1 }), "奏", None));
        assert_eq!(b.content, "A奏");
    }
}
