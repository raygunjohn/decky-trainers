use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct CheatTable {
    pub version: u32,
    pub entries: Vec<CheatEntry>,
    pub lua_script: Option<LuaScript>,
    pub forms: Vec<RawXmlBlob>,
    pub files: Vec<EmbeddedFile>,
    pub cheat_codes: Vec<CodeEntry>,
    pub symbols: Vec<SymbolEntry>,
    pub structures: Vec<RawXmlBlob>,
    pub comments: Option<String>,
    pub disasm_comments: Vec<RawXmlBlob>,
}

impl Default for CheatTable {
    fn default() -> Self {
        Self {
            version: 45,
            entries: Vec::new(),
            lua_script: None,
            forms: Vec::new(),
            files: Vec::new(),
            cheat_codes: Vec::new(),
            symbols: Vec::new(),
            structures: Vec::new(),
            comments: None,
            disasm_comments: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CheatEntry {
    pub id: u32,
    pub description: String,
    pub var_type: VariableType,
    pub address: Option<String>,
    pub offsets: Vec<String>,
    pub assembler_script: Option<String>,
    pub options: EntryOptions,
    pub hotkeys: Vec<Hotkey>,
    pub children: Vec<CheatEntry>,
    pub color: Option<u32>,
    pub show_as_hex: bool,
    pub show_as_signed: bool,
    pub is_group_header: bool,
    pub drop_down: Option<DropDownConfig>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum VariableType {
    Byte,
    Word,
    #[default]
    Dword,
    Qword,
    Float,
    Double,
    String,
    UnicodeString,
    ByteArray,
    Binary,
    AutoAssembler,
    Pointer,
    Custom(String),
    Grouped,
}

impl VariableType {
    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "Byte" => VariableType::Byte,
            "2 Bytes" | "Word" => VariableType::Word,
            "4 Bytes" | "Dword" => VariableType::Dword,
            "8 Bytes" | "Qword" => VariableType::Qword,
            "Float" | "Single" => VariableType::Float,
            "Double" => VariableType::Double,
            "String" => VariableType::String,
            "Unicode String" | "UnicodeString" => VariableType::UnicodeString,
            "Array of byte" | "ByteArray" => VariableType::ByteArray,
            "Binary" => VariableType::Binary,
            "Auto Assembler Script" | "AutoAssembler" => VariableType::AutoAssembler,
            "Pointer" => VariableType::Pointer,
            "Grouped" => VariableType::Grouped,
            other => {
                if other.starts_with("Custom:") || other.starts_with("Custom ") {
                    VariableType::Custom(other.trim_start_matches("Custom:").trim_start_matches("Custom ").trim().to_string())
                } else if !other.is_empty() {
                    VariableType::Custom(other.to_string())
                } else {
                    VariableType::Dword
                }
            }
        }
    }

    pub fn to_str(&self) -> String {
        match self {
            VariableType::Byte => "Byte".to_string(),
            VariableType::Word => "2 Bytes".to_string(),
            VariableType::Dword => "4 Bytes".to_string(),
            VariableType::Qword => "8 Bytes".to_string(),
            VariableType::Float => "Float".to_string(),
            VariableType::Double => "Double".to_string(),
            VariableType::String => "String".to_string(),
            VariableType::UnicodeString => "Unicode String".to_string(),
            VariableType::ByteArray => "Array of byte".to_string(),
            VariableType::Binary => "Binary".to_string(),
            VariableType::AutoAssembler => "Auto Assembler Script".to_string(),
            VariableType::Pointer => "Pointer".to_string(),
            VariableType::Grouped => "Grouped".to_string(),
            VariableType::Custom(name) => format!("Custom: {}", name),
        }
    }
}

impl fmt::Display for VariableType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_str())
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EntryOptions {
    pub hide_children: bool,
    pub activate_children_as_well: bool,
    pub deactivate_children_as_well: bool,
    pub recursive_set_value: bool,
    pub allow_manual_collapse_expand: bool,
    pub manual_expand_collapse: bool,
    pub always_hide_children: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hotkey {
    pub id: u32,
    pub description: String,
    pub action: String,
    pub keys: Vec<u32>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DropDownConfig {
    pub read_only: bool,
    pub description_only: bool,
    pub display_value_as_item: bool,
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LuaScript {
    Single(String),
    MultiTab(Vec<(String, String)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedFile {
    pub filename: String,
    pub encoding: String, // e.g. Ascii85
    pub data: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodeEntry {
    pub description: String,
    pub address_string: String,
    pub color: Option<u32>,
    pub is_group_header: bool,
    pub before: Vec<u8>,
    pub actual: Vec<u8>,
    pub after: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolEntry {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawXmlBlob {
    pub tag: String,
    pub xml: String,
}
