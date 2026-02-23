use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Clone, Debug)]
struct AnnotationSpan {
    full_start: usize,
    full_end: usize,
    inner_start: usize,
    inner_end: usize,
}

#[derive(Clone, Debug)]
struct Locator {
    path: String,
    line: u32,
    columns: Vec<u32>,
}

#[derive(Clone, Debug)]
struct ParsedToken {
    byte_start: usize,
    byte_end: usize,
    locator: Locator,
    range: Range,
}

struct Backend {
    client: Client,
    documents: RwLock<HashMap<Url, String>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: RwLock::new(HashMap::new()),
        }
    }

    async fn read_document(&self, uri: &Url) -> Option<String> {
        if let Some(text) = self.documents.read().await.get(uri).cloned() {
            return Some(text);
        }

        let path = uri.to_file_path().ok()?;
        std::fs::read_to_string(path).ok()
    }

    fn resolve_target_url(&self, path: &str, source_uri: &Url) -> Option<Url> {
        let candidate = PathBuf::from(path);
        let resolved = if candidate.is_absolute() {
            candidate
        } else {
            let source_path = source_uri.to_file_path().ok()?;
            source_path.parent()?.join(candidate)
        };
        Url::from_file_path(resolved).ok()
    }

    async fn read_locator_line(&self, locator: &Locator, source_uri: &Url) -> Option<String> {
        let target_uri = self.resolve_target_url(&locator.path, source_uri)?;
        let text = self.read_document(&target_uri).await?;
        line_text_at(&text, locator.line).map(ToString::to_string)
    }

    fn collect_location_links<'a>(
        &self,
        tokens: impl IntoIterator<Item = &'a ParsedToken>,
        source_uri: &Url,
    ) -> Vec<LocationLink> {
        let mut links = Vec::new();
        let mut seen = HashSet::new();

        for token in tokens {
            if token.locator.line == 0 {
                continue;
            }

            let Some(url) = self.resolve_target_url(&token.locator.path, source_uri) else {
                continue;
            };

            let line = token.locator.line - 1;
            for &column in &token.locator.columns {
                if column == 0 {
                    continue;
                }

                let col = column - 1;
                let dedup_key = format!("{}:{line}:{col}", url);
                if !seen.insert(dedup_key) {
                    continue;
                }

                let target_range = Range::new(
                    Position::new(line, col),
                    Position::new(line, col.saturating_add(1)),
                );
                links.push(LocationLink {
                    origin_selection_range: None,
                    target_uri: url.clone(),
                    target_range,
                    target_selection_range: target_range,
                });
            }
        }

        links
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "firrtl-source-locator".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let _ = self
            .client
            .log_message(
                MessageType::INFO,
                "firrtl-source-locator ready: Go to Definition for @[...] is enabled",
            )
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.documents
            .write()
            .await
            .insert(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.documents
                .write()
                .await
                .insert(params.text_document.uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .write()
            .await
            .remove(&params.text_document.uri);
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let text_document_position = params.text_document_position_params;
        let uri = text_document_position.text_document.uri;
        let position = text_document_position.position;

        let Some(text) = self.read_document(&uri).await else {
            return Ok(None);
        };

        let line_starts = compute_line_starts(&text);
        let Some(offset) = position_to_offset(position, &text, &line_starts) else {
            return Ok(None);
        };

        let Some(annotation) = find_annotation_at_offset(&text, offset) else {
            return Ok(None);
        };

        let tokens = parse_tokens_from_annotation(&text, &annotation, &line_starts);
        if tokens.is_empty() {
            return Ok(None);
        }

        let links = self.collect_location_links(tokens.iter(), &uri);

        if links.is_empty() {
            return Ok(None);
        }

        Ok(Some(GotoDefinitionResponse::Link(links)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let text_document_position = params.text_document_position_params;

        let uri = text_document_position.text_document.uri;
        let position = text_document_position.position;

        let Some(text) = self.read_document(&uri).await else {
            return Ok(None);
        };

        let line_starts = compute_line_starts(&text);
        let Some(offset) = position_to_offset(position, &text, &line_starts) else {
            return Ok(None);
        };

        let Some(annotation) = find_annotation_at_offset(&text, offset) else {
            return Ok(None);
        };

        let tokens = parse_tokens_from_annotation(&text, &annotation, &line_starts);
        let (summary_start, summary_end) =
            summary_hover_byte_range(&text, &annotation, &line_starts);
        if offset >= summary_start && offset < summary_end {
            let mut blocks = Vec::new();

            for token in &tokens {
                let source_line = self
                    .read_locator_line(&token.locator, &uri)
                    .await
                    .unwrap_or_else(|| "<source line unavailable>".to_string());
                let language = markdown_language_from_path(&token.locator.path);
                let column_line = build_column_indicator_line(&source_line, &token.locator.columns);
                blocks.push(format!("```{language}\n{source_line}\n{column_line}\n```"));
            }

            if blocks.is_empty() {
                return Ok(None);
            }

            let summary_range = Range::new(
                offset_to_position(summary_start, &text, &line_starts),
                offset_to_position(summary_end, &text, &line_starts),
            );

            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: blocks.join("\n"),
                }),
                range: Some(summary_range),
            }));
        }

        let Some(token) = tokens
            .iter()
            .find(|token| offset >= token.byte_start && offset < token.byte_end)
        else {
            return Ok(None);
        };

        let source_line = self
            .read_locator_line(&token.locator, &uri)
            .await
            .unwrap_or_else(|| "<source line unavailable>".to_string());
        let column_line = build_column_indicator_line(&source_line, &token.locator.columns);
        let language = markdown_language_from_path(&token.locator.path);
        let value = format!(
            "```{language}\n{source_line}\n{column_line}\n```\n{}",
            format_locator(&token.locator)
        );

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(token.range),
        }))
    }
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn position_to_offset(position: Position, text: &str, line_starts: &[usize]) -> Option<usize> {
    let line = position.line as usize;
    if line >= line_starts.len() {
        return None;
    }

    let line_start = line_starts[line];
    let line_end = if line + 1 < line_starts.len() {
        line_starts[line + 1]
    } else {
        text.len()
    };
    let line_text = &text[line_start..line_end];

    let mut remaining_utf16 = position.character as usize;
    for (idx, ch) in line_text.char_indices() {
        let width = ch.len_utf16();
        if remaining_utf16 == 0 {
            return Some(line_start + idx);
        }
        if remaining_utf16 < width {
            return Some(line_start + idx);
        }
        remaining_utf16 -= width;
    }

    Some(line_end)
}

fn offset_to_position(offset: usize, text: &str, line_starts: &[usize]) -> Position {
    let clamped = offset.min(text.len());
    let line = match line_starts.binary_search(&clamped) {
        Ok(index) => index,
        Err(0) => 0,
        Err(index) => index - 1,
    };

    let line_start = line_starts[line];
    let utf16_col = text[line_start..clamped]
        .chars()
        .map(|ch| ch.len_utf16() as u32)
        .sum();

    Position::new(line as u32, utf16_col)
}

fn find_annotations(text: &str) -> Vec<AnnotationSpan> {
    let mut spans = Vec::new();
    let mut cursor = 0;

    while let Some(relative_start) = text[cursor..].find("@[") {
        let full_start = cursor + relative_start;
        let inner_start = full_start + 2;

        let Some(relative_end) = text[inner_start..].find(']') else {
            break;
        };

        let inner_end = inner_start + relative_end;
        let full_end = inner_end + 1;

        spans.push(AnnotationSpan {
            full_start,
            full_end,
            inner_start,
            inner_end,
        });

        cursor = full_end;
    }

    spans
}

fn find_annotation_at_offset(text: &str, offset: usize) -> Option<AnnotationSpan> {
    find_annotations(text)
        .into_iter()
        .find(|span| offset >= span.full_start && offset < span.full_end)
}

fn split_locator_tokens(inner: &str) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut brace_depth = 0usize;

    for (idx, ch) in inner.char_indices() {
        match ch {
            '{' => brace_depth += 1,
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
            }
            ',' if brace_depth == 0 => {
                result.push((start, idx));
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    if start <= inner.len() {
        result.push((start, inner.len()));
    }

    result
}

fn parse_columns(columns_text: &str) -> Option<Vec<u32>> {
    let trimmed = columns_text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let columns: Vec<u32> = inner
            .split(',')
            .filter_map(|part| part.trim().parse::<u32>().ok())
            .collect();
        if columns.is_empty() {
            None
        } else {
            Some(columns)
        }
    } else {
        trimmed.parse::<u32>().ok().map(|column| vec![column])
    }
}

fn parse_locator_token(token_text: &str, last_path: Option<&str>) -> Option<(Locator, bool)> {
    let trimmed = token_text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let last_colon = trimmed.rfind(':')?;
    let columns_text = &trimmed[last_colon + 1..];
    let before_columns = &trimmed[..last_colon];

    let line_colon = before_columns.rfind(':')?;
    let path_text = &before_columns[..line_colon];
    let line_text = &before_columns[line_colon + 1..];

    let line = line_text.parse::<u32>().ok()?;
    let columns = parse_columns(columns_text)?;

    let (path, used_inherited_path) = if path_text.is_empty() {
        (last_path?.to_string(), true)
    } else {
        (path_text.to_string(), false)
    };

    Some((
        Locator {
            path,
            line,
            columns,
        },
        used_inherited_path,
    ))
}

fn parse_tokens_from_annotation(
    text: &str,
    annotation: &AnnotationSpan,
    line_starts: &[usize],
) -> Vec<ParsedToken> {
    let inner = &text[annotation.inner_start..annotation.inner_end];
    let mut parsed = Vec::new();
    let mut last_path: Option<String> = None;

    for (raw_start, raw_end) in split_locator_tokens(inner) {
        if raw_start >= raw_end || raw_end > inner.len() {
            continue;
        }

        let raw = &inner[raw_start..raw_end];
        let leading = raw.len() - raw.trim_start().len();
        let trailing = raw.len() - raw.trim_end().len();
        if leading + trailing >= raw.len() {
            continue;
        }

        let token_start = raw_start + leading;
        let token_end = raw_end - trailing;
        let token_text = inner[token_start..token_end].to_string();

        let Some((locator, used_inherited_path)) =
            parse_locator_token(&token_text, last_path.as_deref())
        else {
            continue;
        };

        if !used_inherited_path {
            last_path = Some(locator.path.clone());
        }

        let byte_start = annotation.inner_start + token_start;
        let byte_end = annotation.inner_start + token_end;

        parsed.push(ParsedToken {
            byte_start,
            byte_end,
            range: Range::new(
                offset_to_position(byte_start, text, line_starts),
                offset_to_position(byte_end, text, line_starts),
            ),
            locator,
        });
    }

    parsed
}

fn format_locator(locator: &Locator) -> String {
    if locator.columns.len() == 1 {
        format!("{}:{}:{}", locator.path, locator.line, locator.columns[0])
    } else {
        let columns = locator
            .columns
            .iter()
            .map(|column| column.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!("{}:{}:{{{columns}}}", locator.path, locator.line)
    }
}

fn line_text_at(text: &str, one_based_line: u32) -> Option<&str> {
    let line_index = usize::try_from(one_based_line.checked_sub(1)?).ok()?;
    let line = text.split('\n').nth(line_index)?;
    Some(line.strip_suffix('\r').unwrap_or(line))
}

fn build_column_indicator_line(source_line: &str, columns: &[u32]) -> String {
    let mut indicators: Vec<char> = source_line
        .chars()
        .map(|ch| if ch == '\t' { '\t' } else { ' ' })
        .collect();

    let mut has_valid_column = false;
    for &column in columns {
        if column == 0 {
            continue;
        }
        has_valid_column = true;
        let index = (column - 1) as usize;
        if index >= indicators.len() {
            indicators.resize(index + 1, ' ');
        }
        indicators[index] = '^';
    }

    if !has_valid_column {
        return "^".to_string();
    }

    if let Some(last) = indicators.iter().rposition(|ch| *ch == '^') {
        indicators.truncate(last + 1);
    }

    indicators.into_iter().collect()
}

fn markdown_language_from_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".scala") {
        "scala"
    } else if lower.ends_with(".fir") || lower.ends_with(".firrtl") {
        "firrtl"
    } else if lower.ends_with(".rs") {
        "rust"
    } else if lower.ends_with(".py") {
        "python"
    } else if lower.ends_with(".sv") || lower.ends_with(".svh") || lower.ends_with(".v") {
        "verilog"
    } else {
        "text"
    }
}

fn line_start_for_offset(offset: usize, line_starts: &[usize]) -> usize {
    let line = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(0) => 0,
        Err(index) => index - 1,
    };
    line_starts[line]
}

fn summary_hover_byte_range(
    text: &str,
    annotation: &AnnotationSpan,
    line_starts: &[usize],
) -> (usize, usize) {
    let at_start = annotation.full_start;
    let fallback_end = (at_start + 2).min(text.len());
    let line_start = line_start_for_offset(at_start, line_starts);
    let bytes = text.as_bytes();

    if at_start + 2 <= bytes.len() {
        if at_start >= 3 {
            let start = at_start - 3;
            if start >= line_start && bytes[start..at_start + 2] == *b"// @[" {
                return (start, at_start + 2);
            }
        }

        if at_start >= 2 {
            let start = at_start - 2;
            if start >= line_start && bytes[start..at_start + 2] == *b"//@[" {
                return (start, at_start + 2);
            }
        }
    }

    (at_start, fallback_end)
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_inherited_path_token() {
        let first = parse_locator_token("/tmp/Foo.scala:12:5", None).unwrap();
        assert_eq!(first.0.path, "/tmp/Foo.scala");
        assert!(!first.1);

        let inherited = parse_locator_token(":13:{7,9}", Some(&first.0.path)).unwrap();
        assert_eq!(inherited.0.path, "/tmp/Foo.scala");
        assert_eq!(inherited.0.columns, vec![7, 9]);
        assert!(inherited.1);
    }

    #[test]
    fn split_tokens_respects_braces() {
        let input = "/a.scala:1:2, :3:{4,5,6}, /b.scala:7:8";
        let slices = split_locator_tokens(input);
        let tokens: Vec<&str> = slices
            .iter()
            .map(|(start, end)| &input[*start..*end])
            .collect();

        assert_eq!(tokens, vec!["/a.scala:1:2", " :3:{4,5,6}", " /b.scala:7:8"]);
    }

    #[test]
    fn parse_annotation_example() {
        let text = "wire x; // @[/tmp/A.scala:10:3, :11:{4,9}, /tmp/B.scala:12:8]";
        let lines = compute_line_starts(text);
        let annotation = find_annotations(text).pop().unwrap();
        let tokens = parse_tokens_from_annotation(text, &annotation, &lines);

        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].locator.path, "/tmp/A.scala");
        assert_eq!(tokens[1].locator.path, "/tmp/A.scala");
        assert_eq!(tokens[2].locator.path, "/tmp/B.scala");
        assert_eq!(tokens[1].locator.columns, vec![4, 9]);
    }

    #[test]
    fn line_text_at_supports_crlf() {
        let text = "line1\r\nline2\r\nline3";
        assert_eq!(line_text_at(text, 1), Some("line1"));
        assert_eq!(line_text_at(text, 2), Some("line2"));
        assert_eq!(line_text_at(text, 3), Some("line3"));
        assert_eq!(line_text_at(text, 4), None);
    }

    #[test]
    fn column_indicator_marks_all_columns() {
        let marker = build_column_indicator_line("abcdef", &[2, 5]);
        assert_eq!(marker, " ^  ^");
    }

    #[test]
    fn markdown_language_from_extension() {
        assert_eq!(markdown_language_from_path("/tmp/src/Foo.scala"), "scala");
        assert_eq!(markdown_language_from_path("/tmp/src/foo.fir"), "firrtl");
        assert_eq!(markdown_language_from_path("/tmp/src/foo.unknown"), "text");
    }

    #[test]
    fn summary_hover_range_expands_to_comment_prefix() {
        let text = "wire x; // @[/tmp/A.scala:10:3]";
        let lines = compute_line_starts(text);
        let annotation = find_annotations(text).pop().unwrap();
        let (start, end) = summary_hover_byte_range(text, &annotation, &lines);
        assert_eq!(&text[start..end], "// @[");
    }

    #[test]
    fn summary_hover_range_falls_back_to_at_block() {
        let text = "@[/tmp/A.scala:10:3]";
        let lines = compute_line_starts(text);
        let annotation = find_annotations(text).pop().unwrap();
        let (start, end) = summary_hover_byte_range(text, &annotation, &lines);
        assert_eq!(&text[start..end], "@[");
    }
}
