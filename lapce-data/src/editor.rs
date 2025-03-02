use crate::command::InitBufferContentCb;
use crate::command::LapceCommand;
use crate::command::LAPCE_COMMAND;
use crate::command::LAPCE_SAVE_FILE_AS;
use crate::command::{CommandExecuted, CommandKind};
use crate::completion::{CompletionData, CompletionStatus, Snippet};
use crate::config::Config;
use crate::data::EditorView;
use crate::data::{
    EditorDiagnostic, InlineFindDirection, LapceEditorData, LapceMainSplitData,
    SplitContent,
};
use crate::document::BufferContent;
use crate::document::Document;
use crate::document::LocalBufferKind;
use crate::hover::HoverData;
use crate::hover::HoverStatus;
use crate::keypress::KeyMap;
use crate::keypress::KeyPressFocus;
use crate::palette::PaletteData;
use crate::proxy::path_from_url;
use crate::proxy::RequestError;
use crate::{
    command::{
        EnsureVisiblePosition, InitBufferContent, LapceUICommand, LAPCE_UI_COMMAND,
    },
    split::SplitMoveDirection,
};
use crate::{find::Find, split::SplitDirection};
use crate::{proxy::LapceProxy, source_control::SourceControlData};
use anyhow::{anyhow, Result};
use crossbeam_channel::{self, bounded};
use druid::piet::PietTextLayout;
use druid::piet::Svg;
use druid::FileDialogOptions;
use druid::Modifiers;
use druid::{
    piet::PietText, Command, Env, EventCtx, Point, Rect, Target, Vec2, WidgetId,
};
use druid::{ExtEventSink, MouseEvent};
use indexmap::IndexMap;
use lapce_core::buffer::Buffer;
use lapce_core::buffer::{DiffLines, InvalLines};
use lapce_core::command::{
    EditCommand, FocusCommand, MotionModeCommand, MultiSelectionCommand,
};
use lapce_core::editor::EditType;
use lapce_core::mode::{Mode, MotionMode};
use lapce_core::selection::InsertDrift;
use lapce_core::selection::Selection;
pub use lapce_core::syntax::Syntax;
use lsp_types::request::GotoTypeDefinitionResponse;
use lsp_types::CodeActionOrCommand;
use lsp_types::CompletionTextEdit;
use lsp_types::DocumentChangeOperation;
use lsp_types::DocumentChanges;
use lsp_types::OneOf;
use lsp_types::TextEdit;
use lsp_types::Url;
use lsp_types::WorkspaceEdit;
use lsp_types::{
    CodeActionResponse, CompletionItem, DiagnosticSeverity, GotoDefinitionResponse,
    Location, Position,
};
use std::cmp::Ordering;
use std::path::Path;
use std::thread;
use std::{collections::HashMap, sync::Arc};
use std::{iter::Iterator, path::PathBuf};
use std::{str::FromStr, time::Duration};
use xi_rope::Rope;
use xi_rope::{RopeDelta, Transformer};

pub struct LapceUI {}

#[derive(Copy, Clone)]
pub struct EditorCount(Option<usize>);

#[derive(Copy, Clone)]
pub enum EditorOperator {
    Delete(EditorCount),
    Yank(EditorCount),
}

pub trait EditorPosition: Sized {
    /// Convert the position to a utf8 offset
    fn to_utf8_offset(&self, buffer: &Buffer) -> Option<usize>;

    fn init_buffer_content_cmd(
        path: PathBuf,
        content: Rope,
        locations: Vec<(WidgetId, EditorLocation<Self>)>,
        edits: Option<Rope>,
        cb: Option<InitBufferContentCb>,
    ) -> LapceUICommand;
}

// Usize is always a utf8 offset
impl EditorPosition for usize {
    fn to_utf8_offset(&self, _buffer: &Buffer) -> Option<usize> {
        Some(*self)
    }

    fn init_buffer_content_cmd(
        path: PathBuf,
        content: Rope,
        locations: Vec<(WidgetId, EditorLocation<Self>)>,
        unsaved_buffers: Option<Rope>,
        cb: Option<InitBufferContentCb>,
    ) -> LapceUICommand {
        LapceUICommand::InitBufferContent(InitBufferContent {
            path,
            content,
            locations,
            edits: unsaved_buffers,
            cb,
        })
    }
}

/// Jump to first non blank character on a line
/// (If you want to jump to the very first character then use `LineCol` with column set to 0)
#[derive(Debug, Clone, Copy)]
pub struct Line(pub usize);
impl EditorPosition for Line {
    fn to_utf8_offset(&self, buffer: &Buffer) -> Option<usize> {
        Some(buffer.first_non_blank_character_on_line(self.0.saturating_sub(1)))
    }

    fn init_buffer_content_cmd(
        path: PathBuf,
        content: Rope,
        locations: Vec<(WidgetId, EditorLocation<Self>)>,
        edits: Option<Rope>,
        cb: Option<InitBufferContentCb>,
    ) -> LapceUICommand {
        LapceUICommand::InitBufferContentLine(InitBufferContent {
            path,
            content,
            locations,
            edits,
            cb,
        })
    }
}

/// UTF8 line and column-offset
#[derive(Debug, Clone, Copy)]
pub struct LineCol {
    pub line: usize,
    pub column: usize,
}
impl EditorPosition for LineCol {
    fn to_utf8_offset(&self, buffer: &Buffer) -> Option<usize> {
        Some(buffer.offset_of_line_col(self.line, self.column))
    }

    fn init_buffer_content_cmd(
        path: PathBuf,
        content: Rope,
        locations: Vec<(WidgetId, EditorLocation<Self>)>,
        edits: Option<Rope>,
        cb: Option<InitBufferContentCb>,
    ) -> LapceUICommand {
        LapceUICommand::InitBufferContentLineCol(InitBufferContent {
            path,
            content,
            locations,
            edits,
            cb,
        })
    }
}

impl EditorPosition for Position {
    fn to_utf8_offset(&self, buffer: &Buffer) -> Option<usize> {
        buffer.offset_of_position(self)
    }

    fn init_buffer_content_cmd(
        path: PathBuf,
        content: Rope,
        locations: Vec<(WidgetId, EditorLocation<Self>)>,
        edits: Option<Rope>,
        cb: Option<InitBufferContentCb>,
    ) -> LapceUICommand {
        LapceUICommand::InitBufferContentLsp(InitBufferContent {
            path,
            content,
            locations,
            edits,
            cb,
        })
    }
}

#[derive(Clone, Debug)]
pub struct EditorLocation<P: EditorPosition = usize> {
    pub path: PathBuf,
    pub position: Option<P>,
    pub scroll_offset: Option<Vec2>,
    pub history: Option<String>,
}
impl<P: EditorPosition> EditorLocation<P> {
    pub fn into_utf8_location(self, buffer: &Buffer) -> EditorLocation<usize> {
        EditorLocation {
            path: self.path,
            position: self.position.and_then(|p| p.to_utf8_offset(buffer)),
            scroll_offset: self.scroll_offset,
            history: self.history,
        }
    }
}

pub struct LapceEditorBufferData {
    pub view_id: WidgetId,
    pub editor: Arc<LapceEditorData>,
    pub doc: Arc<Document>,
    pub completion: Arc<CompletionData>,
    pub hover: Arc<HoverData>,
    pub main_split: LapceMainSplitData,
    pub source_control: Arc<SourceControlData>,
    pub palette: Arc<PaletteData>,
    pub find: Arc<Find>,
    pub proxy: Arc<LapceProxy>,
    pub command_keymaps: Arc<IndexMap<String, Vec<KeyMap>>>,
    pub config: Arc<Config>,
}

impl LapceEditorBufferData {
    fn doc_mut(&mut self) -> &mut Document {
        Arc::make_mut(&mut self.doc)
    }

    pub fn sync_buffer_position(&mut self, scroll_offset: Vec2) {
        let cursor_offset = self.editor.cursor.offset();
        if self.doc.cursor_offset != cursor_offset
            || self.doc.scroll_offset != scroll_offset
        {
            let doc = self.doc_mut();
            doc.cursor_offset = cursor_offset;
            doc.scroll_offset = scroll_offset;
        }
    }

    fn inline_find(
        &mut self,
        ctx: &mut EventCtx,
        direction: InlineFindDirection,
        c: &str,
    ) {
        let offset = self.editor.cursor.offset();
        let line = self.doc.buffer().line_of_offset(offset);
        let line_content = self.doc.buffer().line_content(line);
        let line_start_offset = self.doc.buffer().offset_of_line(line);
        let index = offset - line_start_offset;
        if let Some(new_index) = match direction {
            InlineFindDirection::Left => line_content[..index].rfind(c),
            InlineFindDirection::Right => {
                if index + 1 >= line_content.len() {
                    None
                } else {
                    let index = index
                        + self.doc.buffer().next_grapheme_offset(
                            offset,
                            1,
                            self.doc.buffer().offset_line_end(offset, false),
                        )
                        - offset;
                    line_content[index..].find(c).map(|i| i + index)
                }
            }
        } {
            self.run_move_command(
                ctx,
                &lapce_core::movement::Movement::Offset(
                    new_index + line_start_offset,
                ),
                None,
                Modifiers::empty(),
            );
        }
    }

    pub fn get_code_actions(&self, ctx: &mut EventCtx) {
        if !self.doc.loaded() {
            return;
        }
        if !self.doc.content().is_file() {
            return;
        }
        if let BufferContent::File(path) = self.doc.content() {
            let path = path.clone();
            let offset = self.editor.cursor.offset();
            let prev_offset = self.doc.buffer().prev_code_boundary(offset);
            if self.doc.code_actions.get(&prev_offset).is_none() {
                let buffer_id = self.doc.id();
                let position = if let Some(position) =
                    self.doc.buffer().offset_to_position(prev_offset)
                {
                    position
                } else {
                    log::error!("Failed to convert prev_offset: {prev_offset} to Position when getting code actions");
                    return;
                };
                let rev = self.doc.rev();
                let event_sink = ctx.get_external_handle();
                self.proxy
                    .get_code_actions(buffer_id, position, move |result| {
                        if let Ok(resp) = result {
                            let _ = event_sink.submit_command(
                                LAPCE_UI_COMMAND,
                                LapceUICommand::UpdateCodeActions(
                                    path,
                                    rev,
                                    prev_offset,
                                    resp,
                                ),
                                Target::Auto,
                            );
                        }
                    });
            }
        }
    }

    fn inactive_apply_delta(&mut self, delta: &RopeDelta) {
        for (view_id, editor) in self.main_split.editors.iter_mut() {
            if view_id != &self.editor.view_id
                && self.doc.content() == &editor.content
            {
                Arc::make_mut(editor).cursor.apply_delta(delta);
            }
        }
    }

    fn is_palette(&self) -> bool {
        self.editor.content == BufferContent::Local(LocalBufferKind::Palette)
    }

    /// Check if there are completions that are being rendered
    fn has_completions(&self) -> bool {
        self.completion.status != CompletionStatus::Inactive
            && self.completion.len() > 0
    }

    fn has_hover(&self) -> bool {
        self.hover.status != HoverStatus::Inactive && !self.hover.is_empty()
    }

    pub fn run_code_action(
        &mut self,
        ctx: &mut EventCtx,
        action: &CodeActionOrCommand,
    ) {
        if let BufferContent::File(path) = &self.editor.content {
            match action {
                CodeActionOrCommand::Command(_cmd) => {}
                CodeActionOrCommand::CodeAction(action) => {
                    if let Some(edit) = action.edit.as_ref() {
                        if let Some(edits) = workspace_edits(edit) {
                            for (url, edits) in edits {
                                if url_matches_path(path, &url) {
                                    let path = path.clone();
                                    let doc = self
                                        .main_split
                                        .open_docs
                                        .get(&path)
                                        .unwrap()
                                        .clone();
                                    apply_code_action(
                                        &doc,
                                        &mut self.main_split,
                                        &path,
                                        &edits,
                                    );
                                } else if let Ok(url_path) = url.to_file_path() {
                                    // If it is not for the file we have open then we assume that
                                    // we may have to load it
                                    // So we jump to the location that the edits were at.
                                    // TODO: url_matches_path checks if the url path 'goes back' to the original url
                                    // Should we do that here?

                                    // We choose to just jump to the start of the first edit. The edit function will jump
                                    // appropriately when we actually apply the edits.
                                    let position =
                                        edits.get(0).map(|edit| edit.range.start);
                                    self.main_split.jump_to_location_cb(
                                        ctx,
                                        None,
                                        EditorLocation {
                                            path: url_path.clone(),
                                            position,
                                            scroll_offset: None,
                                            history: None,
                                        },
                                        &self.config,
                                        // Note: For some reason Rust is unsure about what type the arguments are if we don't specify them
                                        // Perhaps this could be fixed by being very explicit about the lifetimes in the jump_to_location_cb fn?
                                        Some(move |_: &mut EventCtx, main_split: &mut LapceMainSplitData| {
                                            // The file has been loaded, so we want to apply the edits now.
                                            let doc = if let Some(doc) = main_split.open_docs.get(&url_path) {
                                                doc.clone()
                                            } else {
                                                log::warn!("Failed to load URL-path {url_path:?} properly. It was loaded but was not able to be found, which might indicate cross platform path confusion issues.");
                                                return;
                                            };

                                            apply_code_action(&doc, main_split, &url_path, &edits);
                                        }),
                                    );
                                } else {
                                    log::warn!("Text edits failed to apply to URL {url:?} because it was not found");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn apply_completion_item(&mut self, item: &CompletionItem) -> Result<()> {
        let additional_edit: Option<Option<Vec<_>>> =
            item.additional_text_edits.as_ref().map(|edits| {
                edits
                    .iter()
                    .map(|edit| {
                        let selection = lapce_core::selection::Selection::region(
                            self.doc.buffer().offset_of_position(&edit.range.start)?,
                            self.doc.buffer().offset_of_position(&edit.range.end)?,
                        );
                        Some((selection, edit.new_text.as_str()))
                    })
                    .collect::<Option<Vec<(lapce_core::selection::Selection, &str)>>>()
            });

        // If the inner option is empty
        if additional_edit
            .as_ref()
            .map(Option::is_none)
            .unwrap_or(false)
        {
            log::error!("Failed to convert completion item's additional edit Positions to offsets");
            return Err(anyhow!("Bad additional edit position"));
        }

        let additional_edit = additional_edit.flatten();

        let additional_edit: Vec<_> = additional_edit
            .as_ref()
            .map(|edits| {
                edits.iter().map(|(selection, c)| (selection, *c)).collect()
            })
            .unwrap_or_default();

        let text_format = item
            .insert_text_format
            .unwrap_or(lsp_types::InsertTextFormat::PLAIN_TEXT);
        if let Some(edit) = &item.text_edit {
            match edit {
                CompletionTextEdit::Edit(edit) => {
                    let offset = self.editor.cursor.offset();
                    let start_offset = self.doc.buffer().prev_code_boundary(offset);
                    let end_offset = self.doc.buffer().next_code_boundary(offset);
                    let edit_start = if let Some(edit_start) =
                        self.doc.buffer().offset_of_position(&edit.range.start)
                    {
                        edit_start
                    } else {
                        log::error!("Failed to convert completion edit start Position {:?} to offset", edit.range.start);
                        return Err(anyhow!("bad edit start position"));
                    };
                    let edit_end = if let Some(edit_end) =
                        self.doc.buffer().offset_of_position(&edit.range.end)
                    {
                        edit_end
                    } else {
                        log::error!("Failed to convert completion edit end Position {:?} to offset", edit.range.end);
                        return Err(anyhow!("bad edit end position"));
                    };

                    let selection = lapce_core::selection::Selection::region(
                        start_offset.min(edit_start),
                        end_offset.max(edit_end),
                    );
                    match text_format {
                        lsp_types::InsertTextFormat::PLAIN_TEXT => {
                            let (delta, inval_lines) = Arc::make_mut(&mut self.doc)
                                .do_raw_edit(
                                    &[
                                        &[(&selection, edit.new_text.as_str())][..],
                                        &additional_edit[..],
                                    ]
                                    .concat(),
                                    EditType::Completion,
                                );
                            let selection = selection.apply_delta(
                                &delta,
                                true,
                                InsertDrift::Default,
                            );
                            Arc::make_mut(&mut self.editor)
                                .cursor
                                .update_selection(self.doc.buffer(), selection);
                            self.apply_deltas(&[(delta, inval_lines)]);
                            return Ok(());
                        }
                        lsp_types::InsertTextFormat::SNIPPET => {
                            let snippet = Snippet::from_str(&edit.new_text)?;
                            let text = snippet.text();
                            let (delta, inval_lines) = Arc::make_mut(&mut self.doc)
                                .do_raw_edit(
                                    &[
                                        &[(&selection, text.as_str())][..],
                                        &additional_edit[..],
                                    ]
                                    .concat(),
                                    EditType::Completion,
                                );
                            let selection = selection.apply_delta(
                                &delta,
                                true,
                                InsertDrift::Default,
                            );

                            let mut transformer = Transformer::new(&delta);
                            let offset = transformer
                                .transform(start_offset.min(edit_start), false);
                            let snippet_tabs = snippet.tabs(offset);

                            if snippet_tabs.is_empty() {
                                Arc::make_mut(&mut self.editor)
                                    .cursor
                                    .update_selection(self.doc.buffer(), selection);
                                self.apply_deltas(&[(delta, inval_lines)]);
                                return Ok(());
                            }

                            let mut selection =
                                lapce_core::selection::Selection::new();
                            let (_tab, (start, end)) = &snippet_tabs[0];
                            let region = lapce_core::selection::SelRegion::new(
                                *start, *end, None,
                            );
                            selection.add_region(region);
                            Arc::make_mut(&mut self.editor)
                                .cursor
                                .set_insert(selection);
                            self.apply_deltas(&[(delta, inval_lines)]);
                            Arc::make_mut(&mut self.editor)
                                .add_snippet_placeholders(snippet_tabs);
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                CompletionTextEdit::InsertAndReplace(_) => (),
            }
        }

        let offset = self.editor.cursor.offset();
        let start_offset = self.doc.buffer().prev_code_boundary(offset);
        let end_offset = self.doc.buffer().next_code_boundary(offset);
        let selection = Selection::region(start_offset, end_offset);

        let (delta, inval_lines) = Arc::make_mut(&mut self.doc).do_raw_edit(
            &[
                &[(
                    &selection,
                    item.insert_text.as_deref().unwrap_or(item.label.as_str()),
                )][..],
                &additional_edit[..],
            ]
            .concat(),
            EditType::Completion,
        );
        let selection = selection.apply_delta(&delta, true, InsertDrift::Default);
        Arc::make_mut(&mut self.editor)
            .cursor
            .update_selection(self.doc.buffer(), selection);
        self.apply_deltas(&[(delta, inval_lines)]);
        Ok(())
    }

    pub fn cancel_completion(&mut self) {
        let completion = Arc::make_mut(&mut self.completion);
        completion.cancel();
    }

    pub fn cancel_hover(&mut self) {
        let hover = Arc::make_mut(&mut self.hover);
        hover.cancel();
    }

    /// Update the displayed autocompletion box
    /// Sends a request to the LSP for completion information
    fn update_completion(
        &mut self,
        ctx: &mut EventCtx,
        display_if_empty_input: bool,
    ) {
        if self.get_mode() != Mode::Insert {
            self.cancel_completion();
            return;
        }
        if !self.doc.loaded() {
            return;
        }
        if !self.doc.content().is_file() {
            return;
        }
        let offset = self.editor.cursor.offset();
        let start_offset = self.doc.buffer().prev_code_boundary(offset);
        let end_offset = self.doc.buffer().next_code_boundary(offset);
        let input = self
            .doc
            .buffer()
            .slice_to_cow(start_offset..end_offset)
            .to_string();
        let char = if start_offset == 0 {
            "".to_string()
        } else {
            self.doc
                .buffer()
                .slice_to_cow(start_offset - 1..start_offset)
                .to_string()
        };
        let completion = Arc::make_mut(&mut self.completion);
        if !display_if_empty_input && input.is_empty() && char != "." && char != ":"
        {
            completion.cancel();
            return;
        }

        if completion.status != CompletionStatus::Inactive
            && completion.offset == start_offset
            && completion.buffer_id == self.doc.id()
        {
            completion.update_input(input.clone());

            if !completion.input_items.contains_key("") {
                if let Some(start_pos) =
                    self.doc.buffer().offset_to_position(start_offset)
                {
                    let event_sink = ctx.get_external_handle();
                    completion.request(
                        self.proxy.clone(),
                        completion.request_id,
                        self.doc.id(),
                        "".to_string(),
                        start_pos,
                        completion.id,
                        event_sink,
                    );
                } else {
                    log::error!("Failed to convert start offset: {start_offset} to Position when making completion request");
                }
            }

            if !completion.input_items.contains_key(&input) {
                let event_sink = ctx.get_external_handle();
                if let Some(position) = self.doc.buffer().offset_to_position(offset)
                {
                    completion.request(
                        self.proxy.clone(),
                        completion.request_id,
                        self.doc.id(),
                        input,
                        position,
                        completion.id,
                        event_sink,
                    );
                } else {
                    log::error!("Failed to convert offset: {offset} to Position when making completion request");
                }
            }

            return;
        }

        completion.buffer_id = self.doc.id();
        completion.offset = start_offset;
        completion.input = input.clone();
        completion.status = CompletionStatus::Started;
        completion.input_items.clear();
        completion.request_id += 1;
        let event_sink = ctx.get_external_handle();
        if let Some(start_pos) = self.doc.buffer().offset_to_position(start_offset) {
            completion.request(
                self.proxy.clone(),
                completion.request_id,
                self.doc.id(),
                "".to_string(),
                start_pos,
                completion.id,
                event_sink.clone(),
            );
        }
        if !input.is_empty() {
            if let Some(position) = self.doc.buffer().offset_to_position(offset) {
                completion.request(
                    self.proxy.clone(),
                    completion.request_id,
                    self.doc.id(),
                    input,
                    position,
                    completion.id,
                    event_sink,
                );
            }
        }
    }

    /// return true if there's existing hover and it's not changed
    pub fn check_hover(
        &mut self,
        _ctx: &mut EventCtx,
        offset: usize,
        is_inside: bool,
        within_scroll: bool,
    ) -> bool {
        let hover = Arc::make_mut(&mut self.hover);

        if hover.status != HoverStatus::Inactive {
            if !is_inside || !within_scroll {
                hover.cancel();
                return false;
            }

            let start_offset = self.doc.buffer().prev_code_boundary(offset);
            if self.doc.id() == hover.buffer_id && start_offset == hover.offset {
                return true;
            }

            hover.cancel();
            return false;
        }

        false
    }

    pub fn update_hover(&mut self, ctx: &mut EventCtx, offset: usize) {
        if !self.doc.loaded() {
            return;
        }

        if !self.doc.content().is_file() {
            return;
        }

        let start_offset = self.doc.buffer().prev_code_boundary(offset);
        let end_offset = self.doc.buffer().next_code_boundary(offset);
        let input = self.doc.buffer().slice_to_cow(start_offset..end_offset);
        if input.trim().is_empty() {
            return;
        }

        // Get the diagnostics for when we make the request
        let diagnostics = self.diagnostics().map(Arc::clone);

        let mut hover = Arc::make_mut(&mut self.hover);

        if hover.status != HoverStatus::Inactive
            && hover.offset == start_offset
            && hover.buffer_id == self.doc.id()
        {
            // We're hovering over the same location, but are trying to update
            return;
        }

        hover.buffer_id = self.doc.id();
        hover.editor_view_id = self.editor.view_id;
        hover.offset = start_offset;
        hover.status = HoverStatus::Started;
        Arc::make_mut(&mut hover.items).clear();
        hover.request_id += 1;

        let event_sink = ctx.get_external_handle();
        if let Some(start_pos) = self.doc.buffer().offset_to_position(start_offset) {
            hover.request(
                self.proxy.clone(),
                hover.request_id,
                self.doc.clone(),
                diagnostics,
                start_pos,
                hover.id,
                event_sink,
                self.config.clone(),
            );
        } else {
            log::error!(
                "Failed to convert offset {start_offset} to position for hover"
            );
        }
    }

    fn update_snippet_offset(&mut self, delta: &RopeDelta) {
        if let Some(snippet) = &self.editor.snippet {
            let mut transformer = Transformer::new(delta);
            Arc::make_mut(&mut self.editor).snippet = Some(
                snippet
                    .iter()
                    .map(|(tab, (start, end))| {
                        (
                            *tab,
                            (
                                transformer.transform(*start, false),
                                transformer.transform(*end, true),
                            ),
                        )
                    })
                    .collect(),
            );
        }
    }

    fn next_diff(&mut self, ctx: &mut EventCtx) {
        if let BufferContent::File(buffer_path) = self.doc.content() {
            if self.source_control.file_diffs.is_empty() {
                return;
            }

            let buffer = self.doc.buffer();
            let mut diff_files: Vec<(PathBuf, Vec<usize>)> = self
                .source_control
                .file_diffs
                .iter()
                .map(|(diff, _)| {
                    let path = diff.path();
                    let mut positions = Vec::new();
                    if let Some(doc) = self.main_split.open_docs.get(path) {
                        if let Some(history) = doc.get_history("head") {
                            for (i, change) in history.changes().iter().enumerate() {
                                match change {
                                    DiffLines::Left(_) => {
                                        if let Some(next) =
                                            history.changes().get(i + 1)
                                        {
                                            match next {
                                                DiffLines::Right(_) => {}
                                                DiffLines::Left(_) => {}
                                                DiffLines::Both(_, r) => {
                                                    let start = buffer
                                                        .offset_of_line(r.start);
                                                    positions.push(start);
                                                }
                                                DiffLines::Skip(_, r) => {
                                                    let start = buffer
                                                        .offset_of_line(r.start);
                                                    positions.push(start);
                                                }
                                            }
                                        }
                                    }
                                    DiffLines::Both(_, _) => {}
                                    DiffLines::Skip(_, _) => {}
                                    DiffLines::Right(r) => {
                                        let start = buffer.offset_of_line(r.start);
                                        positions.push(start);
                                    }
                                }
                            }
                        }
                    }
                    if positions.is_empty() {
                        positions.push(0);
                    }
                    (path.clone(), positions)
                })
                .collect();
            diff_files.sort();

            let offset = self.editor.cursor.offset();
            let (path, offset) =
                next_in_file_diff_offset(offset, buffer_path, &diff_files);
            let location = EditorLocation {
                path,
                position: Some(offset),
                scroll_offset: None,
                history: Some("head".to_string()),
            };
            ctx.submit_command(Command::new(
                LAPCE_UI_COMMAND,
                LapceUICommand::JumpToLocation(None, location),
                Target::Widget(*self.main_split.tab_id),
            ));
        }
    }

    fn next_error(&mut self, ctx: &mut EventCtx) {
        if let BufferContent::File(buffer_path) = self.doc.content() {
            let mut file_diagnostics: Vec<(&PathBuf, Vec<Position>)> = self
                .main_split
                .diagnostics_items(DiagnosticSeverity::ERROR)
                .into_iter()
                .map(|(p, d)| {
                    (p, d.iter().map(|d| d.diagnostic.range.start).collect())
                })
                .collect();
            if file_diagnostics.is_empty() {
                return;
            }
            file_diagnostics.sort_by(|a, b| a.0.cmp(b.0));

            let offset = self.editor.cursor.offset();
            if let Some(position) = self.doc.buffer().offset_to_position(offset) {
                let (path, position) = next_in_file_errors_offset(
                    position,
                    buffer_path,
                    &file_diagnostics,
                );
                let location = EditorLocation {
                    path,
                    position: Some(position),
                    scroll_offset: None,
                    history: None,
                };
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::JumpToLspLocation(None, location),
                    Target::Auto,
                ));
            } else {
                log::error!("Failed to convert cursor offset to position when getting next error");
            }
        }
    }

    fn jump_location_forward(&mut self, ctx: &mut EventCtx) -> Option<()> {
        if self.editor.locations.is_empty() {
            return None;
        }
        if self.editor.current_location >= self.editor.locations.len() - 1 {
            return None;
        }
        let editor = Arc::make_mut(&mut self.editor);
        editor.current_location += 1;
        let location = editor.locations[editor.current_location].clone();
        ctx.submit_command(Command::new(
            LAPCE_UI_COMMAND,
            LapceUICommand::GoToLocationNew(editor.view_id, location),
            Target::Auto,
        ));
        None
    }

    fn jump_location_backward(&mut self, ctx: &mut EventCtx) -> Option<()> {
        if self.editor.current_location < 1 {
            return None;
        }
        if self.editor.current_location >= self.editor.locations.len() {
            let editor = Arc::make_mut(&mut self.editor);
            editor.save_jump_location(&self.doc);
            editor.current_location -= 1;
        }
        let editor = Arc::make_mut(&mut self.editor);
        editor.current_location -= 1;
        let location = editor.locations[editor.current_location].clone();
        ctx.submit_command(Command::new(
            LAPCE_UI_COMMAND,
            LapceUICommand::GoToLocationNew(editor.view_id, location),
            Target::Auto,
        ));
        None
    }

    fn page_move(&mut self, ctx: &mut EventCtx, down: bool, mods: Modifiers) {
        let line_height = self.config.editor.line_height as f64;
        let lines =
            (self.editor.size.borrow().height / line_height / 2.0).round() as usize;
        let distance = (lines as f64) * line_height;
        self.run_move_command(
            ctx,
            if down {
                &lapce_core::movement::Movement::Down
            } else {
                &lapce_core::movement::Movement::Up
            },
            Some(lines),
            mods,
        );
        let rect = Rect::ZERO
            .with_origin(
                self.editor.scroll_offset.to_point()
                    + Vec2::new(0.0, if down { distance } else { -distance }),
            )
            .with_size(*self.editor.size.borrow());
        ctx.submit_command(Command::new(
            LAPCE_UI_COMMAND,
            LapceUICommand::EnsureRectVisible(rect),
            Target::Widget(self.editor.view_id),
        ));
    }

    fn scroll(
        &mut self,
        ctx: &mut EventCtx,
        down: bool,
        count: usize,
        mods: Modifiers,
    ) {
        let line_height = self.config.editor.line_height as f64;
        let diff = line_height * count as f64;
        let diff = if down { diff } else { -diff };

        let offset = self.editor.cursor.offset();
        let (line, _col) = self.doc.buffer().offset_to_line_col(offset);
        let top = self.editor.scroll_offset.y + diff;
        let bottom = top + self.editor.size.borrow().height;

        let new_line = if (line + 1) as f64 * line_height + line_height > bottom {
            let line = (bottom / line_height).floor() as usize;
            if line > 2 {
                line - 2
            } else {
                0
            }
        } else if line as f64 * line_height - line_height < top {
            let line = (top / line_height).ceil() as usize;
            line + 1
        } else {
            line
        };

        match new_line.cmp(&line) {
            Ordering::Greater => {
                self.run_move_command(
                    ctx,
                    &lapce_core::movement::Movement::Down,
                    Some(new_line - line),
                    mods,
                );
            }
            Ordering::Less => {
                self.run_move_command(
                    ctx,
                    &lapce_core::movement::Movement::Up,
                    Some(line - new_line),
                    mods,
                );
            }
            _ => (),
        };

        ctx.submit_command(Command::new(
            LAPCE_UI_COMMAND,
            LapceUICommand::ScrollTo((self.editor.scroll_offset.x, top)),
            Target::Widget(self.editor.view_id),
        ));
    }

    pub fn current_code_actions(&self) -> Option<&CodeActionResponse> {
        let offset = self.editor.cursor.offset();
        let prev_offset = self.doc.buffer().prev_code_boundary(offset);
        self.doc.code_actions.get(&prev_offset)
    }

    pub fn diagnostics(&self) -> Option<&Arc<Vec<EditorDiagnostic>>> {
        self.doc.diagnostics.as_ref()
    }

    pub fn offset_of_mouse(
        &self,
        text: &mut PietText,
        pos: Point,
        config: &Config,
    ) -> usize {
        let (line, char_width) = if self.editor.is_code_lens() {
            let (line, font_size) = if let Some(syntax) = self.doc.syntax() {
                let line = syntax.lens.line_of_height(pos.y.floor() as usize);
                let line_height = syntax.lens.height_of_line(line + 1)
                    - syntax.lens.height_of_line(line);

                let font_size = if line_height < config.editor.line_height {
                    config.editor.code_lens_font_size
                } else {
                    config.editor.font_size
                };

                (line, font_size)
            } else {
                (
                    (pos.y / config.editor.code_lens_font_size as f64).floor()
                        as usize,
                    config.editor.code_lens_font_size,
                )
            };

            (line, config.char_width(text, font_size as f64))
        } else if let Some(compare) = self.editor.compare.as_ref() {
            let line = (pos.y / config.editor.line_height as f64).floor() as usize;
            let line = self.doc.history_actual_line_from_visual(compare, line);
            (line, config.editor_char_width(text))
        } else {
            let line = (pos.y / config.editor.line_height as f64).floor() as usize;
            (line, config.editor_char_width(text))
        };

        let last_line = self.doc.buffer().last_line();
        let (line, col) = if line > last_line {
            (last_line, 0)
        } else {
            let line_end = self
                .doc
                .buffer()
                .line_end_col(line, self.editor.cursor.get_mode() != Mode::Normal);

            let col = (if self.editor.cursor.get_mode() == Mode::Insert {
                (pos.x / char_width).round() as usize
            } else {
                (pos.x / char_width).floor() as usize
            })
            .min(line_end);
            (line, col)
        };
        self.doc.buffer().offset_of_line_col(line, col)
    }

    pub fn single_click(
        &mut self,
        ctx: &mut EventCtx,
        mouse_event: &MouseEvent,
        config: &Config,
    ) {
        let (new_offset, _) = self.doc.offset_of_point(
            ctx.text(),
            self.get_mode(),
            mouse_event.pos,
            &self.editor.view,
            config,
        );
        let cursor = &mut Arc::make_mut(&mut self.editor).cursor;
        cursor.set_offset(
            new_offset,
            mouse_event.mods.shift(),
            mouse_event.mods.alt(),
        );

        let mut go_to_definition = false;
        #[cfg(target_os = "macos")]
        if mouse_event.mods.meta() {
            go_to_definition = true;
        }
        #[cfg(not(target_os = "macos"))]
        if mouse_event.mods.ctrl() {
            go_to_definition = true;
        }

        if go_to_definition {
            ctx.submit_command(Command::new(
                LAPCE_COMMAND,
                LapceCommand {
                    kind: CommandKind::Focus(FocusCommand::GotoDefinition),
                    data: None,
                },
                Target::Widget(self.editor.view_id),
            ));
        } else if mouse_event.buttons.has_left() {
            ctx.set_active(true);
        }
    }

    pub fn double_click(
        &mut self,
        ctx: &mut EventCtx,
        mouse_event: &MouseEvent,
        config: &Config,
    ) {
        ctx.set_active(true);
        let (mouse_offset, _) = self.doc.offset_of_point(
            ctx.text(),
            self.get_mode(),
            mouse_event.pos,
            &self.editor.view,
            config,
        );
        let (start, end) = self.doc.buffer().select_word(mouse_offset);
        let cursor = &mut Arc::make_mut(&mut self.editor).cursor;
        cursor.add_region(
            start,
            end,
            mouse_event.mods.shift(),
            mouse_event.mods.alt(),
        );
    }

    pub fn triple_click(
        &mut self,
        ctx: &mut EventCtx,
        mouse_event: &MouseEvent,
        config: &Config,
    ) {
        ctx.set_active(true);
        let (mouse_offset, _) = self.doc.offset_of_point(
            ctx.text(),
            self.get_mode(),
            mouse_event.pos,
            &self.editor.view,
            config,
        );
        let line = self.doc.buffer().line_of_offset(mouse_offset);
        let start = self.doc.buffer().offset_of_line(line);
        let end = self.doc.buffer().offset_of_line(line + 1);
        let cursor = &mut Arc::make_mut(&mut self.editor).cursor;
        cursor.add_region(
            start,
            end,
            mouse_event.mods.shift(),
            mouse_event.mods.alt(),
        );
    }

    fn apply_deltas(&mut self, deltas: &[(RopeDelta, InvalLines)]) {
        for (delta, _) in deltas {
            self.inactive_apply_delta(delta);
            self.update_snippet_offset(delta);
        }
    }

    fn save(&mut self, ctx: &mut EventCtx, exit: bool) {
        if self.doc.buffer().is_pristine() && self.doc.content().is_file() {
            if exit {
                ctx.submit_command(Command::new(
                    LAPCE_COMMAND,
                    LapceCommand {
                        kind: CommandKind::Focus(FocusCommand::SplitClose),
                        data: None,
                    },
                    Target::Widget(self.editor.view_id),
                ));
            }
            return;
        }

        if let BufferContent::File(path) = self.doc.content() {
            let format_on_save = self.config.editor.format_on_save;
            let path = path.clone();
            let proxy = self.proxy.clone();
            let buffer_id = self.doc.id();
            let rev = self.doc.rev();
            let event_sink = ctx.get_external_handle();
            let view_id = self.editor.view_id;
            let tab_id = self.main_split.tab_id.clone();
            let (sender, receiver) = bounded(1);
            thread::spawn(move || {
                proxy.get_document_formatting(
                    buffer_id,
                    Box::new(move |result| {
                        let _ = sender.send(result);
                    }),
                );

                let result =
                    receiver.recv_timeout(Duration::from_secs(1)).map_or_else(
                        |e| Err(anyhow!("{}", e)),
                        |v| v.map_err(|e| anyhow!("{:?}", e)),
                    );

                let exit = if exit { Some(view_id) } else { None };
                let cmd = if format_on_save {
                    LapceUICommand::DocumentFormatAndSave(path, rev, result, exit)
                } else {
                    LapceUICommand::DocumentSave(path, exit)
                };

                let _ = event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    cmd,
                    Target::Widget(*tab_id),
                );
            });
        } else if let BufferContent::Scratch(..) = self.doc.content() {
            let content = self.doc.content().clone();
            let view_id = self.editor.view_id;
            self.main_split.current_save_as =
                Some(Arc::new((content, view_id, exit)));
            let options =
                FileDialogOptions::new().accept_command(LAPCE_SAVE_FILE_AS);
            ctx.submit_command(druid::commands::SHOW_SAVE_PANEL.with(options));
        }
    }

    fn run_move_command(
        &mut self,
        ctx: &mut EventCtx,
        movement: &lapce_core::movement::Movement,
        count: Option<usize>,
        mods: Modifiers,
    ) -> CommandExecuted {
        if movement.is_jump() && movement != &self.editor.last_movement_new {
            Arc::make_mut(&mut self.editor).save_jump_location(&self.doc);
        }
        Arc::make_mut(&mut self.editor).last_movement_new = movement.clone();

        let register = Arc::make_mut(&mut self.main_split.register);
        let doc = Arc::make_mut(&mut self.doc);
        let view = self.editor.view.clone();
        doc.move_cursor(
            ctx.text(),
            &mut Arc::make_mut(&mut self.editor).cursor,
            movement,
            count.unwrap_or(1),
            mods.shift(),
            &view,
            register,
            &self.config,
        );
        if let Some(snippet) = self.editor.snippet.as_ref() {
            let offset = self.editor.cursor.offset();
            let mut within_region = false;
            for (_, (start, end)) in snippet {
                if offset >= *start && offset <= *end {
                    within_region = true;
                    break;
                }
            }
            if !within_region {
                Arc::make_mut(&mut self.editor).snippet = None;
            }
        }
        self.cancel_completion();
        self.cancel_hover();
        CommandExecuted::Yes
    }

    fn run_edit_command(
        &mut self,
        ctx: &mut EventCtx,
        cmd: &EditCommand,
    ) -> CommandExecuted {
        let modal = self.config.lapce.modal && !self.editor.content.is_input();
        let doc = Arc::make_mut(&mut self.doc);
        let register = Arc::make_mut(&mut self.main_split.register);
        let cursor = &mut Arc::make_mut(&mut self.editor).cursor;
        let yank_data =
            if let lapce_core::cursor::CursorMode::Visual { .. } = &cursor.mode {
                Some(cursor.yank(doc.buffer()))
            } else {
                None
            };

        let deltas = doc.do_edit(cursor, cmd, modal, register);

        if !deltas.is_empty() {
            if let Some(data) = yank_data {
                register.add_delete(data);
            }
        }

        match cmd {
            EditCommand::InsertNewLine
            | EditCommand::InsertTab
            | EditCommand::NewLineAbove
            | EditCommand::NewLineBelow => {}
            _ => self.update_completion(ctx, false),
        }
        self.apply_deltas(&deltas);

        CommandExecuted::Yes
    }

    fn run_focus_command(
        &mut self,
        ctx: &mut EventCtx,
        cmd: &FocusCommand,
        count: Option<usize>,
        mods: Modifiers,
    ) -> CommandExecuted {
        use FocusCommand::*;
        match cmd {
            ModalClose => {
                if self.is_palette() {
                    ctx.submit_command(Command::new(
                        LAPCE_COMMAND,
                        LapceCommand {
                            kind: CommandKind::Focus(FocusCommand::ModalClose),
                            data: None,
                        },
                        Target::Widget(self.palette.widget_id),
                    ));
                }
                if self.has_completions() {
                    self.cancel_completion();
                }
                if self.has_hover() {
                    self.cancel_hover();
                }
            }
            SplitVertical => {
                self.main_split.split_editor(
                    ctx,
                    Arc::make_mut(&mut self.editor),
                    SplitDirection::Vertical,
                    &self.config,
                );
            }
            SplitHorizontal => {
                self.main_split.split_editor(
                    ctx,
                    Arc::make_mut(&mut self.editor),
                    SplitDirection::Horizontal,
                    &self.config,
                );
            }
            SplitExchange => {
                if let Some(widget_id) = self.editor.tab_id.as_ref() {
                    self.main_split
                        .split_exchange(ctx, SplitContent::EditorTab(*widget_id));
                }
            }
            SplitLeft => {
                if let Some(widget_id) = self.editor.tab_id.as_ref() {
                    self.main_split.split_move(
                        ctx,
                        SplitContent::EditorTab(*widget_id),
                        SplitMoveDirection::Left,
                    );
                }
            }
            SplitRight => {
                if let Some(widget_id) = self.editor.tab_id.as_ref() {
                    self.main_split.split_move(
                        ctx,
                        SplitContent::EditorTab(*widget_id),
                        SplitMoveDirection::Right,
                    );
                }
            }
            SplitUp => {
                if let Some(widget_id) = self.editor.tab_id.as_ref() {
                    self.main_split.split_move(
                        ctx,
                        SplitContent::EditorTab(*widget_id),
                        SplitMoveDirection::Up,
                    );
                }
            }
            SplitDown => {
                if let Some(widget_id) = self.editor.tab_id.as_ref() {
                    self.main_split.split_move(
                        ctx,
                        SplitContent::EditorTab(*widget_id),
                        SplitMoveDirection::Down,
                    );
                }
            }
            SplitClose => {
                self.main_split.editor_close(ctx, self.view_id, false);
            }
            ForceExit => {
                self.main_split.editor_close(ctx, self.view_id, true);
            }
            SearchWholeWordForward => {
                Arc::make_mut(&mut self.find).visual = true;
                let offset = self.editor.cursor.offset();
                let (start, end) = self.doc.buffer().select_word(offset);
                let word = self.doc.buffer().slice_to_cow(start..end).to_string();
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::UpdateSearchInput(word.clone()),
                    Target::Widget(*self.main_split.tab_id),
                ));
                Arc::make_mut(&mut self.find).set_find(&word, false, false, true);
                let next =
                    self.find
                        .next(self.doc.buffer().text(), offset, false, true);
                if let Some((start, _end)) = next {
                    self.run_move_command(
                        ctx,
                        &lapce_core::movement::Movement::Offset(start),
                        None,
                        mods,
                    );
                }
            }
            SearchForward => {
                if self.editor.content.is_search() {
                    if let Some(parent_view_id) = self.editor.parent_view_id {
                        ctx.submit_command(Command::new(
                            LAPCE_COMMAND,
                            LapceCommand {
                                kind: CommandKind::Focus(
                                    FocusCommand::SearchForward,
                                ),
                                data: None,
                            },
                            Target::Widget(parent_view_id),
                        ));
                    }
                } else {
                    Arc::make_mut(&mut self.find).visual = true;
                    let offset = self.editor.cursor.offset();
                    let next = self.find.next(
                        self.doc.buffer().text(),
                        offset,
                        false,
                        true,
                    );
                    if let Some((start, _end)) = next {
                        self.run_move_command(
                            ctx,
                            &lapce_core::movement::Movement::Offset(start),
                            None,
                            mods,
                        );
                    }
                }
            }
            SearchBackward => {
                if self.editor.content.is_search() {
                    if let Some(parent_view_id) = self.editor.parent_view_id {
                        ctx.submit_command(Command::new(
                            LAPCE_COMMAND,
                            LapceCommand {
                                kind: CommandKind::Focus(
                                    FocusCommand::SearchBackward,
                                ),
                                data: None,
                            },
                            Target::Widget(parent_view_id),
                        ));
                    }
                } else {
                    Arc::make_mut(&mut self.find).visual = true;
                    let offset = self.editor.cursor.offset();
                    let next =
                        self.find.next(self.doc.buffer().text(), offset, true, true);
                    if let Some((start, _end)) = next {
                        self.run_move_command(
                            ctx,
                            &lapce_core::movement::Movement::Offset(start),
                            None,
                            mods,
                        );
                    }
                }
            }
            GlobalSearchRefresh => {
                let tab_id = *self.main_split.tab_id;
                let pattern = self.doc.buffer().text().to_string();
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::UpdateSearch(pattern),
                    Target::Widget(tab_id),
                ));
            }
            ClearSearch => {
                Arc::make_mut(&mut self.find).visual = false;
                let view_id =
                    if let Some(parent_view_id) = self.editor.parent_view_id {
                        parent_view_id
                    } else if self.editor.content.is_search() {
                        (*self.main_split.active).unwrap_or(self.editor.view_id)
                    } else {
                        self.editor.view_id
                    };
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::Focus,
                    Target::Widget(view_id),
                ));
            }
            SearchInView => {
                let start_line = ((self.editor.scroll_offset.y
                    / self.config.editor.line_height as f64)
                    .ceil() as usize)
                    .max(self.doc.buffer().last_line());
                let end_line = ((self.editor.scroll_offset.y
                    + self.editor.size.borrow().height
                        / self.config.editor.line_height as f64)
                    .ceil() as usize)
                    .max(self.doc.buffer().last_line());
                let end_offset = self.doc.buffer().offset_of_line(end_line + 1);

                let offset = self.editor.cursor.offset();
                let line = self.doc.buffer().line_of_offset(offset);
                let offset = self.doc.buffer().offset_of_line(line);
                let next =
                    self.find
                        .next(self.doc.buffer().text(), offset, false, false);

                if let Some(start) = next
                    .map(|(start, _)| start)
                    .filter(|start| *start < end_offset)
                {
                    self.run_move_command(
                        ctx,
                        &lapce_core::movement::Movement::Offset(start),
                        None,
                        mods,
                    );
                } else {
                    let start_offset = self.doc.buffer().offset_of_line(start_line);
                    if let Some((start, _)) = self.find.next(
                        self.doc.buffer().text(),
                        start_offset,
                        false,
                        true,
                    ) {
                        self.run_move_command(
                            ctx,
                            &lapce_core::movement::Movement::Offset(start),
                            None,
                            mods,
                        );
                    }
                }
            }
            ListSelect => {
                if self.is_palette() {
                    ctx.submit_command(Command::new(
                        LAPCE_COMMAND,
                        LapceCommand {
                            kind: CommandKind::Focus(FocusCommand::ListSelect),
                            data: None,
                        },
                        Target::Widget(self.palette.widget_id),
                    ));
                } else {
                    let item = self.completion.current_item().to_owned();
                    self.cancel_completion();
                    if item.data.is_some() {
                        let view_id = self.editor.view_id;
                        let buffer_id = self.doc.id();
                        let rev = self.doc.rev();
                        let offset = self.editor.cursor.offset();
                        let event_sink = ctx.get_external_handle();
                        self.proxy.completion_resolve(
                            buffer_id,
                            item.clone(),
                            move |result| {
                                let item = result.unwrap_or_else(|_| item.clone());
                                let _ = event_sink.submit_command(
                                    LAPCE_UI_COMMAND,
                                    LapceUICommand::ResolveCompletion(
                                        buffer_id,
                                        rev,
                                        offset,
                                        Box::new(item),
                                    ),
                                    Target::Widget(view_id),
                                );
                            },
                        );
                    } else {
                        let _ = self.apply_completion_item(&item);
                    }
                }
            }
            ListNext => {
                if self.is_palette() {
                    ctx.submit_command(Command::new(
                        LAPCE_COMMAND,
                        LapceCommand {
                            kind: CommandKind::Focus(FocusCommand::ListNext),
                            data: None,
                        },
                        Target::Widget(self.palette.widget_id),
                    ));
                } else {
                    let completion = Arc::make_mut(&mut self.completion);
                    completion.next();
                }
            }
            ListNextPage => {
                if self.is_palette() {
                    ctx.submit_command(Command::new(
                        LAPCE_COMMAND,
                        LapceCommand {
                            kind: CommandKind::Focus(FocusCommand::ListNextPage),
                            data: None,
                        },
                        Target::Widget(self.palette.widget_id),
                    ));
                } else {
                    let completion = Arc::make_mut(&mut self.completion);
                    completion.next_page(self.config.editor.line_height);
                }
            }
            ListPrevious => {
                if self.is_palette() {
                    ctx.submit_command(Command::new(
                        LAPCE_COMMAND,
                        LapceCommand {
                            kind: CommandKind::Focus(FocusCommand::ListPrevious),
                            data: None,
                        },
                        Target::Widget(self.palette.widget_id),
                    ));
                } else {
                    let completion = Arc::make_mut(&mut self.completion);
                    completion.previous();
                }
            }
            ListPreviousPage => {
                if self.is_palette() {
                    ctx.submit_command(Command::new(
                        LAPCE_COMMAND,
                        LapceCommand {
                            kind: CommandKind::Focus(FocusCommand::ListPreviousPage),
                            data: None,
                        },
                        Target::Widget(self.palette.widget_id),
                    ));
                } else {
                    let completion = Arc::make_mut(&mut self.completion);
                    completion.previous_page(self.config.editor.line_height);
                }
            }
            JumpToNextSnippetPlaceholder => {
                if let Some(snippet) = self.editor.snippet.as_ref() {
                    let mut current = 0;
                    let offset = self.editor.cursor.offset();
                    for (i, (_, (start, end))) in snippet.iter().enumerate() {
                        if *start <= offset && offset <= *end {
                            current = i;
                            break;
                        }
                    }

                    let last_placeholder = current + 1 >= snippet.len() - 1;

                    if let Some((_, (start, end))) = snippet.get(current + 1) {
                        let mut selection = lapce_core::selection::Selection::new();
                        let region = lapce_core::selection::SelRegion::new(
                            *start, *end, None,
                        );
                        selection.add_region(region);
                        Arc::make_mut(&mut self.editor).cursor.set_insert(selection);
                    }

                    if last_placeholder {
                        Arc::make_mut(&mut self.editor).snippet = None;
                    }
                    self.cancel_completion();
                }
            }
            JumpToPrevSnippetPlaceholder => {
                if let Some(snippet) = self.editor.snippet.as_ref() {
                    let mut current = 0;
                    let offset = self.editor.cursor.offset();
                    for (i, (_, (start, end))) in snippet.iter().enumerate() {
                        if *start <= offset && offset <= *end {
                            current = i;
                            break;
                        }
                    }

                    if current > 0 {
                        if let Some((_, (start, end))) = snippet.get(current - 1) {
                            let mut selection =
                                lapce_core::selection::Selection::new();
                            let region = lapce_core::selection::SelRegion::new(
                                *start, *end, None,
                            );
                            selection.add_region(region);
                            Arc::make_mut(&mut self.editor)
                                .cursor
                                .set_insert(selection);
                        }
                        self.cancel_completion();
                    }
                }
            }
            PageUp => {
                self.page_move(ctx, false, mods);
            }
            PageDown => {
                self.page_move(ctx, true, mods);
            }
            ScrollUp => {
                self.scroll(ctx, false, count.unwrap_or(1), mods);
            }
            ScrollDown => {
                self.scroll(ctx, true, count.unwrap_or(1), mods);
            }
            CenterOfWindow => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::EnsureCursorPosition(
                        EnsureVisiblePosition::CenterOfWindow,
                    ),
                    Target::Widget(self.editor.view_id),
                ));
            }
            TopOfWindow => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::EnsureCursorPosition(
                        EnsureVisiblePosition::TopOfWindow,
                    ),
                    Target::Widget(self.editor.view_id),
                ));
            }
            BottomOfWindow => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::EnsureCursorPosition(
                        EnsureVisiblePosition::BottomOfWindow,
                    ),
                    Target::Widget(self.editor.view_id),
                ));
            }
            ShowCodeActions => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::ShowCodeActions(None),
                    Target::Widget(self.editor.editor_id),
                ));
            }
            GetCompletion => {
                // we allow empty inputs to allow for cases where the user wants to get the autocompletion beforehand
                self.update_completion(ctx, true);
            }
            GotoDefinition => {
                let offset = self.editor.cursor.offset();
                let start_offset = self.doc.buffer().prev_code_boundary(offset);
                let start_position = if let Some(start_position) =
                    self.doc.buffer().offset_to_position(start_offset)
                {
                    start_position
                } else {
                    log::error!("Failed to convert offset {start_offset} to position in GotoDefinition");
                    return CommandExecuted::Yes;
                };
                let event_sink = ctx.get_external_handle();
                let buffer_id = self.doc.id();
                let position = if let Some(position) =
                    self.doc.buffer().offset_to_position(offset)
                {
                    position
                } else {
                    log::error!("Failed to convert offset {offset} to position in GotoDefinition");
                    return CommandExecuted::Yes;
                };
                let proxy = self.proxy.clone();
                let editor_view_id = self.editor.view_id;
                self.proxy.get_definition(
                    offset,
                    buffer_id,
                    position,
                    move |result| {
                        if let Ok(resp) = result {
                            if let Some(location) = match resp {
                                GotoDefinitionResponse::Scalar(location) => {
                                    Some(location)
                                }
                                GotoDefinitionResponse::Array(locations) => {
                                    if !locations.is_empty() {
                                        Some(locations[0].clone())
                                    } else {
                                        None
                                    }
                                }
                                GotoDefinitionResponse::Link(_location_links) => {
                                    None
                                }
                            } {
                                if location.range.start == start_position {
                                    proxy.get_references(
                                        buffer_id,
                                        position,
                                        move |result| {
                                            let _ = process_get_references(
                                                offset, result, event_sink,
                                            );
                                        },
                                    );
                                } else {
                                    let _ = event_sink.submit_command(
                                        LAPCE_UI_COMMAND,
                                        LapceUICommand::GotoDefinition {
                                            editor_view_id,
                                            offset,
                                            location: EditorLocation {
                                                path: path_from_url(&location.uri),
                                                position: Some(location.range.start),
                                                scroll_offset: None,
                                                history: None,
                                            },
                                        },
                                        Target::Auto,
                                    );
                                }
                            }
                        }
                    },
                );
            }
            GotoTypeDefinition => {
                let offset = self.editor.cursor.offset();
                let event_sink = ctx.get_external_handle();
                let buffer_id = self.doc.id();
                let position = if let Some(position) =
                    self.doc.buffer().offset_to_position(offset)
                {
                    position
                } else {
                    log::error!("Failed to convert offset {offset} to position in GotoTypeDefinition");
                    return CommandExecuted::Yes;
                };
                let editor_view_id = self.editor.view_id;
                self.proxy.get_type_definition(
                    offset,
                    buffer_id,
                    position,
                    move |result| {
                        if let Ok(resp) = result {
                            match resp {
                                GotoTypeDefinitionResponse::Scalar(location) => {
                                    let _ = event_sink.submit_command(
                                        LAPCE_UI_COMMAND,
                                        LapceUICommand::GotoDefinition {
                                            editor_view_id,
                                            offset,
                                            location: EditorLocation {
                                                path: path_from_url(&location.uri),
                                                position: Some(location.range.start),
                                                scroll_offset: None,
                                                history: None,
                                            },
                                        },
                                        Target::Auto,
                                    );
                                }
                                GotoTypeDefinitionResponse::Array(locations) => {
                                    let len = locations.len();
                                    match len {
                                        1 => {
                                            let _ = event_sink.submit_command(
                                                LAPCE_UI_COMMAND,
                                                LapceUICommand::GotoDefinition {
                                                    editor_view_id,
                                                    offset,
                                                    location: EditorLocation {
                                                        path: path_from_url(
                                                            &locations[0].uri,
                                                        ),
                                                        position: Some(
                                                            locations[0].range.start,
                                                        ),
                                                        scroll_offset: None,
                                                        history: None,
                                                    },
                                                },
                                                Target::Auto,
                                            );
                                        }
                                        _ if len > 1 => {
                                            let _ = event_sink.submit_command(
                                                LAPCE_UI_COMMAND,
                                                LapceUICommand::PaletteReferences(
                                                    offset, locations,
                                                ),
                                                Target::Auto,
                                            );
                                        }
                                        _ => (),
                                    }
                                }
                                GotoTypeDefinitionResponse::Link(
                                    _location_links,
                                ) => {}
                            }
                        }
                    },
                );
            }
            JumpLocationBackward => {
                self.jump_location_backward(ctx);
            }
            JumpLocationForward => {
                self.jump_location_forward(ctx);
            }
            NextError => {
                self.next_error(ctx);
            }
            NextDiff => {
                self.next_diff(ctx);
            }
            ToggleCodeLens => {
                let editor = Arc::make_mut(&mut self.editor);
                editor.view = match editor.view {
                    EditorView::Normal => EditorView::Lens,
                    EditorView::Lens => EditorView::Normal,
                    EditorView::Diff(_) => return CommandExecuted::Yes,
                };
            }
            FormatDocument => {
                if let BufferContent::File(path) = self.doc.content() {
                    let path = path.clone();
                    let proxy = self.proxy.clone();
                    let buffer_id = self.doc.id();
                    let rev = self.doc.rev();
                    let event_sink = ctx.get_external_handle();
                    let (sender, receiver) = bounded(1);
                    let tab_id = self.main_split.tab_id.clone();
                    thread::spawn(move || {
                        proxy.get_document_formatting(
                            buffer_id,
                            Box::new(move |result| {
                                let _ = sender.send(result);
                            }),
                        );

                        let result = receiver
                            .recv_timeout(Duration::from_secs(1))
                            .map_or_else(
                                |e| Err(anyhow!("{}", e)),
                                |v| v.map_err(|e| anyhow!("{:?}", e)),
                            );
                        let _ = event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::DocumentFormat(path, rev, result),
                            Target::Widget(*tab_id),
                        );
                    });
                }
            }
            Search => {
                Arc::make_mut(&mut self.find).visual = true;
                let region = match &self.editor.cursor.mode {
                    lapce_core::cursor::CursorMode::Normal(offset) => {
                        lapce_core::selection::SelRegion::caret(*offset)
                    }
                    lapce_core::cursor::CursorMode::Visual {
                        start,
                        end,
                        mode: _,
                    } => lapce_core::selection::SelRegion::new(
                        *start.min(end),
                        self.doc.buffer().next_grapheme_offset(
                            *start.max(end),
                            1,
                            self.doc.buffer().len(),
                        ),
                        None,
                    ),
                    lapce_core::cursor::CursorMode::Insert(selection) => {
                        *selection.last_inserted().unwrap()
                    }
                };
                let pattern = if region.is_caret() {
                    let (start, end) = self.doc.buffer().select_word(region.start);
                    self.doc.buffer().slice_to_cow(start..end).to_string()
                } else {
                    self.doc
                        .buffer()
                        .slice_to_cow(region.min()..region.max())
                        .to_string()
                };
                if !pattern.contains('\n') {
                    Arc::make_mut(&mut self.find)
                        .set_find(&pattern, false, false, false);
                    ctx.submit_command(Command::new(
                        LAPCE_UI_COMMAND,
                        LapceUICommand::UpdateSearchInput(pattern),
                        Target::Widget(*self.main_split.tab_id),
                    ));
                }
                if let Some((find_view_id, _)) = self.editor.find_view_id {
                    ctx.submit_command(Command::new(
                        LAPCE_COMMAND,
                        LapceCommand {
                            kind: CommandKind::MultiSelection(
                                MultiSelectionCommand::SelectAll,
                            ),
                            data: None,
                        },
                        Target::Widget(find_view_id),
                    ));
                    ctx.submit_command(Command::new(
                        LAPCE_UI_COMMAND,
                        LapceUICommand::Focus,
                        Target::Widget(find_view_id),
                    ));
                }
            }
            InlineFindLeft => {
                Arc::make_mut(&mut self.editor).inline_find =
                    Some(InlineFindDirection::Left);
            }
            InlineFindRight => {
                Arc::make_mut(&mut self.editor).inline_find =
                    Some(InlineFindDirection::Right);
            }
            RepeatLastInlineFind => {
                if let Some((direction, c)) = self.editor.last_inline_find.clone() {
                    self.inline_find(ctx, direction, &c);
                }
            }
            SaveAndExit => {
                self.save(ctx, true);
            }
            Save => {
                self.save(ctx, false);
            }
            _ => return CommandExecuted::No,
        }
        CommandExecuted::Yes
    }

    fn run_motion_mode_command(
        &mut self,
        _ctx: &mut EventCtx,
        cmd: &MotionModeCommand,
    ) -> CommandExecuted {
        let motion_mode = match cmd {
            MotionModeCommand::MotionModeDelete => MotionMode::Delete,
            MotionModeCommand::MotionModeIndent => MotionMode::Indent,
            MotionModeCommand::MotionModeOutdent => MotionMode::Outdent,
            MotionModeCommand::MotionModeYank => MotionMode::Yank,
        };
        let cursor = &mut Arc::make_mut(&mut self.editor).cursor;
        let doc = Arc::make_mut(&mut self.doc);
        let register = Arc::make_mut(&mut self.main_split.register);
        doc.do_motion_mode(cursor, motion_mode, register);
        CommandExecuted::Yes
    }

    fn run_multi_selection_command(
        &mut self,
        ctx: &mut EventCtx,
        cmd: &MultiSelectionCommand,
    ) -> CommandExecuted {
        let view = self.editor.view.clone();
        let cursor = &mut Arc::make_mut(&mut self.editor).cursor;
        self.doc
            .do_multi_selection(ctx.text(), cursor, cmd, &view, &self.config);
        self.cancel_completion();
        CommandExecuted::Yes
    }
}

impl KeyPressFocus for LapceEditorBufferData {
    fn get_mode(&self) -> Mode {
        self.editor.cursor.get_mode()
    }

    fn focus_only(&self) -> bool {
        self.editor.content.is_settings()
    }

    fn expect_char(&self) -> bool {
        self.editor.inline_find.is_some()
    }

    fn check_condition(&self, condition: &str) -> bool {
        match condition {
            "search_focus" => {
                self.editor.content == BufferContent::Local(LocalBufferKind::Search)
                    && self.editor.parent_view_id.is_some()
            }
            "global_search_focus" => {
                self.editor.content == BufferContent::Local(LocalBufferKind::Search)
                    && self.editor.parent_view_id.is_none()
            }
            "input_focus" => self.editor.content.is_input(),
            "editor_focus" => match self.editor.content {
                BufferContent::File(_) => true,
                BufferContent::Scratch(..) => true,
                BufferContent::Local(_) => false,
                BufferContent::SettingsValue(..) => false,
            },
            "diff_focus" => self.editor.compare.is_some(),
            "source_control_focus" => {
                self.editor.content
                    == BufferContent::Local(LocalBufferKind::SourceControl)
            }
            "in_snippet" => self.editor.snippet.is_some(),
            "completion_focus" => self.has_completions(),
            "hover_focus" => self.has_hover(),
            "list_focus" => self.has_completions() || self.is_palette(),
            "modal_focus" => {
                (self.has_completions() && !self.config.lapce.modal)
                    || self.has_hover()
                    || self.is_palette()
            }
            _ => false,
        }
    }

    fn receive_char(&mut self, ctx: &mut EventCtx, c: &str) {
        if self.get_mode() == Mode::Insert {
            let doc = Arc::make_mut(&mut self.doc);
            let cursor = &mut Arc::make_mut(&mut self.editor).cursor;
            let deltas = doc.do_insert(cursor, c);

            if !c
                .chars()
                .all(|c| c.is_whitespace() || c.is_ascii_whitespace())
            {
                self.update_completion(ctx, false);
            }
            self.cancel_hover();
            self.apply_deltas(&deltas);
        } else if let Some(direction) = self.editor.inline_find.clone() {
            self.inline_find(ctx, direction.clone(), c);
            let editor = Arc::make_mut(&mut self.editor);
            editor.last_inline_find = Some((direction, c.to_string()));
            editor.inline_find = None;
        }
    }

    fn run_command(
        &mut self,
        ctx: &mut EventCtx,
        command: &LapceCommand,
        count: Option<usize>,
        mods: Modifiers,
        _env: &Env,
    ) -> CommandExecuted {
        let old_doc = self.doc.clone();
        let executed = match &command.kind {
            CommandKind::Edit(cmd) => self.run_edit_command(ctx, cmd),
            CommandKind::Move(cmd) => {
                let movement = cmd.to_movement(count);
                self.run_move_command(ctx, &movement, count, mods)
            }
            CommandKind::Focus(cmd) => self.run_focus_command(ctx, cmd, count, mods),
            CommandKind::MotionMode(cmd) => self.run_motion_mode_command(ctx, cmd),
            CommandKind::MultiSelection(cmd) => {
                self.run_multi_selection_command(ctx, cmd)
            }
            CommandKind::Workbench(_) => CommandExecuted::No,
        };
        let doc = self.doc.clone();
        if doc.content() != old_doc.content() || doc.rev() != old_doc.rev() {
            Arc::make_mut(&mut self.editor)
                .cursor
                .history_selections
                .clear();
        }

        executed
    }
}

#[derive(Clone)]
pub struct TabRect {
    pub svg: Svg,
    pub rect: Rect,
    pub close_rect: Rect,
    pub text_layout: PietTextLayout,
}

#[derive(Clone)]
pub struct HighlightTextLayout {
    pub layout: PietTextLayout,
    pub text: String,
    pub highlights: Vec<(usize, usize, String)>,
}

fn next_in_file_diff_offset(
    offset: usize,
    path: &Path,
    file_diffs: &[(PathBuf, Vec<usize>)],
) -> (PathBuf, usize) {
    for (current_path, offsets) in file_diffs {
        if path == current_path {
            for diff_offset in offsets {
                if *diff_offset > offset {
                    return ((*current_path).clone(), *diff_offset);
                }
            }
        }
        if current_path > path {
            return ((*current_path).clone(), offsets[0]);
        }
    }
    ((file_diffs[0].0).clone(), file_diffs[0].1[0])
}

fn next_in_file_errors_offset(
    position: Position,
    path: &Path,
    file_diagnostics: &[(&PathBuf, Vec<Position>)],
) -> (PathBuf, Position) {
    for (current_path, positions) in file_diagnostics {
        if &path == current_path {
            for error_position in positions {
                if error_position.line > position.line
                    || (error_position.line == position.line
                        && error_position.character > position.character)
                {
                    return ((*current_path).clone(), *error_position);
                }
            }
        }
        if current_path > &path {
            return ((*current_path).clone(), positions[0]);
        }
    }
    ((*file_diagnostics[0].0).clone(), file_diagnostics[0].1[0])
}

fn process_get_references(
    offset: usize,
    result: Result<Vec<Location>, RequestError>,
    event_sink: ExtEventSink,
) -> Result<()> {
    let locations = result.map_err(|e| anyhow!("{:?}", e))?;
    if locations.is_empty() {
        return Ok(());
    }
    if locations.len() == 1 {
        // If there's only a single location then just jump directly to it
        let location = &locations[0];
        let _ = event_sink.submit_command(
            LAPCE_UI_COMMAND,
            LapceUICommand::JumpToLspLocation(
                None,
                EditorLocation {
                    path: path_from_url(&location.uri),
                    position: Some(location.range.start),
                    scroll_offset: None,
                    history: None,
                },
            ),
            Target::Auto,
        );
    } else {
        let _ = event_sink.submit_command(
            LAPCE_UI_COMMAND,
            LapceUICommand::PaletteReferences(offset, locations),
            Target::Auto,
        );
    }
    Ok(())
}

fn workspace_edits(edit: &WorkspaceEdit) -> Option<HashMap<Url, Vec<TextEdit>>> {
    if let Some(changes) = edit.changes.as_ref() {
        return Some(changes.clone());
    }

    let changes = edit.document_changes.as_ref()?;
    let edits = match changes {
        DocumentChanges::Edits(edits) => edits
            .iter()
            .map(|e| {
                (
                    e.text_document.uri.clone(),
                    e.edits
                        .iter()
                        .map(|e| match e {
                            OneOf::Left(e) => e.clone(),
                            OneOf::Right(e) => e.text_edit.clone(),
                        })
                        .collect(),
                )
            })
            .collect::<HashMap<Url, Vec<TextEdit>>>(),
        DocumentChanges::Operations(ops) => ops
            .iter()
            .filter_map(|o| match o {
                DocumentChangeOperation::Op(_op) => None,
                DocumentChangeOperation::Edit(e) => Some((
                    e.text_document.uri.clone(),
                    e.edits
                        .iter()
                        .map(|e| match e {
                            OneOf::Left(e) => e.clone(),
                            OneOf::Right(e) => e.text_edit.clone(),
                        })
                        .collect(),
                )),
            })
            .collect::<HashMap<Url, Vec<TextEdit>>>(),
    };
    Some(edits)
}

/// Check if a [`Url`] matches the path
fn url_matches_path(path: &Path, url: &Url) -> bool {
    // TODO: Neither of these methods work for paths
    // on different filesystems (i.e. windows and linux),
    // as pathbuf is meant to represent a path on the host
    let mut matches = false;
    // This handles windows drive letters, which rust-url doesn't do.
    if let Ok(url_path) = url.to_file_path() {
        matches |= url_path == path;
    }
    // This is the previous check, to ensure this isn't a regression
    if let Ok(path_url) = Url::from_file_path(path) {
        matches |= &path_url == url;
    }

    matches
}

fn apply_code_action(
    doc: &Document,
    main_split: &mut LapceMainSplitData,
    path: &Path,
    edits: &[TextEdit],
) {
    let edits = edits
        .iter()
        .map(|edit| {
            let selection = lapce_core::selection::Selection::region(
                doc.buffer().offset_of_position(&edit.range.start)?,
                doc.buffer().offset_of_position(&edit.range.end)?,
            );
            Some((selection, edit.new_text.as_str()))
        })
        .collect::<Option<Vec<_>>>();

    if let Some(edits) = edits {
        main_split.edit(path, &edits, lapce_core::editor::EditType::Other);
    } else {
        log::error!("Failed to convert code action edit Position to offset");
    }
}
