//! Safe Rust API for compiling and decompiling Neverwinter Nights MDL files.

#![forbid(unsafe_code)]

mod routines;

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
};

use anyhow::{Context, Result, bail};
use nwnrs_types::mdl::{
    BinaryToAsciiOptions, ModelEncoding, compile_ascii_model, detect_model_encoding,
    lower_binary_model_to_ascii_with_options, parse_binary_model_bytes, read_ascii_model,
    restore_compiled_model, write_ascii_model, write_original_binary_model,
};

/// Options controlling ASCII-to-binary compilation.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompileOptions {
    /// Accept harmless syntax emitted by older NWMax/nwnmdlcomp versions.
    pub legacy_compatibility: bool,
}

/// Options controlling binary-to-ASCII decompilation.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecompileOptions {
    /// Embed the original compiled payload in comments for byte-exact restoration.
    pub preserve_compiled_source: bool,
}

/// Summary produced while validating a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    /// Detected source encoding.
    pub encoding: ModelEncoding,
    /// Model name from the parsed or compiled header.
    pub model_name: String,
    /// Number of geometry nodes.
    pub nodes: usize,
    /// Number of animations.
    pub animations: usize,
}

/// Compile an ASCII MDL payload into an NWN:EE binary MDL payload.
///
/// The writer uses explicit little-endian offsets and never maps the file as
/// native Rust structures. `BioWare`'s model, node, and mesh routine tables are
/// restored after semantic compilation because the engine-facing binary
/// format includes those fixed 32-bit values.
///
/// # Errors
///
/// Returns an error for binary input, malformed or unsupported ASCII, engine
/// limit violations, or an internally inconsistent compiled representation.
pub fn compile_bytes(source: &[u8], options: CompileOptions) -> Result<Vec<u8>> {
    if detect_model_encoding(source) == ModelEncoding::Compiled {
        bail!("input is already a compiled binary MDL");
    }

    let normalized;
    let input = if options.legacy_compatibility {
        normalized = normalize_legacy_ascii(source)?;
        normalized.as_slice()
    } else {
        source
    };

    let ascii = read_ascii_model(&mut Cursor::new(input)).context("failed to parse ASCII MDL")?;
    if let Ok(original) = restore_compiled_model(&ascii) {
        return Ok(original.into_bytes());
    }
    let binary = compile_ascii_model(&ascii).context("failed to compile semantic MDL")?;
    let mut output = Vec::new();
    write_original_binary_model(&mut output, &binary)
        .context("failed to serialize compiled MDL")?;

    routines::patch_bioware_routines(&mut output, &binary)
        .context("failed to finalize BioWare routine tables")?;
    parse_binary_model_bytes(&output).context("compiled MDL failed self-validation")?;
    Ok(output)
}

fn normalize_long_node_names(source: Vec<u8>) -> Vec<u8> {
    const NODE_NAME_WIDTH: usize = 32;
    const NODE_NAME_MAX_BYTES: usize = NODE_NAME_WIDTH - 1;
    const ALIAS_SUFFIX_BYTES: usize = 7; // `~` plus six hexadecimal digits.

    let mut used = BTreeSet::new();
    let mut aliases = BTreeMap::new();
    let mut serial = 0usize;

    for line in source.split(|byte| *byte == b'\n' || *byte == b'\r') {
        let mut tokens = ascii_tokens(trim_ascii(line));
        if !tokens
            .next()
            .is_some_and(|token| token.eq_ignore_ascii_case(b"node"))
        {
            continue;
        }
        let _kind = tokens.next();
        let Some(name) = tokens.next() else {
            continue;
        };
        let key = ascii_lowercase(name);
        used.insert(key.clone());
        if name.len() <= NODE_NAME_MAX_BYTES || aliases.contains_key(&key) {
            continue;
        }

        let prefix_len = NODE_NAME_MAX_BYTES - ALIAS_SUFFIX_BYTES;
        loop {
            let suffix = format!("~{serial:06x}");
            let mut alias = name[..prefix_len.min(name.len())].to_vec();
            alias.extend_from_slice(suffix.as_bytes());
            serial += 1;
            let alias_key = ascii_lowercase(&alias);
            if used.insert(alias_key) {
                aliases.insert(key, alias);
                break;
            }
        }
    }

    if aliases.is_empty() {
        return source;
    }

    let mut output = Vec::with_capacity(source.len());
    let mut cursor = 0;
    while cursor < source.len() {
        let line_end = source[cursor..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(source.len(), |offset| cursor + offset);
        replace_node_alias_tokens(&source[cursor..line_end], &aliases, &mut output);
        cursor = line_end;
        while cursor < source.len() && (source[cursor] == b'\r' || source[cursor] == b'\n') {
            output.push(source[cursor]);
            cursor += 1;
        }
    }
    output
}

fn replace_node_alias_tokens(
    line: &[u8],
    aliases: &BTreeMap<Vec<u8>, Vec<u8>>,
    output: &mut Vec<u8>,
) {
    let mut cursor = 0;
    while cursor < line.len() {
        let token_start = cursor;
        while cursor < line.len() && !line[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if token_start < cursor {
            let token = &line[token_start..cursor];
            if let Some(alias) = aliases.get(&ascii_lowercase(token)) {
                output.extend_from_slice(alias);
            } else {
                output.extend_from_slice(token);
            }
        }
        let whitespace_start = cursor;
        while cursor < line.len() && line[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        output.extend_from_slice(&line[whitespace_start..cursor]);
    }
}

fn ascii_lowercase(value: &[u8]) -> Vec<u8> {
    value.iter().map(u8::to_ascii_lowercase).collect()
}

/// Decompile an NWN binary MDL payload into canonical NWN:EE ASCII MDL.
///
/// # Errors
///
/// Returns an error for ASCII input, malformed compiled offsets or payloads,
/// or binary features that cannot be represented safely as canonical ASCII.
pub fn decompile_bytes(source: &[u8], options: DecompileOptions) -> Result<Vec<u8>> {
    if detect_model_encoding(source) == ModelEncoding::Ascii {
        bail!("input is already an ASCII MDL");
    }

    let binary = parse_binary_model_bytes(source).context("failed to parse compiled MDL")?;
    let ascii = lower_binary_model_to_ascii_with_options(
        &binary,
        BinaryToAsciiOptions {
            embed_original_binary: options.preserve_compiled_source,
        },
    )
    .context("failed to lower compiled MDL to ASCII")?;
    let mut output = Vec::new();
    write_ascii_model(&mut output, &ascii).context("failed to write ASCII MDL")?;
    Ok(output)
}

/// Convert a model to the opposite on-disk encoding.
///
/// # Errors
///
/// Returns any parsing, lowering, validation, or serialization error raised by
/// [`compile_bytes`] or [`decompile_bytes`].
pub fn convert_bytes(source: &[u8], preserve_compiled_source: bool) -> Result<Vec<u8>> {
    match detect_model_encoding(source) {
        ModelEncoding::Ascii => compile_bytes(
            source,
            CompileOptions {
                legacy_compatibility: true,
            },
        ),
        ModelEncoding::Compiled => decompile_bytes(
            source,
            DecompileOptions {
                preserve_compiled_source,
            },
        ),
    }
}

/// Parse and deeply validate either ASCII or binary MDL input.
///
/// # Errors
///
/// Returns an error when parsing, semantic lowering, or a trial compilation
/// detects malformed data, unsupported fields, or an NWN:EE engine-limit
/// violation.
pub fn validate_bytes(source: &[u8]) -> Result<ValidationReport> {
    match detect_model_encoding(source) {
        ModelEncoding::Ascii => {
            let compiled = compile_bytes(
                source,
                CompileOptions {
                    legacy_compatibility: true,
                },
            )?;
            let model = parse_binary_model_bytes(&compiled)?;
            Ok(ValidationReport {
                encoding: ModelEncoding::Ascii,
                model_name: model.name().to_owned(),
                nodes: model.nodes().len(),
                animations: model.animations().len(),
            })
        }
        ModelEncoding::Compiled => {
            let model = parse_binary_model_bytes(source)?;
            // Lowering catches semantic inconsistencies that a bounds-safe
            // binary traversal alone cannot identify.
            lower_binary_model_to_ascii_with_options(
                &model,
                BinaryToAsciiOptions {
                    embed_original_binary: false,
                },
            )?;
            Ok(ValidationReport {
                encoding: ModelEncoding::Compiled,
                model_name: model.name().to_owned(),
                nodes: model.nodes().len(),
                animations: model.animations().len(),
            })
        }
    }
}

fn normalize_legacy_ascii(source: &[u8]) -> Result<Vec<u8>> {
    let model_name = source
        .split(|byte| *byte == b'\n' || *byte == b'\r')
        .find_map(|line| {
            let line = trim_ascii(line);
            let rest = strip_prefix_ascii_case(line, b"newmodel")?;
            let value = trim_ascii(rest);
            (!value.is_empty()).then_some(value)
        })
        .context("ASCII MDL has no newmodel declaration")?;

    let mut output = Vec::with_capacity(source.len() + model_name.len());
    let mut cursor = 0;
    let mut remaining_face_rows = 0usize;
    let mut remaining_weight_rows = 0usize;
    let mut remaining_constraint_rows = 0usize;
    while cursor < source.len() {
        let line_end = source[cursor..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(source.len(), |offset| cursor + offset);
        let line = &source[cursor..line_end];
        let trimmed = trim_ascii(line);
        if remaining_constraint_rows > 0 && trimmed.eq_ignore_ascii_case(b"endnode") {
            write_missing_constraint_rows(&mut output, remaining_constraint_rows);
            remaining_constraint_rows = 0;
            output.extend_from_slice(line);
        } else if remaining_constraint_rows > 0 && !trimmed.is_empty() && !trimmed.starts_with(b"#")
        {
            output.extend_from_slice(line);
            remaining_constraint_rows -= 1;
        } else if remaining_weight_rows > 0 && !trimmed.is_empty() && !trimmed.starts_with(b"#") {
            let tokens: Vec<_> = ascii_tokens(trimmed).collect();
            let stray_one = if tokens.len() % 2 == 1 {
                (1..tokens.len().saturating_sub(1)).find(|&index| {
                    tokens[index] == b"1"
                        && !is_ascii_number(tokens[index - 1])
                        && is_ascii_number(tokens[index + 1])
                })
            } else {
                None
            };
            if let Some(stray_one) = stray_one {
                let leading = line.len() - line.trim_ascii_start().len();
                output.extend_from_slice(&line[..leading]);
                let mut first = true;
                for (index, token) in tokens.iter().enumerate() {
                    if index == stray_one {
                        continue;
                    }
                    if !first {
                        output.push(b' ');
                    }
                    output.extend_from_slice(token);
                    first = false;
                }
            } else {
                output.extend_from_slice(line);
            }
            remaining_weight_rows -= 1;
        } else if remaining_face_rows > 0 && !trimmed.is_empty() && !trimmed.starts_with(b"#") {
            output.extend_from_slice(line);
            if ascii_tokens(trimmed).count() == 7 {
                output.extend_from_slice(b" 0");
            }
            remaining_face_rows -= 1;
        } else if is_legacy_timestamp_preamble(trimmed) || is_legacy_maxmodel_preamble(trimmed) {
            output.extend_from_slice(b"# legacy preamble: ");
            output.extend_from_slice(trimmed);
        } else if trimmed.eq_ignore_ascii_case(b"donemodel") {
            let leading = line.len() - line.trim_ascii_start().len();
            output.extend_from_slice(&line[..leading]);
            output.extend_from_slice(b"donemodel ");
            output.extend_from_slice(model_name);
        } else if has_undefined_animation_scale(trimmed) {
            let leading = line.len() - line.trim_ascii_start().len();
            output.extend_from_slice(&line[..leading]);
            output.extend_from_slice(b"setanimationscale 1.0");
        } else {
            output.extend_from_slice(line);
            if let Some(count) = face_row_count(trimmed) {
                remaining_face_rows = count;
            }
            if let Some(count) = weight_row_count(trimmed) {
                remaining_weight_rows = count;
            }
            if let Some(count) = counted_statement_rows(trimmed, b"constraints") {
                remaining_constraint_rows = count;
            }
        }

        cursor = line_end;
        while cursor < source.len() && (source[cursor] == b'\r' || source[cursor] == b'\n') {
            output.push(source[cursor]);
            cursor += 1;
        }
    }
    let output = remove_legacy_control_bytes(&output);
    Ok(normalize_long_node_names(repair_legacy_node_structure(
        &output,
    )))
}

fn write_missing_constraint_rows(output: &mut Vec<u8>, count: usize) {
    for _ in 0..count {
        output.extend_from_slice(b"  0\n");
    }
}

fn has_undefined_animation_scale(line: &[u8]) -> bool {
    statement_starts_with(line, b"setanimationscale")
        && ascii_tokens(line)
            .nth(1)
            .is_some_and(|value| value.eq_ignore_ascii_case(b"undefined"))
}

fn remove_legacy_control_bytes(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(source.len());
    for &byte in source {
        if byte >= 0x20 || matches!(byte, b'\t' | b'\r' | b'\n') {
            output.push(byte);
        }
    }
    output
}

fn repair_legacy_node_structure(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(source.len());
    let mut previous_was_endnode = false;
    let mut inside_node = false;
    let mut skipping_aabb_tree = false;

    for segment in source.split_inclusive(|byte| *byte == b'\n') {
        let line = segment.strip_suffix(b"\n").unwrap_or(segment);
        let trimmed = trim_ascii(line);
        let is_endnode = trimmed.eq_ignore_ascii_case(b"endnode");
        if skipping_aabb_tree {
            if is_endnode {
                output.extend_from_slice(segment);
                skipping_aabb_tree = false;
                inside_node = false;
                previous_was_endnode = true;
            }
            continue;
        }
        if inside_node && statement_starts_with(trimmed, b"aabb") {
            skipping_aabb_tree = true;
            continue;
        }
        if statement_starts_with(trimmed, b"node") {
            if inside_node {
                output.extend_from_slice(b"endnode\n");
            }
            inside_node = true;
        } else if statement_starts_with(trimmed, b"endmodelgeom") && inside_node {
            output.extend_from_slice(b"endnode\n");
            inside_node = false;
        }
        if is_endnode && previous_was_endnode {
            continue;
        }
        output.extend_from_slice(segment);
        if !trimmed.is_empty() && !trimmed.starts_with(b"#") {
            previous_was_endnode = is_endnode;
            if is_endnode {
                inside_node = false;
            }
        }
    }
    output
}

fn statement_starts_with(line: &[u8], keyword: &[u8]) -> bool {
    let mut tokens = ascii_tokens(line);
    tokens
        .next()
        .is_some_and(|token| token.eq_ignore_ascii_case(keyword))
}

fn face_row_count(line: &[u8]) -> Option<usize> {
    let mut tokens = ascii_tokens(line);
    let keyword = tokens.next()?;
    if !keyword.eq_ignore_ascii_case(b"faces") {
        return None;
    }
    let count = std::str::from_utf8(tokens.next()?).ok()?.parse().ok()?;
    tokens.next().is_none().then_some(count)
}

fn weight_row_count(line: &[u8]) -> Option<usize> {
    counted_statement_rows(line, b"weights")
}

fn counted_statement_rows(line: &[u8], expected_keyword: &[u8]) -> Option<usize> {
    let mut tokens = ascii_tokens(line);
    let keyword = tokens.next()?;
    if !keyword.eq_ignore_ascii_case(expected_keyword) {
        return None;
    }
    let count = std::str::from_utf8(tokens.next()?).ok()?.parse().ok()?;
    tokens.next().is_none().then_some(count)
}

fn is_ascii_number(token: &[u8]) -> bool {
    std::str::from_utf8(token)
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .is_some()
}

fn is_legacy_timestamp_preamble(line: &[u8]) -> bool {
    let mut tokens = ascii_tokens(line);
    let Some(date) = tokens.next() else {
        return false;
    };
    let Some(time) = tokens.next() else {
        return false;
    };
    let Some(period) = tokens.next() else {
        return false;
    };
    let mut time_parts = time.split(|byte| *byte == b':');
    let has_three_time_parts = time_parts.next().is_some()
        && time_parts.next().is_some()
        && time_parts.next().is_some()
        && time_parts.next().is_none();
    tokens.next().is_none()
        && date.contains(&b'/')
        && has_three_time_parts
        && (period.eq_ignore_ascii_case(b"AM") || period.eq_ignore_ascii_case(b"PM"))
}

fn is_legacy_maxmodel_preamble(line: &[u8]) -> bool {
    let Some(marker) = line
        .windows(b"#MAXMODEL".len())
        .position(|window| window.eq_ignore_ascii_case(b"#MAXMODEL"))
    else {
        return false;
    };
    marker > 0 && !line[..marker].contains(&b' ') && !line[..marker].contains(&b'\t')
}

fn ascii_tokens(value: &[u8]) -> impl Iterator<Item = &[u8]> {
    value
        .split(u8::is_ascii_whitespace)
        .filter(|token| !token.is_empty())
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    value = value.trim_ascii_start();
    value.trim_ascii_end()
}

fn strip_prefix_ascii_case<'a>(value: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let candidate = value.get(..prefix.len())?;
    candidate
        .eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::{
        CompileOptions, DecompileOptions, compile_bytes, decompile_bytes, parse_binary_model_bytes,
        validate_bytes,
    };

    const SIMPLE_MODEL: &str = "\
#MAXMODEL ASCII
newmodel demo
setsupermodel demo null
classification character
setanimationscale 1
beginmodelgeom demo
node dummy demo
  parent null
endnode
node trimesh body
  parent demo
  bitmap tex01
  verts 3
    0 0 0
    1 0 0
    0 1 0
  tverts 3
    0 0 0
    1 0 0
    0 1 0
  faces 1
    0 1 2 1 0 1 2 0
endnode
endmodelgeom demo
donemodel
";

    #[test]
    fn compiles_legacy_ascii_and_restores_engine_tables() -> anyhow::Result<()> {
        let binary = compile_bytes(
            SIMPLE_MODEL.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;

        assert_eq!(&binary[..4], &[0, 0, 0, 0]);
        assert_eq!(&binary[12..20], &[0x0c, 0xab, 0x46, 0, 0x1c, 0xab, 0x46, 0]);
        // Model data offset 232 is the root dummy node.
        assert_eq!(&binary[244..248], &[0xe0, 0xb9, 0x46, 0]);
        let parsed = parse_binary_model_bytes(&binary)?;
        let mesh = &parsed.nodes()[1];
        let mesh_offset = 12 + usize::try_from(mesh.offset)?;
        assert_eq!(
            &binary[mesh_offset..mesh_offset + 4],
            &[0xb4, 0xe7, 0x46, 0]
        );
        assert_eq!(
            &binary[mesh_offset + 112..mesh_offset + 116],
            &[0x28, 0xe8, 0x46, 0]
        );
        Ok(())
    }

    #[test]
    fn accepts_mesh_texture_names_longer_than_legacy_resrefs() -> anyhow::Result<()> {
        let source = SIMPLE_MODEL.replace("bitmap tex01", "bitmap rivendelleliteswordfighter");
        let binary = compile_bytes(
            source.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        let ascii = decompile_bytes(
            &binary,
            DecompileOptions {
                preserve_compiled_source: false,
            },
        )?;
        assert!(String::from_utf8(ascii)?.contains("bitmap rivendelleliteswordfighter"));
        Ok(())
    }

    #[test]
    fn malformed_inputs_never_panic() -> anyhow::Result<()> {
        let valid_binary = compile_bytes(
            SIMPLE_MODEL.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        let seeds = [SIMPLE_MODEL.as_bytes(), valid_binary.as_slice()];

        for seed in seeds {
            for case in 0..512usize {
                let mut mutated = seed.to_vec();
                let index = case.wrapping_mul(2_654_435_761) % mutated.len();
                mutated[index] ^= case.to_le_bytes()[0].wrapping_mul(73).wrapping_add(1);
                if case % 4 == 0 {
                    mutated.truncate(index);
                }
                let result = std::panic::catch_unwind(|| validate_bytes(&mutated));
                assert!(result.is_ok(), "validation panicked for mutation {case}");
            }
        }
        Ok(())
    }

    #[test]
    fn preserved_compiled_source_restores_byte_exactly() -> anyhow::Result<()> {
        let binary = compile_bytes(
            SIMPLE_MODEL.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        let ascii = decompile_bytes(
            &binary,
            DecompileOptions {
                preserve_compiled_source: true,
            },
        )?;
        let restored = compile_bytes(
            &ascii,
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        assert_eq!(restored, binary);
        Ok(())
    }

    #[test]
    fn legacy_mode_accepts_timestamp_preambles_and_omitted_face_surfaces() -> anyhow::Result<()> {
        let source = format!(
            "1/2005 1:25:06 PM\n{}",
            SIMPLE_MODEL.replace("0 1 2 1 0 1 2 0", "0 1 2 1 0 1 2")
        );
        compile_bytes(
            source.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        assert!(
            compile_bytes(
                source.as_bytes(),
                CompileOptions {
                    legacy_compatibility: false,
                },
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn legacy_mode_accepts_filename_prefixes_glued_to_maxmodel_comments() -> anyhow::Result<()> {
        let source = format!("crs01_floor01#MAXMODEL ASCII\n{SIMPLE_MODEL}");
        compile_bytes(
            source.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        assert!(
            compile_bytes(
                source.as_bytes(),
                CompileOptions {
                    legacy_compatibility: false,
                },
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn legacy_mode_accepts_cross_geometry_supermodels_and_duplicate_endnodes() -> anyhow::Result<()>
    {
        let source = SIMPLE_MODEL
            .replace(
                "setsupermodel demo null",
                "setsupermodel source_model c_sharkmk",
            )
            .replace("endmodelgeom demo", "endnode\nendmodelgeom demo");
        let source = format!("\u{12}{source}");
        compile_bytes(
            source.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        Ok(())
    }

    #[test]
    fn legacy_mode_aliases_overlong_node_names_without_hierarchy_collisions() -> anyhow::Result<()>
    {
        let long_name = "this_node_name_is_longer_than_thirtyone";
        let source = SIMPLE_MODEL
            .replace("node dummy demo", &format!("node dummy {long_name}"))
            .replace("parent demo", &format!("parent {long_name}"));
        let normalized = String::from_utf8(super::normalize_long_node_names(
            source.clone().into_bytes(),
        ))?;
        assert!(normalized.contains("~000000"));
        assert!(!normalized.contains(long_name));
        let binary = compile_bytes(
            source.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        let ascii = String::from_utf8(decompile_bytes(&binary, DecompileOptions::default())?)?;
        assert!(ascii.contains("~000000"));
        assert!(!ascii.contains(long_name));
        assert!(
            compile_bytes(
                source.as_bytes(),
                CompileOptions {
                    legacy_compatibility: false,
                },
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn compile_decompile_roundtrip_is_semantically_valid() -> anyhow::Result<()> {
        let binary = compile_bytes(
            SIMPLE_MODEL.as_bytes(),
            CompileOptions {
                legacy_compatibility: true,
            },
        )?;
        let ascii = decompile_bytes(&binary, DecompileOptions::default())?;
        let report = validate_bytes(&ascii)?;
        assert_eq!(report.model_name, "demo");
        assert_eq!(report.nodes, 2);
        Ok(())
    }
}
