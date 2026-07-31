#![allow(clippy::type_complexity)]

use anyhow::{Result, anyhow};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;
use tree_sitter::{Node, Parser, Point};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HunkLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug)]
struct HunkLine {
    kind: HunkLineKind,
    text: String,
}

#[derive(Debug)]
struct DiffHunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    old_seen: usize,
    new_seen: usize,
    lines: Vec<HunkLine>,
}

#[derive(Default, Debug)]
struct FileDiff {
    sections: usize,
    hunks: Vec<DiffHunk>,
}

impl FileDiff {
    fn fully_displays(&self, start: usize, end: usize) -> bool {
        let mut ranges: Vec<(usize, usize)> = self
            .hunks
            .iter()
            .filter(|hunk| hunk.new_count > 0)
            .map(|hunk| {
                (
                    hunk.new_start,
                    hunk.new_start + hunk.new_count.saturating_sub(1),
                )
            })
            .collect();
        ranges.sort_unstable();

        let mut next = start;
        for (range_start, range_end) in ranges {
            if range_end < next {
                continue;
            }
            if range_start > next {
                return false;
            }
            if range_end >= end {
                return true;
            }
            next = range_end + 1;
        }
        false
    }

    fn added_lines(&self) -> BTreeSet<usize> {
        let mut added = BTreeSet::new();
        for hunk in &self.hunks {
            let mut new_line = hunk.new_start;
            for line in &hunk.lines {
                match line.kind {
                    HunkLineKind::Context => new_line += 1,
                    HunkLineKind::Added => {
                        added.insert(new_line);
                        new_line += 1;
                    }
                    HunkLineKind::Removed => {}
                }
            }
        }
        added
    }

    fn removed_old_lines(&self) -> BTreeSet<usize> {
        let mut removed = BTreeSet::new();
        for hunk in &self.hunks {
            let mut old_line = hunk.old_start;
            for line in &hunk.lines {
                match line.kind {
                    HunkLineKind::Context => old_line += 1,
                    HunkLineKind::Added => {}
                    HunkLineKind::Removed => {
                        removed.insert(old_line);
                        old_line += 1;
                    }
                }
            }
        }
        removed
    }

    fn unchanged_line_pairs(&self) -> Vec<(usize, usize)> {
        let mut pairs = Vec::new();
        for hunk in &self.hunks {
            let mut old_line = hunk.old_start;
            let mut new_line = hunk.new_start;
            for line in &hunk.lines {
                match line.kind {
                    HunkLineKind::Context => {
                        pairs.push((old_line, new_line));
                        old_line += 1;
                        new_line += 1;
                    }
                    HunkLineKind::Added => new_line += 1,
                    HunkLineKind::Removed => old_line += 1,
                }
            }
        }
        pairs
    }

    fn has_deletion_anchored_in(&self, start: usize, end: usize) -> bool {
        self.hunks.iter().any(|hunk| {
            let mut new_line = hunk.new_start;
            hunk.lines.iter().any(|line| match line.kind {
                HunkLineKind::Context | HunkLineKind::Added => {
                    new_line += 1;
                    false
                }
                HunkLineKind::Removed => new_line >= start && new_line <= end,
            })
        })
    }

    fn reconstruct_preimage(&self, postimage: &str) -> Option<String> {
        let mut lines: Vec<String> = postimage.lines().map(str::to_string).collect();
        let mut hunks: Vec<&DiffHunk> = self.hunks.iter().collect();
        hunks.sort_by_key(|hunk| std::cmp::Reverse(hunk.new_start));

        for hunk in hunks {
            if hunk.old_seen != hunk.old_count || hunk.new_seen != hunk.new_count {
                return None;
            }
            let new_lines: Vec<&str> = hunk
                .lines
                .iter()
                .filter(|line| line.kind != HunkLineKind::Removed)
                .map(|line| line.text.as_str())
                .collect();
            let old_lines: Vec<String> = hunk
                .lines
                .iter()
                .filter(|line| line.kind != HunkLineKind::Added)
                .map(|line| line.text.clone())
                .collect();
            let replace_end = hunk.new_start.checked_add(new_lines.len())?;
            if replace_end > lines.len()
                || !lines[hunk.new_start..replace_end]
                    .iter()
                    .map(String::as_str)
                    .eq(new_lines)
            {
                return None;
            }
            lines.splice(hunk.new_start..replace_end, old_lines);
        }
        Some(lines.join("\n"))
    }
}

fn parse_unified_diff(diff: &str) -> HashMap<String, FileDiff> {
    let header_re = Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap();
    let mut files = HashMap::new();
    let mut current_file: Option<String> = None;
    let mut current_hunk: Option<usize> = None;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            current_file = None;
            current_hunk = None;
            continue;
        }
        if let Some(fname) = line.strip_prefix("+++ b/") {
            let fname = fname.to_string();
            files
                .entry(fname.clone())
                .or_insert_with(FileDiff::default)
                .sections += 1;
            current_file = Some(fname);
            current_hunk = None;
            continue;
        }
        if let Some(caps) = header_re.captures(line)
            && let Some(fname) = &current_file
        {
            let parse = |index: usize, default: usize| {
                caps.get(index)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(default)
            };
            let hunk = DiffHunk {
                old_start: parse(1, 1).saturating_sub(1),
                old_count: parse(2, 1),
                new_start: parse(3, 1).saturating_sub(1),
                new_count: parse(4, 1),
                old_seen: 0,
                new_seen: 0,
                lines: Vec::new(),
            };
            let file = files.get_mut(fname).unwrap();
            file.hunks.push(hunk);
            current_hunk = Some(file.hunks.len() - 1);
            continue;
        }

        let (Some(fname), Some(hunk_index)) = (&current_file, current_hunk) else {
            continue;
        };
        let Some(kind) = line.as_bytes().first().and_then(|prefix| match prefix {
            b' ' => Some(HunkLineKind::Context),
            b'+' => Some(HunkLineKind::Added),
            b'-' => Some(HunkLineKind::Removed),
            _ => None,
        }) else {
            continue;
        };
        let hunk = &mut files.get_mut(fname).unwrap().hunks[hunk_index];
        hunk.lines.push(HunkLine {
            kind,
            text: line.get(1..).unwrap_or_default().to_string(),
        });
        match kind {
            HunkLineKind::Context => {
                hunk.old_seen += 1;
                hunk.new_seen += 1;
            }
            HunkLineKind::Added => hunk.new_seen += 1,
            HunkLineKind::Removed => hunk.old_seen += 1,
        }
        if hunk.old_seen == hunk.old_count && hunk.new_seen == hunk.new_count {
            current_hunk = None;
        }
    }

    files
}

/// Parses a unified diff and returns a map of filename -> list of modified line ranges.
/// Line numbers are 0-based to align with Tree-sitter's Point API.
pub fn parse_diff_ranges(diff: &str) -> HashMap<String, Vec<(usize, usize)>> {
    let parsed = parse_unified_diff(diff);
    let mut files: HashMap<String, Vec<(usize, usize)>> = parsed
        .into_iter()
        .map(|(file, diff)| {
            let ranges = diff
                .hunks
                .into_iter()
                .filter(|hunk| hunk.new_count > 0)
                .map(|hunk| {
                    (
                        hunk.new_start,
                        hunk.new_start + hunk.new_count.saturating_sub(1),
                    )
                })
                .collect();
            (file, ranges)
        })
        .collect();

    // Merge overlapping/adjacent ranges (within 10 lines)
    for ranges in files.values_mut() {
        ranges.sort_by_key(|r| r.0);
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for r in ranges.iter() {
            if let Some(last) = merged.last_mut() {
                if r.0 <= last.1 + 10 {
                    last.1 = std::cmp::max(last.1, r.1);
                } else {
                    merged.push(*r);
                }
            } else {
                merged.push(*r);
            }
        }
        *ranges = merged;
    }

    files
}

use tokio::process::Command;

const MAX_PREFETCH_CHARS: usize = 200000;

type LineRangeMap = BTreeMap<PathBuf, BTreeSet<(usize, usize)>>;

fn add_range(map: &mut LineRangeMap, path: PathBuf, start: usize, end: usize) {
    map.entry(path).or_default().insert((start, end));
}

pub async fn prefetch_context(worktree_path: &Path, diff: &str) -> Result<String> {
    let parsed_diff = parse_unified_diff(diff);
    let file_ranges = parse_diff_ranges(diff);
    let mut range_map: LineRangeMap = BTreeMap::new();
    let mut symbols_to_lookup = HashSet::new();
    let mut already_extracted = HashSet::new();
    let mut called_functions = HashSet::new();

    // Phase 1: modified code — find enclosing blocks, types, and called functions.
    for (file, ranges) in &file_ranges {
        if !file.ends_with(".c") && !file.ends_with(".h") {
            continue;
        }
        let file_path = worktree_path.join(file);
        if !file_path.exists() {
            continue;
        }

        if let Ok(content) = fs::read_to_string(&file_path).await {
            let diff_file = parsed_diff.get(file);
            let old_functions = diff_file
                .and_then(|file_diff| file_diff.reconstruct_preimage(&content))
                .map(|preimage| function_definitions(&preimage));
            for &(start, end) in ranges {
                for definition in overlapping_definitions(&content, start, end) {
                    if definition.is_function
                        && diff_file.is_some_and(|file_diff| {
                            function_is_redundant(&definition, file_diff, old_functions.as_deref())
                        })
                    {
                        continue;
                    }

                    let (blk_start, blk_end) = definition.render_range(start, end);
                    add_range(&mut range_map, file_path.clone(), blk_start, blk_end);
                }
                already_extracted.extend(extract_defined_names(&content, start, end));
                symbols_to_lookup.extend(extract_type_names(&content, start, end));
            }
            called_functions.extend(extract_called_functions(&content, ranges));
        }
    }

    // Remove symbols whose definitions are already in context.
    for sym in &already_extracted {
        symbols_to_lookup.remove(sym);
    }

    // Drop opaque container types.
    let opaque = find_opaque_types(&symbols_to_lookup, &file_ranges, worktree_path).await;
    for sym in &opaque {
        symbols_to_lookup.remove(sym);
    }

    // Merge called functions *after* opaque filtering — find_opaque_types looks
    // for `struct X *var` declarations, so non-struct names (function calls) would
    // all be falsely classified as opaque and dropped.
    called_functions.retain(|f| !already_extracted.contains(f));
    symbols_to_lookup.extend(called_functions);

    // _ops structs are large vtables (e.g. net_device_ops) — not useful for review.
    symbols_to_lookup.retain(|s| !s.ends_with("_ops"));

    let symbols: Vec<String> = symbols_to_lookup.into_iter().take(50).collect();

    // Phase 2: look up referenced symbol definitions via git grep + tree-sitter.
    if !symbols.is_empty() {
        let regex_pattern = format!(
            "^((struct|enum|union)\\s+({0})\\b|#define\\s+({0})\\b|([a-zA-Z_][a-zA-Z0-9_ \\t*]+\\s+)?({0})\\s*\\()",
            symbols.join("|")
        );

        let caller_dirs: HashSet<&str> = file_ranges
            .keys()
            .filter_map(|f| f.rsplit_once('/').map(|(dir, _)| dir))
            .collect();

        let mut cmd = Command::new("git");
        cmd.current_dir(worktree_path)
            .arg("grep")
            .arg("-n")
            .arg("-I")
            .arg("-P")
            .arg("-e")
            .arg(&regex_pattern)
            .arg("--")
            .arg("*.c")
            .arg("*.h");

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return Err(anyhow!("Failed to run git grep: {}", e)),
        };

        if !output.status.success() {
            // git grep returns exit status 1 if no matches are found, which is not a hard error.
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                return Err(anyhow!("git grep failed: {}", stderr));
            }
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut candidates: HashMap<String, (Vec<(PathBuf, u64)>, Vec<(PathBuf, u64)>)> =
            HashMap::new();

        for line in stdout.lines() {
            if let Some((path_str, rest)) = line.split_once(':')
                && let Some((line_num_str, line_content)) = rest.split_once(':')
                && let Ok(line_num) = line_num_str.parse::<u64>()
            {
                let abs_path = worktree_path.join(path_str);
                let abs_path_str = abs_path.to_string_lossy();
                if is_noisy_tree(&abs_path_str) {
                    continue;
                }

                let rel = path_str;
                let is_priority = rel.starts_with("include/")
                    || caller_dirs
                        .iter()
                        .any(|d| rel.starts_with(d) && rel.as_bytes().get(d.len()) == Some(&b'/'));

                for sym in &symbols {
                    if line_matches_symbol(line_content, sym) {
                        let (general, priority) = candidates
                            .entry(sym.clone())
                            .or_insert_with(|| (Vec::new(), Vec::new()));

                        if is_priority {
                            if priority.len() < 32 {
                                priority.push((abs_path.clone(), line_num));
                            }
                        } else if general.len() < 32 {
                            general.push((abs_path.clone(), line_num));
                        }
                    }
                }
            }
        }

        for (sym, (general, priority)) in candidates {
            let mut hits = priority;
            hits.extend(general);
            if let Some((path, start, end)) =
                best_definition_range(&sym, &hits, worktree_path, &caller_dirs).await
            {
                add_range(&mut range_map, path, start, end);
            }
        }
    }

    render_range_map(&range_map, worktree_path, &file_ranges).await
}

/// Merge overlapping or adjacent ranges (within `gap` lines).
fn merge_ranges(ranges: &BTreeSet<(usize, usize)>, gap: usize) -> Vec<(usize, usize)> {
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for &(start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 + gap + 1 {
                last.1 = std::cmp::max(last.1, end);
            } else {
                merged.push((start, end));
            }
        } else {
            merged.push((start, end));
        }
    }
    merged
}

/// Render the collected line ranges into the final prefetch context string.
/// Modified files are rendered first (higher priority when nearing budget).
async fn render_range_map(
    range_map: &LineRangeMap,
    worktree_path: &Path,
    modified_files: &HashMap<String, Vec<(usize, usize)>>,
) -> Result<String> {
    let mut output = String::new();
    let mut current_chars = 0;

    let modified_paths: HashSet<PathBuf> = modified_files
        .keys()
        .map(|f| worktree_path.join(f))
        .collect();

    // Render modified files first, then definition-only files.
    let mut ordered_files: Vec<&PathBuf> = range_map.keys().collect();
    ordered_files.sort_by_key(|p| if modified_paths.contains(*p) { 0 } else { 1 });

    for file_path in ordered_files {
        let Some(ranges) = range_map.get(file_path) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(file_path).await else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        let relative = file_path
            .strip_prefix(worktree_path)
            .unwrap_or(file_path)
            .to_string_lossy();

        let merged = merge_ranges(ranges, 3);

        for &(start, end) in &merged {
            let clamped_end = std::cmp::min(end, lines.len().saturating_sub(1));

            let names = extract_defined_names(&content, start, clamped_end);
            let header = if names.len() == 1 {
                let name = names.into_iter().next().unwrap();
                format!("--- {}:{} ({}) ---\n", relative, start + 1, name)
            } else {
                format!("--- {}:{} ---\n", relative, start + 1)
            };

            let block: String = if clamped_end >= start && start < lines.len() {
                lines[start..=clamped_end].join("\n")
            } else {
                String::new()
            };

            if current_chars + header.len() + block.len() + 1 > MAX_PREFETCH_CHARS {
                output.push_str("\n... (Context prefetch limits reached)\n");
                return Ok(output);
            }

            output.push_str(&header);
            output.push_str(&block);
            output.push('\n');
            current_chars += header.len() + block.len() + 1;
        }
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Tree-sitter helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct DefinitionRange {
    start: usize,
    end: usize,
    name: Option<String>,
    is_function: bool,
}

impl DefinitionRange {
    fn render_range(&self, diff_start: usize, diff_end: usize) -> (usize, usize) {
        if self.end.saturating_sub(self.start) > 200 {
            let center = (diff_start + diff_end) / 2;
            (
                center.saturating_sub(100),
                std::cmp::min(center + 100, self.end),
            )
        } else {
            (self.start, self.end)
        }
    }
}

fn function_definitions(source_code: &str) -> Vec<DefinitionRange> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source_code, None) else {
        return Vec::new();
    };
    let source = source_code.as_bytes();
    let root = tree.root_node();
    let mut cursor = root.walk();
    root.children(&mut cursor)
        .filter(|node| node.kind() == "function_definition")
        .map(|node| DefinitionRange {
            start: node.start_position().row,
            end: node.end_position().row,
            name: function_name(node, source),
            is_function: true,
        })
        .collect()
}

fn function_is_redundant(
    function: &DefinitionRange,
    diff: &FileDiff,
    old_functions: Option<&[DefinitionRange]>,
) -> bool {
    // Multiple sections for one path describe intermediate revisions in a
    // series, while the worktree contains only the final revision.
    if diff.sections != 1 {
        return false;
    }
    if !diff.fully_displays(function.start, function.end) {
        return false;
    }

    let added = diff.added_lines();
    if (function.start..=function.end).all(|line| added.contains(&line)) {
        return true;
    }

    let removed = diff.removed_old_lines();
    if removed.is_empty() {
        return true;
    }

    let Some(old_functions) = old_functions else {
        return !diff.has_deletion_anchored_in(function.start, function.end);
    };
    let unchanged = diff.unchanged_line_pairs();
    let corresponding: Vec<&DefinitionRange> = old_functions
        .iter()
        .filter(|old| {
            old.name == function.name
                || unchanged.iter().any(|&(old_line, new_line)| {
                    old_line >= old.start
                        && old_line <= old.end
                        && new_line >= function.start
                        && new_line <= function.end
                })
        })
        .collect();

    if corresponding.is_empty() {
        return !diff.has_deletion_anchored_in(function.start, function.end);
    }
    !corresponding
        .iter()
        .any(|old| removed.range(old.start..=old.end).next().is_some())
}

/// Collect line ranges of all top-level definitions that overlap a diff range.
/// Returns complete, parseable definitions (functions, structs, enums, etc.)
/// rather than walking up to a single enclosing block.
fn overlapping_definitions(
    source_code: &str,
    start_line: usize,
    end_line: usize,
) -> Vec<DefinitionRange> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return vec![];
    }
    let Some(tree) = parser.parse(source_code, None) else {
        return vec![];
    };

    let target_kinds = [
        "function_definition",
        "struct_specifier",
        "enum_specifier",
        "union_specifier",
        "declaration",
        "type_definition",
        "preproc_def",
        "preproc_function_def",
    ];

    // Iterate root children (top-down) rather than walking up from the diff point.
    // Walking up finds only one enclosing block and misses sibling definitions
    // that also overlap the diff range.
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut ranges = Vec::new();
    for child in root.children(&mut cursor) {
        if child.end_position().row < start_line || child.start_position().row > end_line {
            continue;
        }
        if !target_kinds.contains(&child.kind()) {
            continue;
        }
        ranges.push(DefinitionRange {
            start: child.start_position().row,
            end: child.end_position().row,
            name: if child.kind() == "function_definition" {
                function_name(child, source_code.as_bytes())
            } else {
                None
            },
            is_function: child.kind() == "function_definition",
        });
    }
    ranges
}

/// Returns (block_text, symbol_name) for the first overlapping definition.
pub fn extract_enclosing_block(
    source_code: &str,
    start_line: usize,
    end_line: usize,
) -> Option<(String, Option<String>)> {
    let defs = overlapping_definitions(source_code, start_line, end_line);
    let definition = defs.first()?;
    let (blk_start, blk_end) = definition.render_range(start_line, end_line);
    let lines: Vec<&str> = source_code.lines().collect();
    let clamped_end = std::cmp::min(blk_end, lines.len().saturating_sub(1));
    let text = if clamped_end >= blk_start && blk_start < lines.len() {
        lines[blk_start..=clamped_end].join("\n")
    } else {
        return None;
    };

    let names = extract_defined_names(source_code, blk_start, clamped_end);
    let name = if names.len() == 1 {
        names.into_iter().next()
    } else {
        None
    };
    Some((text, name))
}

// ---------------------------------------------------------------------------
// Ripgrep + tree-sitter symbol lookup
// ---------------------------------------------------------------------------

// These directories contain userspace reimplementations of kernel primitives
// (e.g. tools/virtio/ringtest/ has a toy spin_lock) that shadow the real
// definitions and provide no signal for patch review.
fn is_noisy_tree(path_str: &str) -> bool {
    const NOISY_PREFIXES: &[&str] = &[
        "/tools/",
        "/samples/",
        "/Documentation/",
        "/scripts/",
        "/LICENSES/",
    ];
    NOISY_PREFIXES.iter().any(|p| path_str.contains(p))
}

fn line_matches_symbol(line: &str, sym: &str) -> bool {
    let bytes = line.as_bytes();
    let sym_bytes = sym.as_bytes();
    let mut i = 0;
    while let Some(pos) = line[i..].find(sym) {
        let start = i + pos;
        let end = start + sym_bytes.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = end;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Score a candidate definition block from tree-sitter. Higher is better.
/// 0 means "not actually a definition" (forward decl, parameter name, etc.).
fn score_definition_node(node: Node<'_>, sym: &str, source: &[u8]) -> i32 {
    let kind = node.kind();
    let names_symbol = |field: &str| {
        node.child_by_field_name(field)
            .and_then(|n| n.utf8_text(source).ok())
            .map(|t| t == sym)
            .unwrap_or(false)
    };
    let has_body = node.child_by_field_name("body").is_some();

    match kind {
        "struct_specifier" | "union_specifier" | "enum_specifier" => {
            if !names_symbol("name") {
                return 0;
            }
            if has_body { 100 } else { 0 }
        }
        "function_definition" => {
            let declared = function_name(node, source);
            if declared.as_deref() != Some(sym) {
                return 0;
            }
            if has_body { 90 } else { 0 }
        }
        "preproc_def" | "preproc_function_def" if names_symbol("name") => 70,
        "preproc_def" | "preproc_function_def" => 0,
        "type_definition" if typedef_names_match(node, sym, source) => 80,
        "type_definition" => 0,
        _ => 0,
    }
}

fn function_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cur = node.child_by_field_name("declarator")?;
    loop {
        match cur.kind() {
            "identifier" => return cur.utf8_text(source).ok().map(str::to_string),
            "function_declarator" | "pointer_declarator" | "parenthesized_declarator" => {
                cur = cur.child_by_field_name("declarator")?;
            }
            _ => return None,
        }
    }
}

fn typedef_names_match(node: Node<'_>, sym: &str, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_identifier" && child.utf8_text(source).ok() == Some(sym) {
            return true;
        }
    }
    false
}

/// Pick the highest-scoring definition across all ripgrep candidates for `sym`.
/// Total score = definition kind score + proximity score.
async fn best_definition_range(
    sym: &str,
    hits: &[(PathBuf, u64)],
    worktree_path: &Path,
    caller_dirs: &HashSet<&str>,
) -> Option<(PathBuf, usize, usize)> {
    let mut seen = HashSet::new();
    let mut best: Option<(i32, PathBuf, usize, usize)> = None;

    for (path, _line) in hits {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(content) = fs::read_to_string(path).await else {
            continue;
        };
        let Some((def_score, is_static, start, end)) = score_best_in_file_for_sym(&content, sym)
        else {
            continue;
        };
        if def_score == 0 {
            continue;
        }
        let rel_path = path
            .strip_prefix(worktree_path)
            .unwrap_or(path)
            .to_string_lossy();
        let score = def_score + proximity_score(&rel_path, is_static, caller_dirs);
        match &best {
            Some((best_score, _, _, _)) if *best_score >= score => {}
            _ => best = Some((score, path.clone(), start, end)),
        }
    }
    best.map(|(_, p, s, e)| (p, s, e))
}

fn proximity_score(def_path: &str, is_static: bool, caller_dirs: &HashSet<&str>) -> i32 {
    // Static .c definitions outside caller directories are almost certainly
    // wrong-file matches (e.g. mkregtable.c reimplements list_add_tail).
    if is_static && def_path.ends_with(".c") {
        let def_dir = def_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        if !caller_dirs.contains(def_dir) {
            return -200;
        }
    }

    let def_dir = def_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    if caller_dirs.contains(def_dir) {
        return 50;
    }

    if def_path.starts_with("include/") {
        return 40;
    }

    // Fall back to longest common path prefix with any caller directory.
    let best_common = caller_dirs
        .iter()
        .map(|cd| common_prefix_len(def_dir, cd))
        .max()
        .unwrap_or(0);

    best_common as i32
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

/// Parse `content` and find the highest-scoring definition of `sym`.
/// Returns (score, is_static, start_line, end_line).
fn score_best_in_file_for_sym(content: &str, sym: &str) -> Option<(i32, bool, usize, usize)> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c::LANGUAGE.into()).ok()?;
    let tree = parser.parse(content, None)?;
    let source = content.as_bytes();

    let mut best: Option<(i32, Node)> = None;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let score = score_definition_node(node, sym, source);
        if score > 0 {
            match &best {
                Some((b, _)) if *b >= score => {}
                _ => best = Some((score, node)),
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    let (score, node) = best?;
    let is_static = has_static_storage(node, source);
    let start = node.start_position().row;
    let end = node.end_position().row;
    let line_count = end.saturating_sub(start);
    if line_count > 200 {
        Some((score, is_static, start, std::cmp::min(start + 200, end)))
    } else {
        Some((score, is_static, start, end))
    }
}

fn has_static_storage(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "storage_class_specifier"
            && child.utf8_text(source).ok() == Some("static")
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Symbol extraction helpers
// ---------------------------------------------------------------------------

fn is_common_c_word(word: &str) -> bool {
    const COMMON: &[&str] = &[
        "int", "char", "void", "long", "short", "unsigned", "signed", "struct", "union", "enum",
        "typedef", "static", "const", "volatile", "if", "else", "for", "while", "do", "switch",
        "case", "default", "return", "break", "continue", "goto", "sizeof", "true", "false",
        "NULL", "inline", "extern", "register", "auto", "restrict", "u8", "u16", "u32", "u64",
        "s8", "s16", "s32", "s64", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "int8_t",
        "int16_t", "int32_t", "int64_t", "bool", "size_t", "ssize_t", "pid_t", "uid_t", "gid_t",
        "off_t", "ret", "err", "len", "size", "res", "tmp", "val", "ptr", "idx", "out",
    ];
    COMMON.contains(&word)
}

/// Identifies types that are only used as opaque containers in the modified files.
///
/// A type is "opaque" if, across all modified files:
///   - no variable of that type is ever dereferenced (`var->member`), OR
///   - every dereferenced member name contains "priv"
async fn find_opaque_types(
    types: &HashSet<String>,
    file_ranges: &HashMap<String, Vec<(usize, usize)>>,
    worktree_path: &Path,
) -> HashSet<String> {
    if types.is_empty() {
        return HashSet::new();
    }

    let mut type_members: HashMap<&str, HashSet<String>> = HashMap::new();
    for t in types {
        type_members.insert(t, HashSet::new());
    }

    let decl_re = Regex::new(r"struct\s+(\w+)\s+\*(\w+)").unwrap();

    for file in file_ranges.keys() {
        let file_path = worktree_path.join(file);
        let Ok(content) = fs::read_to_string(&file_path).await else {
            continue;
        };

        let mut var_to_type: Vec<(String, String)> = Vec::new();
        for cap in decl_re.captures_iter(&content) {
            let type_name = cap[1].to_string();
            let var_name = cap[2].to_string();
            if type_members.contains_key(type_name.as_str()) {
                var_to_type.push((var_name, type_name));
            }
        }

        for (var, typ) in &var_to_type {
            let pattern = format!(r"{}\s*->\s*(\w+)", regex::escape(var));
            if let Ok(re) = Regex::new(&pattern) {
                for cap in re.captures_iter(&content) {
                    let member = cap[1].to_string();
                    type_members.get_mut(typ.as_str()).unwrap().insert(member);
                }
            }
        }
    }

    type_members
        .into_iter()
        .filter(|(_, members)| members.is_empty() || members.iter().all(|m| m.contains("priv")))
        .map(|(t, _)| t.to_string())
        .collect()
}

/// Collects the names of all function/struct/enum/union definitions that overlap
/// the given line range.
fn extract_defined_names(source_code: &str, start_line: usize, end_line: usize) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return names;
    }
    let Some(tree) = parser.parse(source_code, None) else {
        return names;
    };
    let source = source_code.as_bytes();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.end_position().row < start_line || child.start_position().row > end_line {
            continue;
        }
        let name = match child.kind() {
            "function_definition" => function_name(child, source),
            "struct_specifier" | "enum_specifier" | "union_specifier" => child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string),
            _ => None,
        };
        if let Some(n) = name {
            names.insert(n);
        }
    }
    names
}

/// Extracts function call names from modified lines using tree-sitter.
fn extract_called_functions(source_code: &str, diff_ranges: &[(usize, usize)]) -> HashSet<String> {
    let mut funcs = HashSet::new();
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return funcs;
    }
    let Some(tree) = parser.parse(source_code, None) else {
        return funcs;
    };
    let source = source_code.as_bytes();

    fn collect_calls(
        node: Node<'_>,
        source: &[u8],
        diff_ranges: &[(usize, usize)],
        out: &mut HashSet<String>,
    ) {
        if node.kind() == "call_expression" {
            let row = node.start_position().row;
            let in_diff = diff_ranges.iter().any(|&(s, e)| row >= s && row <= e);
            if in_diff && let Some(func) = node.child_by_field_name("function") {
                // Skip field_expression (e.g. obj->method) — only direct calls.
                if func.kind() == "identifier"
                    && let Ok(name) = func.utf8_text(source)
                    && name.len() >= 3
                    && !is_common_c_word(name)
                {
                    out.insert(name.to_string());
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_calls(child, source, diff_ranges, out);
        }
    }

    collect_calls(tree.root_node(), source, diff_ranges, &mut funcs);
    funcs
}

/// Extracts C type names referenced within (and around) the modified line range.
pub fn extract_type_names(
    source_code: &str,
    start_line: usize,
    end_line: usize,
) -> HashSet<String> {
    let mut types = HashSet::new();
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return types;
    }

    let Some(tree) = parser.parse(source_code, None) else {
        return types;
    };
    let root_node = tree.root_node();
    let start_point = Point::new(start_line, 0);
    let end_point = Point::new(end_line, usize::MAX);

    let Some(mut scope) = root_node.descendant_for_point_range(start_point, end_point) else {
        return types;
    };

    let target_kinds = [
        "function_definition",
        "struct_specifier",
        "union_specifier",
        "enum_specifier",
        "type_definition",
    ];
    // Walk up from the diff range to find the enclosing definition. If we hit
    // root (file-scope code), we restrict type extraction to just the diff lines
    // to avoid pulling types from unrelated functions in the same file.
    let hit_root = loop {
        if target_kinds.contains(&scope.kind()) {
            break false;
        }
        match scope.parent() {
            Some(p) => scope = p,
            None => break true,
        }
    };

    fn walk(n: Node<'_>, src: &[u8], out: &mut HashSet<String>, bounds: Option<(usize, usize)>) {
        if let Some((lo, hi)) = bounds
            && (n.end_position().row < lo || n.start_position().row > hi)
        {
            return;
        }
        if n.kind() == "type_identifier"
            && let Ok(text) = n.utf8_text(src)
        {
            let s = text.to_string();
            if s.len() >= 3 && !is_common_c_word(&s) {
                out.insert(s);
            }
        }
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            walk(child, src, out, bounds);
        }
    }
    // Also restrict for struct/union scopes (huge headers like netdevice.h) and
    // when the parse tree has errors (scope is unreliable, fall back to range).
    let bounds = if hit_root
        || scope.kind() == "struct_specifier"
        || scope.kind() == "union_specifier"
        || scope.has_error()
    {
        Some((start_line, end_line))
    } else {
        None
    };
    walk(scope, source_code.as_bytes(), &mut types, bounds);
    types
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redundant_function(postimage: &str, diff: &str) -> bool {
        let parsed = parse_unified_diff(diff);
        let file_diff = parsed.get("file.c").unwrap();
        let function = function_definitions(postimage).into_iter().next().unwrap();
        let old_functions = file_diff
            .reconstruct_preimage(postimage)
            .map(|preimage| function_definitions(&preimage));
        function_is_redundant(&function, file_diff, old_functions.as_deref())
    }

    #[test]
    fn test_parse_diff_ranges() {
        let diff = r#"
--- a/file.c
+++ b/file.c
@@ -10,2 +10,4 @@
 context
+new line 1
+new line 2
 context
@@ -50,0 +52,1 @@
+new line 3
"#;
        let ranges = parse_diff_ranges(diff);
        assert_eq!(ranges.len(), 1);
        let file_ranges = ranges.get("file.c").unwrap();
        assert_eq!(file_ranges.len(), 2);
        assert_eq!(file_ranges[0], (9, 12)); // 0-based: 10->9, count 4 -> 9,10,11,12 -> end 12
        assert_eq!(file_ranges[1], (51, 51)); // 0-based: 52->51, count 1 -> 51
    }

    #[test]
    fn test_extract_enclosing_block() {
        let source_code = r#"#include <stdio.h>

int main() {
    int a = 1;
    // target line 4 (0-based)
    printf("hello");
    return 0;
}

struct MyStruct {
    int x;
};
"#;
        let (block_main, name_main) = extract_enclosing_block(source_code, 4, 4).unwrap();
        assert!(block_main.starts_with("int main() {"));
        assert!(block_main.ends_with("return 0;\n}"));
        assert_eq!(name_main.as_deref(), Some("main"));

        let (block_struct, name_struct) = extract_enclosing_block(source_code, 10, 10).unwrap();
        assert!(block_struct.starts_with("struct MyStruct"));
        assert_eq!(name_struct.as_deref(), Some("MyStruct"));
    }

    #[test]
    fn test_merge_ranges() {
        let mut ranges = BTreeSet::new();
        ranges.insert((10, 20));
        ranges.insert((22, 30)); // gap of 1 — merges with gap=3
        ranges.insert((50, 60)); // gap of 19 — does not merge
        let merged = merge_ranges(&ranges, 3);
        assert_eq!(merged, vec![(10, 30), (50, 60)]);
    }

    #[test]
    fn test_fully_visible_new_function_is_redundant() {
        let postimage = "static int sample(int value)\n{\n    return value;\n}\n";
        let diff = "--- /dev/null\n+++ b/file.c\n@@ -0,0 +1,4 @@\n+static int sample(int value)\n+{\n+    return value;\n+}\n";

        assert!(redundant_function(postimage, diff));
    }

    #[test]
    fn test_fully_visible_additive_function_is_redundant() {
        let postimage = "static int sample(int value)\n{\n    value++;\n    return value;\n}\n";
        let diff = "--- a/file.c\n+++ b/file.c\n@@ -1,4 +1,5 @@\n static int sample(int value)\n {\n+    value++;\n     return value;\n }\n";

        assert!(redundant_function(postimage, diff));
    }

    #[test]
    fn test_partially_visible_additive_function_needs_context() {
        let postimage = "static int sample(int value)\n{\n    int result = value;\n\n    result++;\n    result *= 2;\n\n    return result;\n}\n";
        let diff = "--- a/file.c\n+++ b/file.c\n@@ -3,3 +3,4 @@\n     int result = value;\n \n+    result++;\n     result *= 2;\n";

        assert!(!redundant_function(postimage, diff));
    }

    #[test]
    fn test_body_removal_needs_context() {
        let postimage = "static int sample(int value)\n{\n    return value + 1;\n}\n";
        let diff = "--- a/file.c\n+++ b/file.c\n@@ -1,4 +1,4 @@\n static int sample(int value)\n {\n-    return value;\n+    return value + 1;\n }\n";

        assert!(!redundant_function(postimage, diff));
    }

    #[test]
    fn test_declaration_removal_needs_context() {
        let postimage = "static long sample(long value)\n{\n    return value;\n}\n";
        let diff = "--- a/file.c\n+++ b/file.c\n@@ -1,4 +1,4 @@\n-static int sample(int value)\n+static long sample(long value)\n {\n     return value;\n }\n";

        assert!(!redundant_function(postimage, diff));
    }

    #[test]
    fn test_adjacent_removal_does_not_disqualify_additive_function() {
        let postimage = "static int sample(int value)\n{\n    value++;\n    return value;\n}\n";
        let diff = "--- a/file.c\n+++ b/file.c\n@@ -1,5 +1,5 @@\n-static int obsolete;\n static int sample(int value)\n {\n+    value++;\n     return value;\n }\n";

        assert!(redundant_function(postimage, diff));
    }

    #[test]
    fn test_multiple_revisions_of_file_keep_function_context() {
        let postimage = "static int sample(int value)\n{\n    value++;\n    return value;\n}\n";
        let diff = "--- a/file.c\n+++ b/file.c\n@@ -1,4 +1,5 @@\n static int sample(int value)\n {\n+    value++;\n     return value;\n }\n--- a/file.c\n+++ b/file.c\n@@ -1,5 +1,5 @@\n static int sample(int value)\n {\n     value++;\n     return value;\n }\n";

        assert!(!redundant_function(postimage, diff));
    }

    #[tokio::test]
    async fn test_prefetch_omits_only_redundant_function() {
        let dir = tempfile::tempdir().unwrap();
        let postimage = "static int sample(int value)\n{\n    value++;\n    return value;\n}\n";
        std::fs::write(dir.path().join("file.c"), postimage).unwrap();
        let additive = "--- a/file.c\n+++ b/file.c\n@@ -1,4 +1,5 @@\n static int sample(int value)\n {\n+    value++;\n     return value;\n }\n";
        assert_eq!(prefetch_context(dir.path(), additive).await.unwrap(), "");

        let subtractive = "--- a/file.c\n+++ b/file.c\n@@ -1,5 +1,5 @@\n static int sample(int value)\n {\n-    value += 2;\n+    value++;\n     return value;\n }\n";
        let context = prefetch_context(dir.path(), subtractive).await.unwrap();
        assert!(context.contains("static int sample(int value)"));
    }
}
