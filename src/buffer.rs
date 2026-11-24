use ropey::Rope;
use std::collections::HashMap;
use tree_sitter::Point;

pub struct EditDelta {
    pub lines: usize,
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: Point,
    pub old_end_point: Point,
    pub new_end_point: Point,
}

pub struct BufferStore {
    files: HashMap<String, Rope>,
}

impl BufferStore {
    pub fn new() -> Self {
        Self { files: HashMap::new() }
    }

    pub fn open(&mut self, uri: String, text: &str) {
        self.files.insert(uri, Rope::from_str(text));
    }

    pub fn edit(&mut self, uri: &str, start: usize, end: usize, new_text: &str) -> Result<EditDelta, String> {
        let rope = self.files.get_mut(uri).ok_or_else(|| "File not open".to_string())?;
        if start > end || end > rope.len_chars() {
            return Err("Invalid edit range".into());
        }

        let start_byte = rope.char_to_byte(start);
        let old_end_byte = rope.char_to_byte(end);
        let start_point = char_to_point(rope, start);
        let old_end_point = char_to_point(rope, end);

        rope.remove(start..end);
        rope.insert(start, new_text);

        let new_end_char = start + new_text.chars().count();
        let new_end_byte = rope.char_to_byte(new_end_char);
        let new_end_point = char_to_point(rope, new_end_char);

        Ok(EditDelta {
            lines: rope.len_lines(),
            start_byte,
            old_end_byte,
            new_end_byte,
            start_point,
            old_end_point,
            new_end_point,
        })
    }

    pub fn stats(&self, uri: &str) -> Option<(usize, usize)> {
        self.files.get(uri).map(|r| (r.len_lines(), r.len_chars()))
    }

    pub fn get_text(&self, uri: &str) -> Option<String> {
        self.files.get(uri).map(|r| r.to_string())
    }
}

fn char_to_point(rope: &Rope, char_idx: usize) -> Point {
    let row = rope.char_to_line(char_idx);
    let line_start_byte = rope.line_to_byte(row);
    let char_byte = rope.char_to_byte(char_idx);
    Point::new(row, char_byte - line_start_byte)
}
