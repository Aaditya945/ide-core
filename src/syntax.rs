use crate::buffer::EditDelta;
use serde::Serialize;
use std::collections::HashMap;
use tree_sitter::{InputEdit, Language, Node, Parser, Tree};

#[derive(Serialize, Debug)]
pub struct SyntaxError {
    pub source: &'static str,
    pub severity: &'static str,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub snippet: String,
    pub context: String,
    pub message: String,
}

#[derive(Serialize, Debug)]
pub struct SymbolItem {
    pub name: String,
    pub kind: String,
    pub line: usize,
}

#[derive(Serialize, Debug)]
pub struct AnalysisResult {
    pub language: String,
    pub incremental: bool,
    pub errors: Vec<SyntaxError>,
    pub symbols: Vec<SymbolItem>,
}

pub struct SyntaxEngine {
    trees: HashMap<String, Tree>,
}

impl SyntaxEngine {
    pub fn new() -> Self {
        Self { trees: HashMap::new() }
    }

    pub fn invalidate(&mut self, uri: &str) {
        self.trees.remove(uri);
    }

    pub fn apply_edit(&mut self, uri: &str, delta: &EditDelta) {
        if let Some(tree) = self.trees.get_mut(uri) {
            tree.edit(&InputEdit {
                start_byte: delta.start_byte,
                old_end_byte: delta.old_end_byte,
                new_end_byte: delta.new_end_byte,
                start_position: delta.start_point,
                old_end_position: delta.old_end_point,
                new_end_position: delta.new_end_point,
            });
        }
    }

    pub fn detect_language(uri: &str) -> Option<&'static str> {
        if uri.ends_with(".py") { Some("python") }
        else if uri.ends_with(".js") || uri.ends_with(".jsx") { Some("javascript") }
        else if uri.ends_with(".ts") { Some("typescript") }
        else if uri.ends_with(".tsx") { Some("tsx") }
        else if uri.ends_with(".java") { Some("java") }
        else if uri.ends_with(".c") || uri.ends_with(".h") { Some("c") }
        else if uri.ends_with(".cpp") || uri.ends_with(".cc") || uri.ends_with(".hpp") { Some("cpp") }
        else if uri.ends_with(".cs") { Some("csharp") }
        else if uri.ends_with(".rs") { Some("rust") }
        else if uri.ends_with(".go") { Some("go") }
        else if uri.ends_with(".sh") || uri.ends_with(".bash") { Some("bash") }
        else { None }
    }

    fn get_ts_language(lang: &str) -> Language {
        match lang {
            "python" => tree_sitter_python::LANGUAGE.into(),
            "javascript" => tree_sitter_javascript::LANGUAGE.into(),
            "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
            "java" => tree_sitter_java::LANGUAGE.into(),
            "c" => tree_sitter_c::LANGUAGE.into(),
            "cpp" => tree_sitter_cpp::LANGUAGE.into(),
            "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
            "rust" => tree_sitter_rust::LANGUAGE.into(),
            "go" => tree_sitter_go::LANGUAGE.into(),
            "bash" => tree_sitter_bash::LANGUAGE.into(),
            _ => unreachable!(),
        }
    }

    pub fn analyze(&mut self, uri: &str, source: &str) -> Result<AnalysisResult, String> {
        let lang_name = Self::detect_language(uri)
            .ok_or_else(|| format!("Unsupported extension for {uri}"))?;

        let mut parser = Parser::new();
        parser
            .set_language(&Self::get_ts_language(lang_name))
            .map_err(|e| e.to_string())?;

        let old_tree = self.trees.get(uri);
        let incremental = old_tree.is_some();
        let tree = parser
            .parse(source, old_tree)
            .ok_or_else(|| "Failed to parse source".to_string())?;

        let mut errors = check_delimiters_and_indent(lang_name, source);
        let mut symbols = Vec::new();
        collect_nodes(tree.root_node(), source, &mut errors, &mut symbols);
        self.trees.insert(uri.to_string(), tree);

        Ok(AnalysisResult {
            language: lang_name.to_string(),
            incremental,
            errors,
            symbols,
        })
    }
}

fn check_delimiters_and_indent(lang: &str, source: &str) -> Vec<SyntaxError> {
    let mut errors = Vec::new();
    let hash_comments = matches!(lang, "python" | "bash");
    let slash_comments = !hash_comments;

    let mut stack: Vec<(char, usize, usize)> = Vec::new();
    let mut chars = source.chars().peekable();
    let mut line: usize = 1;
    let mut col: usize = 1;

    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_string: Option<(char, usize, usize)> = None;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if ch == '\n' {
            if let Some((q, s_line, s_col)) = in_string {
                if q != '`' {
                    errors.push(SyntaxError {
                        source: "delimiter",
                        severity: "error",
                        line: s_line,
                        column: s_col,
                        end_line: line,
                        end_column: col,
                        snippet: q.to_string(),
                        context: "string literal".into(),
                        message: format!("Unterminated string literal started at line {s_line}, col {s_col}"),
                    });
                    in_string = None;
                    escaped = false;
                }
            }
            in_line_comment = false;
            line += 1;
            col = 1;
            continue;
        }

        if in_line_comment {
            col += 1;
            continue;
        }

        if in_block_comment {
            if ch == '*' && chars.peek() == Some(&'/') {
                chars.next();
                col += 2;
                in_block_comment = false;
                continue;
            }
            col += 1;
            continue;
        }

        if let Some((q, _, _)) = in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                in_string = None;
            }
            col += 1;
            continue;
        }

        if hash_comments && ch == '#' {
            in_line_comment = true;
            col += 1;
            continue;
        }

        if slash_comments && ch == '/' {
            if chars.peek() == Some(&'/') {
                chars.next();
                in_line_comment = true;
                col += 2;
                continue;
            } else if chars.peek() == Some(&'*') {
                chars.next();
                in_block_comment = true;
                col += 2;
                continue;
            }
        }

        if ch == '"' || ch == '\'' || (ch == '`' && matches!(lang, "javascript" | "typescript" | "tsx" | "go" | "bash")) {
            in_string = Some((ch, line, col));
            col += 1;
            continue;
        }

        match ch {
            '{' | '(' | '[' => stack.push((ch, line, col)),
            '}' | ')' | ']' => {
                let expected_open = match ch {
                    '}' => '{',
                    ')' => '(',
                    ']' => '[',
                    _ => unreachable!(),
                };
                match stack.pop() {
                    Some((open_ch, _o_line, _o_col)) if open_ch == expected_open => {}
                    Some((open_ch, o_line, o_col)) => {
                        let expected_close = match open_ch {
                            '{' => '}',
                            '(' => ')',
                            '[' => ']',
                            _ => '?',
                        };
                        errors.push(SyntaxError {
                            source: "delimiter",
                            severity: "error",
                            line,
                            column: col,
                            end_line: line,
                            end_column: col + 1,
                            snippet: ch.to_string(),
                            context: "bracket matching".into(),
                            message: format!(
                                "Mismatched '{ch}' - expected '{expected_close}' for '{open_ch}' opened at line {o_line}, col {o_col}"
                            ),
                        });
                    }
                    None => {
                        errors.push(SyntaxError {
                            source: "delimiter",
                            severity: "error",
                            line,
                            column: col,
                            end_line: line,
                            end_column: col + 1,
                            snippet: ch.to_string(),
                            context: "bracket matching".into(),
                            message: format!("Unexpected '{ch}' with no matching '{expected_open}'"),
                        });
                    }
                }
            }
            _ => {}
        }

        col += 1;
    }

    if let Some((q, s_line, s_col)) = in_string {
        errors.push(SyntaxError {
            source: "delimiter",
            severity: "error",
            line: s_line,
            column: s_col,
            end_line: line,
            end_column: col,
            snippet: q.to_string(),
            context: "string literal".into(),
            message: format!("Unterminated string literal started at line {s_line}, col {s_col}"),
        });
    }

    for (open_ch, o_line, o_col) in stack {
        let close_ch = match open_ch {
            '{' => '}',
            '(' => ')',
            '[' => ']',
            _ => '?',
        };
        errors.push(SyntaxError {
            source: "delimiter",
            severity: "error",
            line: o_line,
            column: o_col,
            end_line: o_line,
            end_column: o_col + 1,
            snippet: open_ch.to_string(),
            context: "bracket matching".into(),
            message: format!("Unclosed '{open_ch}' opened at line {o_line}, col {o_col} (missing '{close_ch}')"),
        });
    }

    if lang == "python" {
        for (idx, raw_line) in source.lines().enumerate() {
            let indent: String = raw_line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            if indent.contains(' ') && indent.contains('\t') {
                errors.push(SyntaxError {
                    source: "indent",
                    severity: "error",
                    line: idx + 1,
                    column: 1,
                    end_line: idx + 1,
                    end_column: indent.len() + 1,
                    snippet: String::new(),
                    context: "python indentation".into(),
                    message: "Inconsistent use of tabs and spaces in indentation".into(),
                });
            }
        }
    }

    errors
}

fn friendly_context(kind: &str) -> String {
    kind.replace('_', " ")
}

fn collect_nodes(
    node: Node,
    source: &str,
    errors: &mut Vec<SyntaxError>,
    symbols: &mut Vec<SymbolItem>,
) {
    if node.is_missing() {
        let start = node.start_position();
        let end = node.end_position();
        let context = node
            .parent()
            .map(|p| friendly_context(p.kind()))
            .unwrap_or_else(|| "top-level".to_string());
        let expected = node.kind();
        errors.push(SyntaxError {
            source: "syntax",
            severity: "error",
            line: start.row + 1,
            column: start.column + 1,
            end_line: end.row + 1,
            end_column: end.column + 1,
            snippet: String::new(),
            context: context.clone(),
            message: format!("Missing '{expected}' in {context}"),
        });
        return;
    }

    if node.is_error() {
        let start = node.start_position();
        let end = node.end_position();
        let context = node
            .parent()
            .map(|p| friendly_context(p.kind()))
            .unwrap_or_else(|| "top-level".to_string());
        let raw = node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let snippet: String = raw.chars().take(40).collect();
        let message = if snippet.is_empty() {
            format!("Incomplete syntax in {context}")
        } else {
            format!("Unexpected '{snippet}' in {context}")
        };
        errors.push(SyntaxError {
            source: "syntax",
            severity: "error",
            line: start.row + 1,
            column: start.column + 1,
            end_line: end.row + 1,
            end_column: end.column + 1,
            snippet,
            context,
            message,
        });
        return;
    }

    let kind = node.kind();
    if matches!(
        kind,
        "function_definition"
            | "class_definition"
            | "function_declaration"
            | "class_declaration"
            | "method_declaration"
            | "interface_declaration"
            | "function_item"
            | "struct_item"
            | "enum_item"
            | "impl_item"
            | "type_declaration"
    ) {
        if let Some(name) = extract_symbol_name(node, source) {
            symbols.push(SymbolItem {
                name,
                kind: kind.to_string(),
                line: node.start_position().row + 1,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_nodes(child, source, errors, symbols);
    }
}

fn extract_symbol_name(node: Node, source: &str) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source.as_bytes()).ok().map(str::to_string);
    }
    if let Some(mut decl) = node.child_by_field_name("declarator") {
        while let Some(inner) = decl.child_by_field_name("declarator") {
            decl = inner;
        }
        return decl.utf8_text(source.as_bytes()).ok().map(str::to_string);
    }
    if let Some(ty) = node.child_by_field_name("type") {
        return ty.utf8_text(source.as_bytes()).ok().map(str::to_string);
    }
    None
}
