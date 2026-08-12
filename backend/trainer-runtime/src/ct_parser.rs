use crate::cetrainer;
use crate::ct_types::*;
use anyhow::{bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::io::BufRead;

fn read_text<R: BufRead>(reader: &mut Reader<R>, tag: quick_xml::name::QName) -> Result<String> {
    let mut buf = Vec::new();
    let mut text = String::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Text(e) => {
                text.push_str(&e.unescape()?);
            }
            Event::CData(e) => {
                text.push_str(std::str::from_utf8(e.as_ref()).unwrap_or(""));
            }
            Event::End(e) if e.name() == tag => {
                break;
            }
            Event::Eof => bail!("Unexpected EOF while reading text for tag {:?}", tag),
            _ => {}
        }
        buf.clear();
    }
    Ok(text)
}

pub fn parse_file(path: &str) -> Result<CheatTable> {
    let content = std::fs::read(path).with_context(|| format!("Failed to read file: {}", path))?;
    parse_bytes(&content)
}

pub fn parse_bytes(content: &[u8]) -> Result<CheatTable> {
    // Pre-XML 5.6 tables are a binary record stream. The magic string is
    // not reliable (rebranded forks use their own), so detection is based on
    // the version/record-count header - see ct_legacy.
    if crate::ct_legacy::looks_like_legacy(content) {
        return crate::ct_legacy::parse(content);
    }

    let xml_bytes = if content.starts_with(b"<?xml") || content.starts_with(b"<?XML") || content.starts_with(b"<CheatTable") {
        content.to_vec()
    } else if let Ok(unprotected) = cetrainer::unprotect(content) {
        unprotected
    } else {
        content.to_vec()
    };

    parse_xml(&xml_bytes)
}

pub fn parse_xml(xml_bytes: &[u8]) -> Result<CheatTable> {
    // Community tables are not reliably UTF-8; plenty carry Windows-1252
    // accented characters in descriptions. They read fine elsewhere, so decode
    // leniently rather than rejecting the whole table.
    let decoded = crate::ct_legacy::decode_ansi(xml_bytes);
    let xml_str = decoded.as_str();

    let mut table = CheatTable::default();

    // Parse CheatEngineTableVersion attribute
    if let Some(pos) = xml_str.find("CheatEngineTableVersion=\"") {
        let rest = &xml_str[pos + 25..];
        if let Some(end) = rest.find('"') {
            if let Ok(ver) = rest[..end].parse::<u32>() {
                table.version = ver;
            }
        }
    }

    let mut reader = Reader::from_reader(xml_bytes);
    reader.config_mut().trim_text(false);

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name_bytes = e.name();
                let name = std::str::from_utf8(name_bytes.as_ref()).unwrap_or("");
                match name {
                    "CheatEntries" => {
                        let entries = parse_cheat_entries(&mut reader)?;
                        table.entries.extend(entries);
                    }
                    "LuaScript" => {
                        let lua = parse_lua_script(&mut reader)?;
                        table.lua_script = Some(lua);
                    }
                    "Comments" => {
                        let text = read_text(&mut reader, e.name())?;
                        table.comments = Some(text);
                    }
                    "Forms" => {
                        let blob = read_raw_xml_block(&mut reader, "Forms")?;
                        table.forms.push(RawXmlBlob { tag: "Forms".to_string(), xml: blob });
                    }
                    "Files" => {
                        let files = parse_files(&mut reader)?;
                        table.files = files;
                    }
                    "CheatCodes" => {
                        let codes = parse_cheat_codes(&mut reader)?;
                        table.cheat_codes = codes;
                    }
                    "UserdefinedSymbols" => {
                        let symbols = parse_symbols(&mut reader)?;
                        table.symbols = symbols;
                    }
                    "Structures" => {
                        let blob = read_raw_xml_block(&mut reader, "Structures")?;
                        table.structures.push(RawXmlBlob { tag: "Structures".to_string(), xml: blob });
                    }
                    "DisassemblerComments" => {
                        let blob = read_raw_xml_block(&mut reader, "DisassemblerComments")?;
                        table.disasm_comments.push(RawXmlBlob { tag: "DisassemblerComments".to_string(), xml: blob });
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("XML syntax error: {}", e),
            _ => {}
        }
        buf.clear();
    }

    Ok(table)
}

fn parse_cheat_entries<R: BufRead>(reader: &mut Reader<R>) -> Result<Vec<CheatEntry>> {
    let mut entries = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"CheatEntry" => {
                let entry = parse_single_cheat_entry(reader)?;
                entries.push(entry);
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"CheatEntries" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(entries)
}

/// Reads a `<Options .../>` element's attributes onto the entry.
///
/// Split out because that element arrives as **two different quick-xml events**.
/// It is written self-closing - an `Options` node
/// carrying only attributes, and every one of the 226 in the RDR2 table is
/// `<Options ... />` - which quick-xml reports as `Event::Empty`, never
/// `Event::Start`. Handling only the latter silently dropped every record option
/// in every real table: `moActivateChildrenAsWell` parsed as `false`, so the
/// cascade in `MemoryRecord::set_active` was unreachable code and a group that
/// should bring its whole subtree up did nothing at all. Nothing errored, which
/// is exactly the silent-success failure this project keeps paying for.
fn apply_options(entry: &mut CheatEntry, e: &quick_xml::events::BytesStart) {
    for attr in e.attributes().flatten() {
        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
        let val = std::str::from_utf8(&attr.value).unwrap_or("");
        match key {
            "moHideChildren" => entry.options.hide_children = val == "1",
            "moActivateChildrenAsWell" => entry.options.activate_children_as_well = val == "1",
            "moDeactivateChildrenAsWell" => entry.options.deactivate_children_as_well = val == "1",
            "moRecursiveSetValue" => entry.options.recursive_set_value = val == "1",
            "moAllowManualCollapseAndExpand" => entry.options.allow_manual_collapse_expand = val == "1",
            "moManualExpandCollapse" => entry.options.manual_expand_collapse = val == "1",
            "moAlwaysHideChildren" => entry.options.always_hide_children = val == "1",
            _ => {}
        }
    }
}

fn parse_single_cheat_entry<R: BufRead>(reader: &mut Reader<R>) -> Result<CheatEntry> {
    let mut entry = CheatEntry::default();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = std::str::from_utf8(e.name().as_ref()).unwrap_or("").to_string();
                match name.as_str() {
                    "ID" => {
                        let txt = read_text(reader, e.name())?;
                        entry.id = txt.parse().unwrap_or(0);
                    }
                    "Description" => {
                        let txt = read_text(reader, e.name())?;
                        entry.description = txt.trim_matches('"').to_string();
                    }
                    "VariableType" => {
                        let txt = read_text(reader, e.name())?;
                        entry.var_type = VariableType::from_str(&txt);
                    }
                    "Address" => {
                        let txt = read_text(reader, e.name())?;
                        entry.address = Some(txt);
                    }
                    "Offsets" => {
                        entry.offsets = parse_offsets(reader)?;
                    }
                    "AssemblerScript" => {
                        let txt = read_text(reader, e.name())?;
                        entry.assembler_script = Some(txt);
                    }
                    "ShowAsHex" => {
                        let txt = read_text(reader, e.name())?;
                        entry.show_as_hex = txt == "1";
                    }
                    "ShowAsSigned" => {
                        let txt = read_text(reader, e.name())?;
                        entry.show_as_signed = txt == "1";
                    }
                    "Color" => {
                        let txt = read_text(reader, e.name())?;
                        entry.color = u32::from_str_radix(&txt, 16).ok();
                    }
                    "GroupHeader" => {
                        let txt = read_text(reader, e.name())?;
                        entry.is_group_header = txt == "1";
                    }
                    "Options" => apply_options(&mut entry, &e),
                    "Hotkeys" => {
                        entry.hotkeys = parse_hotkeys(reader)?;
                    }
                    "DropDownList" => {
                        entry.drop_down = Some(parse_drop_down(reader, &e)?);
                    }
                    "CheatEntries" => {
                        entry.children = parse_cheat_entries(reader)?;
                    }
                    _ => {
                        let _ = read_text(reader, e.name());
                    }
                }
            }
            // Self-closing elements. `<Options/>` is the one that matters and
            // the reason this arm exists at all.
            Ok(Event::Empty(e)) => {
                if e.name().as_ref() == b"Options" {
                    apply_options(&mut entry, &e);
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"CheatEntry" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(entry)
}

fn parse_offsets<R: BufRead>(reader: &mut Reader<R>) -> Result<Vec<String>> {
    let mut offsets = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"Offset" => {
                let txt = read_text(reader, e.name())?;
                offsets.push(txt);
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"Offsets" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(offsets)
}

fn parse_hotkeys<R: BufRead>(reader: &mut Reader<R>) -> Result<Vec<Hotkey>> {
    let mut hotkeys = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"Hotkey" => {
                let hk = parse_single_hotkey(reader)?;
                hotkeys.push(hk);
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"Hotkeys" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(hotkeys)
}

fn parse_single_hotkey<R: BufRead>(reader: &mut Reader<R>) -> Result<Hotkey> {
    let mut hk = Hotkey::default();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("");
                match name {
                    "ID" => {
                        let txt = read_text(reader, e.name())?;
                        hk.id = txt.parse().unwrap_or(0);
                    }
                    "Action" => {
                        hk.action = read_text(reader, e.name())?;
                    }
                    "Description" => {
                        hk.description = read_text(reader, e.name())?;
                    }
                    "Value" => {
                        hk.value = Some(read_text(reader, e.name())?);
                    }
                    "Keys" => {
                        hk.keys = parse_hotkey_keys(reader)?;
                    }
                    _ => {
                        let _ = read_text(reader, e.name());
                    }
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"Hotkey" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(hk)
}

fn parse_hotkey_keys<R: BufRead>(reader: &mut Reader<R>) -> Result<Vec<u32>> {
    let mut keys = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"Key" => {
                let txt = read_text(reader, e.name())?;
                if let Ok(k) = txt.parse::<u32>() {
                    keys.push(k);
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"Keys" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(keys)
}

fn parse_drop_down<R: BufRead>(reader: &mut Reader<R>, start_elem: &quick_xml::events::BytesStart) -> Result<DropDownConfig> {
    let mut config = DropDownConfig::default();
    for attr in start_elem.attributes().flatten() {
        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
        let val = std::str::from_utf8(&attr.value).unwrap_or("");
        match key {
            "ReadOnly" => config.read_only = val == "1",
            "DescriptionOnly" => config.description_only = val == "1",
            "DisplayValueAsItem" | "DisplayAsDropDownListItem" => config.display_value_as_item = val == "1",
            _ => {}
        }
    }

    let text = read_text(reader, quick_xml::name::QName(b"DropDownList"))?;
    config.items = text.lines().map(|s| s.to_string()).collect();

    Ok(config)
}

fn parse_lua_script<R: BufRead>(reader: &mut Reader<R>) -> Result<LuaScript> {
    let mut tabs = Vec::new();
    let mut main_script = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"LuaScriptEntry" => {
                let mut tab_name = String::new();
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"Name" {
                        tab_name = std::str::from_utf8(&attr.value).unwrap_or("").to_string();
                    }
                }
                let script = read_text(reader, e.name())?;
                tabs.push((tab_name, script));
            }
            Ok(Event::Text(t)) => {
                main_script.push_str(&t.unescape()?.to_string());
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"LuaScript" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    if !tabs.is_empty() {
        Ok(LuaScript::MultiTab(tabs))
    } else {
        Ok(LuaScript::Single(main_script.trim().to_string()))
    }
}

fn parse_files<R: BufRead>(reader: &mut Reader<R>) -> Result<Vec<EmbeddedFile>> {
    let mut files = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let filename = std::str::from_utf8(e.name().as_ref()).unwrap_or("").to_string();
                let mut encoding = "Ascii85".to_string();
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"Encoding" {
                        encoding = std::str::from_utf8(&attr.value).unwrap_or("").to_string();
                    }
                }
                let data = read_text(reader, e.name())?;
                files.push(EmbeddedFile { filename, encoding, data });
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"Files" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(files)
}

fn parse_cheat_codes<R: BufRead>(reader: &mut Reader<R>) -> Result<Vec<CodeEntry>> {
    let mut codes = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"CodeEntry" => {
                let mut desc = String::new();
                let mut addr = String::new();
                let mut color = None;
                let mut is_header = false;

                for attr in e.attributes().flatten() {
                    let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                    let val = std::str::from_utf8(&attr.value).unwrap_or("");
                    if key == "Color" {
                        color = u32::from_str_radix(val, 16).ok();
                    } else if key == "GroupHeader" {
                        is_header = val == "1";
                    }
                }

                let mut sub_buf = Vec::new();
                loop {
                    match reader.read_event_into(&mut sub_buf) {
                        Ok(Event::Start(se)) => match se.name().as_ref() {
                            b"Description" => desc = read_text(reader, se.name())?,
                            b"AddressString" => addr = read_text(reader, se.name())?,
                            _ => {
                                let _ = read_text(reader, se.name());
                            }
                        },
                        Ok(Event::End(se)) if se.name().as_ref() == b"CodeEntry" => break,
                        Ok(Event::Eof) => break,
                        _ => {}
                    }
                    sub_buf.clear();
                }

                codes.push(CodeEntry {
                    description: desc,
                    address_string: addr,
                    color,
                    is_group_header: is_header,
                    before: Vec::new(),
                    actual: Vec::new(),
                    after: Vec::new(),
                });
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"CheatCodes" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(codes)
}

fn parse_symbols<R: BufRead>(reader: &mut Reader<R>) -> Result<Vec<SymbolEntry>> {
    let mut symbols = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"SymbolEntry" => {
                let mut name = String::new();
                let mut address = String::new();
                let mut sub_buf = Vec::new();

                loop {
                    match reader.read_event_into(&mut sub_buf) {
                        Ok(Event::Start(se)) => match se.name().as_ref() {
                            b"Name" => name = read_text(reader, se.name())?,
                            b"Address" => address = read_text(reader, se.name())?,
                            _ => {
                                let _ = read_text(reader, se.name());
                            }
                        },
                        Ok(Event::End(se)) if se.name().as_ref() == b"SymbolEntry" => break,
                        Ok(Event::Eof) => break,
                        _ => {}
                    }
                    sub_buf.clear();
                }

                symbols.push(SymbolEntry { name, address });
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"UserdefinedSymbols" => break,
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(symbols)
}

fn read_raw_xml_block<R: BufRead>(reader: &mut Reader<R>, tag: &str) -> Result<String> {
    let mut block = format!("<{}>", tag);
    let mut depth = 1;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("");
                if name == tag {
                    depth += 1;
                }
                block.push_str(&format!("<{}>", name));
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("");
                block.push_str(&format!("</{}>", name));
                if name == tag {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            Ok(Event::Text(t)) => {
                block.push_str(&t.unescape()?.to_string());
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(block)
}

pub fn to_xml(table: &CheatTable) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    xml.push_str(&format!("<CheatTable CheatEngineTableVersion=\"{}\">\n", table.version));

    // Forms
    for form in &table.forms {
        xml.push_str(&form.xml);
        xml.push('\n');
    }

    // Files
    if !table.files.is_empty() {
        xml.push_str("  <Files>\n");
        for file in &table.files {
            xml.push_str(&format!("    <{} Encoding=\"{}\">{}</{}>\n", file.filename, file.encoding, file.data, file.filename));
        }
        xml.push_str("  </Files>\n");
    }

    // CheatEntries
    xml.push_str("  <CheatEntries>\n");
    for entry in &table.entries {
        write_cheat_entry_xml(&mut xml, entry, 2);
    }
    xml.push_str("  </CheatEntries>\n");

    // Symbols
    if !table.symbols.is_empty() {
        xml.push_str("  <UserdefinedSymbols>\n");
        for sym in &table.symbols {
            xml.push_str("    <SymbolEntry>\n");
            xml.push_str(&format!("      <Name>{}</Name>\n", sym.name));
            xml.push_str(&format!("      <Address>{}</Address>\n", sym.address));
            xml.push_str("    </SymbolEntry>\n");
        }
        xml.push_str("  </UserdefinedSymbols>\n");
    }

    // Comments
    if let Some(ref comments) = table.comments {
        xml.push_str(&format!("  <Comments>{}</Comments>\n", comments));
    }

    // LuaScript
    if let Some(ref lua) = table.lua_script {
        xml.push_str("  <LuaScript>\n");
        match lua {
            LuaScript::Single(script) => {
                xml.push_str(script);
                xml.push('\n');
            }
            LuaScript::MultiTab(tabs) => {
                for (name, script) in tabs {
                    xml.push_str(&format!("    <LuaScriptEntry Name=\"{}\">{}</LuaScriptEntry>\n", name, script));
                }
            }
        }
        xml.push_str("  </LuaScript>\n");
    }

    xml.push_str("</CheatTable>\n");
    xml
}

fn write_cheat_entry_xml(xml: &mut String, entry: &CheatEntry, indent_level: usize) {
    let pad = " ".repeat(indent_level * 2);
    xml.push_str(&format!("{}<CheatEntry>\n", pad));
    xml.push_str(&format!("{}  <ID>{}</ID>\n", pad, entry.id));
    xml.push_str(&format!("{}  <Description>\"{}\"</Description>\n", pad, entry.description));

    // Options
    let mut opts = Vec::new();
    if entry.options.hide_children { opts.push("moHideChildren=\"1\""); }
    if entry.options.activate_children_as_well { opts.push("moActivateChildrenAsWell=\"1\""); }
    if entry.options.deactivate_children_as_well { opts.push("moDeactivateChildrenAsWell=\"1\""); }
    if entry.options.recursive_set_value { opts.push("moRecursiveSetValue=\"1\""); }
    if !opts.is_empty() {
        xml.push_str(&format!("{}  <Options {}/>\n", pad, opts.join(" ")));
    }

    if entry.show_as_hex {
        xml.push_str(&format!("{}  <ShowAsHex>1</ShowAsHex>\n", pad));
    }
    if entry.show_as_signed {
        xml.push_str(&format!("{}  <ShowAsSigned>1</ShowAsSigned>\n", pad));
    }
    if let Some(color) = entry.color {
        xml.push_str(&format!("{}  <Color>{:08X}</Color>\n", pad, color));
    }
    if entry.is_group_header {
        xml.push_str(&format!("{}  <GroupHeader>1</GroupHeader>\n", pad));
    }

    xml.push_str(&format!("{}  <VariableType>{}</VariableType>\n", pad, entry.var_type));

    if let Some(ref addr) = entry.address {
        xml.push_str(&format!("{}  <Address>{}</Address>\n", pad, addr));
    }

    if !entry.offsets.is_empty() {
        xml.push_str(&format!("{}  <Offsets>\n", pad));
        for off in &entry.offsets {
            xml.push_str(&format!("{}    <Offset>{}</Offset>\n", pad, off));
        }
        xml.push_str(&format!("{}  </Offsets>\n", pad));
    }

    if let Some(ref script) = entry.assembler_script {
        xml.push_str(&format!("{}  <AssemblerScript>{}</AssemblerScript>\n", pad, script));
    }

    if !entry.children.is_empty() {
        xml.push_str(&format!("{}  <CheatEntries>\n", pad));
        for child in &entry.children {
            write_cheat_entry_xml(xml, child, indent_level + 2);
        }
        xml.push_str(&format!("{}  </CheatEntries>\n", pad));
    }

    xml.push_str(&format!("{}</CheatEntry>\n", pad));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sample_ct() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<CheatTable CheatEngineTableVersion="45">
  <CheatEntries>
    <CheatEntry>
      <ID>1</ID>
      <Description>"God Mode"</Description>
      <VariableType>Auto Assembler Script</VariableType>
      <AssemblerScript>[ENABLE]
nop
[DISABLE]</AssemblerScript>
    </CheatEntry>
  </CheatEntries>
</CheatTable>"#;

        let table = parse_xml(xml.as_bytes()).expect("parse failed");
        assert_eq!(table.version, 45);
        assert_eq!(table.entries.len(), 1);
        assert_eq!(table.entries[0].description, "God Mode");
        assert_eq!(table.entries[0].var_type, VariableType::AutoAssembler);

        let reencoded = to_xml(&table);
        assert!(reencoded.contains("God Mode"));
    }
}
