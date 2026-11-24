use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Debug)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    #[serde(rename = "buffer/open")]
    OpenBuffer { uri: String, text: String },

    #[serde(rename = "buffer/edit")]
    EditBuffer {
        uri: String,
        start_char: usize,
        end_char: usize,
        new_text: String,
    },

    #[serde(rename = "buffer/info")]
    BufferInfo { uri: String },

    #[serde(rename = "lang/analyze")]
    Analyze { uri: String },

    #[serde(rename = "lang/check")]
    Check { uri: String },

    #[serde(rename = "lang/diagnostics")]
    Diagnostics { uri: String },

    #[serde(rename = "env/check")]
    EnvCheck,

    #[serde(rename = "exec/start")]
    ExecStart { uri: String },

    #[serde(rename = "exec/input")]
    ExecInput { session_id: u64, data: String },

    #[serde(rename = "exec/kill")]
    ExecKill { session_id: u64 },

    #[serde(rename = "lsp/sync")]
    LspSync { uri: String },

    #[serde(rename = "lsp/complete")]
    LspComplete { uri: String, line: u32, character: u32 },

    #[serde(rename = "lsp/definition")]
    LspDefinition { uri: String, line: u32, character: u32 },

    #[serde(rename = "lsp/references")]
    LspReferences { uri: String, line: u32, character: u32 },

    #[serde(rename = "lsp/hover")]
    LspHover { uri: String, line: u32, character: u32 },

    #[serde(rename = "lsp/code_action")]
    LspCodeAction {
        uri: String,
        line: u32,
        character: u32,
        end_line: u32,
        end_character: u32,
    },

    #[serde(rename = "lsp/rename")]
    LspRename {
        uri: String,
        line: u32,
        character: u32,
        new_name: String,
    },

    #[serde(rename = "lsp/format")]
    LspFormat { uri: String },

    #[serde(rename = "lsp/stop")]
    LspStop { language: String },
}

#[derive(Deserialize, Debug)]
pub struct RpcEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub req: Request,
}

#[derive(Serialize, Debug)]
pub struct RpcResponse {
    pub id: u64,
    pub result: Option<Value>,
    pub error: Option<String>,
}
