use super::*;
use crossterm::event::{KeyCode, KeyModifiers};

impl ClientShellState {
    pub(super) fn reset_copy_pipeline(&mut self) {
        self.copy_session_generation = self.copy_session_generation.saturating_add(1);
        self.copy_operation_in_flight = false;
        self.copy_operation_queue.clear();
        self.copy_input_queue.clear();
    }

    pub(super) fn enter_copy_mode(&mut self, outcome: &mut ClientShellInput) -> bool {
        let pane_id = match self.focused_pane_id() {
            Some(pane_id) => pane_id,
            None => return false,
        };
        if self
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.pane_id == pane_id)
        {
            self.mode = ClientShellMode::Copy;
            return true;
        }
        if self.copy_mode.is_some() {
            self.exit_copy_mode(false, outcome);
        }
        let Some(hit) = self
            .hits
            .panes
            .iter()
            .find(|hit| hit.pane_id == pane_id)
            .cloned()
        else {
            return false;
        };
        let Some(metrics) = hit.scroll else {
            return false;
        };
        let viewport_top = metrics
            .max_offset_from_bottom
            .saturating_sub(metrics.offset_from_bottom)
            .min(u32::MAX as usize) as u32;
        let cursor = self
            .pane_surface
            .as_ref()
            .and_then(|surface| {
                let pane = surface.panes.iter().find(|pane| pane.pane_id == pane_id)?;
                let cursor = surface
                    .frame
                    .cursor
                    .as_ref()
                    .filter(|cursor| cursor.visible)?;
                let inner = pane.inner_rect;
                (cursor.x >= inner.x
                    && cursor.x < inner.x.saturating_add(inner.width)
                    && cursor.y >= inner.y
                    && cursor.y < inner.y.saturating_add(inner.height))
                .then_some(crate::api::schema::PaneTextPoint {
                    row: viewport_top.saturating_add(u32::from(cursor.y - inner.y)),
                    col: cursor.x - inner.x,
                })
            })
            .unwrap_or(crate::api::schema::PaneTextPoint {
                row: viewport_top
                    .saturating_add(u32::from(hit.inner_rect.height.saturating_sub(1))),
                col: 0,
            });
        self.selection = None;
        self.stop_selection_autoscroll();
        self.selection_highlight_clear_deadline = None;
        self.reset_copy_pipeline();
        let content_revision = self
            .pane_surface
            .as_ref()
            .and_then(|surface| surface.panes.iter().find(|pane| pane.pane_id == pane_id))
            .map_or(0, |pane| pane.content_revision);
        self.copy_mode = Some(ClientCopyModeState {
            pane_id,
            content_revision,
            geometry: (hit.inner_rect.width, hit.inner_rect.height),
            cursor,
            offset_from_bottom: metrics.offset_from_bottom,
            max_offset_from_bottom: metrics.max_offset_from_bottom,
            entry_offset_from_bottom: metrics.offset_from_bottom,
            selection: None,
            search_prompt: None,
            search_query: String::new(),
            search_direction: None,
            search_matches: Vec::new(),
            search_total: 0,
            search_current: None,
            search_current_global: None,
            search_generation: 0,
            copy_after_search: false,
        });
        self.mode = ClientShellMode::Copy;
        true
    }

    pub(super) fn route_copy_mode_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if self.route_copy_search_prompt_key(key, outcome) {
            return;
        }
        if key.code == KeyCode::Esc {
            let should_clear = self.copy_mode.as_ref().is_some_and(|copy_mode| {
                copy_mode.selection.is_some()
                    || !copy_mode.search_query.is_empty()
                    || !copy_mode.search_matches.is_empty()
                    || copy_mode.search_direction.is_some()
            });
            if should_clear {
                if let Some(copy_mode) = self.copy_mode.as_mut() {
                    copy_mode.selection = None;
                    copy_mode.search_query.clear();
                    copy_mode.search_direction = None;
                    copy_mode.search_matches.clear();
                    copy_mode.search_total = 0;
                    copy_mode.search_current = None;
                    copy_mode.search_current_global = None;
                    copy_mode.search_generation = copy_mode.search_generation.saturating_add(1);
                    copy_mode.copy_after_search = false;
                }
                self.selection = None;
            } else {
                self.exit_copy_mode(false, outcome);
            }
            outcome.repaint = true;
            return;
        }

        let Some(action) =
            resolve_copy_mode_action(&self.config.keybinds.keybinds.copy_mode_keys, key)
        else {
            return;
        };
        match action {
            CopyModeAction::Exit { copy: false } => self.exit_copy_mode(false, outcome),
            CopyModeAction::Exit { copy: true } => {
                if !self.defer_copy_until_search_result() {
                    self.exit_copy_mode(true, outcome);
                }
            }
            CopyModeAction::MoveCursor { rows, cols } => self.move_copy_cursor(rows, cols, outcome),
            CopyModeAction::Motion(motion) => self.request_copy_motion(motion, outcome),
            CopyModeAction::StartOfLine => {
                self.set_copy_cursor_col(0);
                self.sync_copy_selection();
            }
            CopyModeAction::History { top } => self.move_copy_history(top, outcome),
            CopyModeAction::ScrollPage {
                direction,
                half_page,
            } => self.move_copy_page(direction, half_page, outcome),
            CopyModeAction::BeginSelection { linewise } => self.begin_copy_selection(linewise),
            CopyModeAction::OpenSearch(direction) => self.open_copy_search(direction),
            CopyModeAction::RepeatSearch { reverse } => self.repeat_copy_search(reverse, outcome),
        }
        outcome.repaint = true;
    }

    fn route_copy_search_prompt_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(prompt) = self
            .copy_mode
            .as_ref()
            .and_then(|copy_mode| copy_mode.search_prompt.as_ref())
        else {
            return false;
        };
        let mut submit = None;
        match key.code {
            KeyCode::Esc => {
                if let Some(copy_mode) = self.copy_mode.as_mut() {
                    copy_mode.search_prompt = None;
                }
            }
            KeyCode::Enter => {
                submit = Some((prompt.query.clone(), prompt.direction));
                if let Some(copy_mode) = self.copy_mode.as_mut() {
                    copy_mode.search_prompt = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.search_prompt.as_mut())
                {
                    prompt.query.pop();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(prompt) = self
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.search_prompt.as_mut())
                {
                    prompt.query.clear();
                }
            }
            _ => {
                if let Some(ch) = crate::copy_mode::copy_mode_command_char(key.clone()) {
                    if let Some(prompt) = self
                        .copy_mode
                        .as_mut()
                        .and_then(|copy_mode| copy_mode.search_prompt.as_mut())
                    {
                        prompt.query.push(ch);
                    }
                }
            }
        }
        if let Some((query, direction)) = submit {
            self.request_copy_search(query, direction, false, outcome);
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn insert_copy_search_text(&mut self, text: &str) -> bool {
        let Some(prompt) = self
            .copy_mode
            .as_mut()
            .and_then(|copy_mode| copy_mode.search_prompt.as_mut())
        else {
            return false;
        };
        prompt
            .query
            .extend(text.chars().filter(|character| !character.is_control()));
        true
    }

    fn open_copy_search(&mut self, direction: crate::api::schema::PaneCopySearchDirection) {
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return;
        };
        copy_mode.search_prompt = Some(ClientCopySearchPrompt {
            direction,
            query: String::new(),
        });
    }

    fn repeat_copy_search(&mut self, reverse: bool, outcome: &mut ClientShellInput) {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return;
        };
        if copy_mode.search_query.is_empty() {
            return;
        }
        let Some(direction) = copy_mode.search_direction else {
            return;
        };
        let direction = if reverse {
            match direction {
                crate::api::schema::PaneCopySearchDirection::Forward => {
                    crate::api::schema::PaneCopySearchDirection::Backward
                }
                crate::api::schema::PaneCopySearchDirection::Backward => {
                    crate::api::schema::PaneCopySearchDirection::Forward
                }
            }
        } else {
            direction
        };
        self.request_copy_search(copy_mode.search_query.clone(), direction, true, outcome);
    }

    fn defer_copy_until_search_result(&mut self) -> bool {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return false;
        };
        let pane_id = copy_mode.pane_id.clone();
        let generation = copy_mode.search_generation;
        let pending = self.pending_requests.values().any(|pending| {
            matches!(
                &pending.kind,
                PendingEndpointKind::CopySearch {
                    pane_id: pending_pane,
                    generation: pending_generation,
                    ..
                } if pending_pane == &pane_id && *pending_generation == generation
            )
        }) || self
            .copy_operation_queue
            .iter()
            .any(|operation| matches!(operation, ClientCopyOperation::Search { .. }));
        if pending {
            if let Some(copy_mode) = self.copy_mode.as_mut() {
                copy_mode.copy_after_search = true;
            }
        }
        pending
    }

    fn request_copy_search(
        &mut self,
        query: String,
        direction: crate::api::schema::PaneCopySearchDirection,
        repeat: bool,
        outcome: &mut ClientShellInput,
    ) {
        if query.is_empty() || self.copy_mode.is_none() {
            return;
        }
        self.copy_operation_queue
            .push_back(ClientCopyOperation::Search {
                query,
                direction,
                repeat,
            });
        self.dispatch_next_copy_operation(outcome);
    }

    pub(super) fn apply_copy_search_result(
        &mut self,
        pane_id: &str,
        origin: crate::api::schema::PaneTextPoint,
        query: String,
        direction: crate::api::schema::PaneCopySearchDirection,
        repeat: bool,
        generation: u64,
        result: ClientCopySearchResult,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let search_queued = self
            .copy_operation_queue
            .iter()
            .any(|operation| matches!(operation, ClientCopyOperation::Search { .. }));
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return false;
        };
        if copy_mode.pane_id != pane_id
            || copy_mode.cursor != origin
            || copy_mode.content_revision != result.content_revision
            || copy_mode.search_generation != generation
        {
            return false;
        }
        let current = result.current.filter(|index| *index < result.matches.len());
        copy_mode.search_query = query;
        if !repeat {
            copy_mode.search_direction = Some(direction);
        }
        copy_mode.search_matches = result.matches;
        copy_mode.search_total = result.total;
        copy_mode.search_current = current;
        copy_mode.search_current_global = result.current_global;
        let target = current.and_then(|index| copy_mode.search_matches.get(index).copied());
        let copy_after_search = if search_queued {
            false
        } else {
            std::mem::take(&mut copy_mode.copy_after_search)
        };
        if let Some(target) = target {
            copy_mode.cursor = target.start;
            self.reveal_copy_cursor(outcome, true);
            self.sync_copy_selection();
        }
        if copy_after_search {
            self.exit_copy_mode(true, outcome);
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn complete_copy_operation(
        &mut self,
        session_generation: u64,
        continue_queue: bool,
        outcome: &mut ClientShellInput,
    ) {
        if self.copy_session_generation != session_generation {
            return;
        }
        self.copy_operation_in_flight = false;
        if continue_queue && self.copy_mode.is_some() {
            self.dispatch_next_copy_operation(outcome);
            self.dispatch_queued_copy_input(outcome);
        } else {
            self.copy_operation_queue.clear();
            self.copy_input_queue.clear();
        }
    }

    fn dispatch_queued_copy_input(&mut self, outcome: &mut ClientShellInput) {
        while !self.copy_operation_in_flight {
            let Some(key) = self.copy_input_queue.pop_front() else {
                return;
            };
            self.handle_key(key, outcome);
        }
    }

    pub(super) fn cancel_deferred_copy_after_search(&mut self, generation: u64) {
        if let Some(copy_mode) = self
            .copy_mode
            .as_mut()
            .filter(|copy_mode| copy_mode.search_generation == generation)
        {
            copy_mode.copy_after_search = false;
        }
    }

    fn copy_hit(&self) -> Option<PaneHit> {
        let pane_id = self.copy_mode.as_ref()?.pane_id.as_str();
        self.hits
            .panes
            .iter()
            .find(|hit| hit.pane_id == pane_id)
            .cloned()
    }

    fn move_copy_cursor(&mut self, row_delta: i16, col_delta: i16, outcome: &mut ClientShellInput) {
        let Some(hit) = self.copy_hit() else {
            self.exit_copy_mode(false, outcome);
            return;
        };
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return;
        };
        if col_delta < 0 {
            copy_mode.cursor.col = copy_mode
                .cursor
                .col
                .saturating_sub(col_delta.unsigned_abs());
        } else if col_delta > 0 {
            copy_mode.cursor.col = copy_mode
                .cursor
                .col
                .saturating_add(col_delta as u16)
                .min(hit.inner_rect.width.saturating_sub(1));
        }
        let total_rows = copy_mode
            .max_offset_from_bottom
            .saturating_add(hit.inner_rect.height as usize)
            .max(1);
        if row_delta < 0 {
            copy_mode.cursor.row = copy_mode
                .cursor
                .row
                .saturating_sub(u32::from(row_delta.unsigned_abs()));
        } else if row_delta > 0 {
            copy_mode.cursor.row = copy_mode
                .cursor
                .row
                .saturating_add(u32::from(row_delta as u16))
                .min(total_rows.saturating_sub(1).min(u32::MAX as usize) as u32);
        }
        self.reveal_copy_cursor(outcome, false);
        self.sync_copy_selection();
        outcome.repaint = true;
    }

    fn move_copy_page(&mut self, direction: i8, half_page: bool, outcome: &mut ClientShellInput) {
        let Some(hit) = self.copy_hit() else {
            return;
        };
        let lines = crate::copy_mode::copy_mode_page_lines(hit.inner_rect.height, half_page);
        let Some((pane_id, next_offset)) = self.copy_mode.as_mut().map(|copy_mode| {
            if direction < 0 {
                copy_mode.cursor.row = copy_mode.cursor.row.saturating_sub(lines as u32);
                copy_mode.offset_from_bottom = copy_mode
                    .offset_from_bottom
                    .saturating_add(lines)
                    .min(copy_mode.max_offset_from_bottom);
            } else {
                let last_row = copy_mode
                    .max_offset_from_bottom
                    .saturating_add(hit.inner_rect.height as usize)
                    .saturating_sub(1)
                    .min(u32::MAX as usize) as u32;
                copy_mode.cursor.row = copy_mode
                    .cursor
                    .row
                    .saturating_add(lines as u32)
                    .min(last_row);
                copy_mode.offset_from_bottom = copy_mode.offset_from_bottom.saturating_sub(lines);
            }
            (copy_mode.pane_id.clone(), copy_mode.offset_from_bottom)
        }) else {
            return;
        };
        self.push_pane_scroll_offset(pane_id, next_offset, outcome);
        self.sync_copy_selection();
        outcome.repaint = true;
    }

    fn move_copy_history(&mut self, top: bool, outcome: &mut ClientShellInput) {
        let Some(hit) = self.copy_hit() else {
            return;
        };
        let Some((pane_id, offset_from_bottom)) = self.copy_mode.as_mut().map(|copy_mode| {
            if top {
                copy_mode.cursor.row = 0;
                copy_mode.offset_from_bottom = copy_mode.max_offset_from_bottom;
            } else {
                copy_mode.cursor.row = copy_mode
                    .max_offset_from_bottom
                    .saturating_add(hit.inner_rect.height as usize)
                    .saturating_sub(1)
                    .min(u32::MAX as usize) as u32;
                copy_mode.offset_from_bottom = 0;
            }
            (copy_mode.pane_id.clone(), copy_mode.offset_from_bottom)
        }) else {
            return;
        };
        self.push_pane_scroll_offset(pane_id, offset_from_bottom, outcome);
        self.sync_copy_selection();
        outcome.repaint = true;
    }

    fn set_copy_cursor_col(&mut self, col: u16) {
        if let Some(copy_mode) = self.copy_mode.as_mut() {
            copy_mode.cursor.col = col;
        }
    }

    fn reveal_copy_cursor(&mut self, outcome: &mut ClientShellInput, reserve_mode_bar_row: bool) {
        let Some(hit) = self.copy_hit() else {
            return;
        };
        let request = self.copy_mode.as_mut().and_then(|copy_mode| {
            let current_top = copy_mode
                .max_offset_from_bottom
                .saturating_sub(copy_mode.offset_from_bottom) as u32;
            let max_cursor_row = hit
                .inner_rect
                .height
                .saturating_sub(if reserve_mode_bar_row { 2 } else { 1 });
            let bottom = current_top.saturating_add(u32::from(max_cursor_row));
            let desired_top = if copy_mode.cursor.row < current_top {
                copy_mode.cursor.row
            } else if copy_mode.cursor.row > bottom {
                copy_mode
                    .cursor
                    .row
                    .saturating_sub(u32::from(max_cursor_row))
            } else {
                current_top
            };
            let offset = copy_mode
                .max_offset_from_bottom
                .saturating_sub(desired_top as usize);
            if offset == copy_mode.offset_from_bottom {
                return None;
            }
            copy_mode.offset_from_bottom = offset;
            Some((copy_mode.pane_id.clone(), offset))
        });
        if let Some((pane_id, offset)) = request {
            self.push_pane_scroll_offset(pane_id, offset, outcome);
        }
    }

    fn begin_copy_selection(&mut self, linewise: bool) {
        let end_col = self
            .copy_hit()
            .map_or(0, |hit| hit.inner_rect.width.saturating_sub(1));
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return;
        };
        if linewise {
            copy_mode.selection = Some(ClientCopySelection::Linewise {
                anchor_row: copy_mode.cursor.row,
            });
            self.selection = Some(crate::selection::Selection::line_range(
                copy_mode.pane_id.clone(),
                copy_mode.cursor.row,
                copy_mode.cursor.row,
                end_col,
            ));
        } else {
            copy_mode.selection = Some(ClientCopySelection::Character {
                anchor: copy_mode.cursor,
            });
            self.selection = Some(crate::selection::Selection::absolute_anchor(
                copy_mode.pane_id.clone(),
                (copy_mode.cursor.row, copy_mode.cursor.col),
            ));
        }
    }

    pub(super) fn sync_copy_selection(&mut self) {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return;
        };
        let Some(selection) = copy_mode.selection else {
            return;
        };
        self.selection = Some(match selection {
            ClientCopySelection::Character { anchor } => {
                crate::selection::Selection::absolute_range(
                    copy_mode.pane_id.clone(),
                    (anchor.row, anchor.col),
                    (copy_mode.cursor.row, copy_mode.cursor.col),
                )
            }
            ClientCopySelection::Linewise { anchor_row } => {
                crate::selection::Selection::line_range(
                    copy_mode.pane_id.clone(),
                    anchor_row,
                    copy_mode.cursor.row,
                    self.copy_hit()
                        .map_or(0, |hit| hit.inner_rect.width.saturating_sub(1)),
                )
            }
        });
    }

    fn request_copy_motion(
        &mut self,
        motion: crate::api::schema::PaneCopyMotion,
        outcome: &mut ClientShellInput,
    ) {
        if self.copy_mode.is_none() {
            return;
        }
        self.copy_operation_queue
            .push_back(ClientCopyOperation::Motion(motion));
        self.dispatch_next_copy_operation(outcome);
    }

    pub(super) fn dispatch_next_copy_operation(&mut self, outcome: &mut ClientShellInput) {
        if self.copy_operation_in_flight {
            return;
        }
        while let Some(operation) = self.copy_operation_queue.pop_front() {
            let Some(copy_mode) = self.copy_mode.as_mut() else {
                self.copy_operation_queue.clear();
                self.copy_input_queue.clear();
                return;
            };
            let session_generation = self.copy_session_generation;
            let pane_id = copy_mode.pane_id.clone();
            let origin = copy_mode.cursor;
            let (method, kind) = match operation {
                ClientCopyOperation::Motion(motion) => (
                    crate::api::schema::Method::PaneCopyMotion(
                        crate::api::schema::PaneCopyMotionParams {
                            pane_id: pane_id.clone(),
                            cursor: origin,
                            motion,
                            content_revision: Some(copy_mode.content_revision),
                        },
                    ),
                    PendingEndpointKind::CopyMotion {
                        pane_id,
                        origin,
                        session_generation,
                    },
                ),
                ClientCopyOperation::Search {
                    query,
                    direction,
                    repeat,
                } => {
                    if query.is_empty() {
                        continue;
                    }
                    copy_mode.search_generation = copy_mode.search_generation.saturating_add(1);
                    let generation = copy_mode.search_generation;
                    let previous = repeat
                        .then(|| {
                            copy_mode
                                .search_current
                                .and_then(|index| copy_mode.search_matches.get(index).copied())
                                .filter(|text_match| text_match.start == copy_mode.cursor)
                        })
                        .flatten();
                    (
                        crate::api::schema::Method::PaneCopySearch(
                            crate::api::schema::PaneCopySearchParams {
                                pane_id: pane_id.clone(),
                                query: query.clone(),
                                direction,
                                cursor: origin,
                                content_revision: copy_mode.content_revision,
                                previous,
                            },
                        ),
                        PendingEndpointKind::CopySearch {
                            pane_id,
                            origin,
                            query,
                            direction,
                            repeat,
                            generation,
                            session_generation,
                        },
                    )
                }
            };
            self.copy_operation_in_flight = true;
            self.push_endpoint_method_with_kind(method, kind, outcome);
            return;
        }
    }

    pub(super) fn apply_copy_motion_target(
        &mut self,
        pane_id: &str,
        origin: crate::api::schema::PaneTextPoint,
        cursor: crate::api::schema::PaneTextPoint,
        content_revision: u64,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return false;
        };
        if copy_mode.pane_id != pane_id
            || copy_mode.cursor != origin
            || copy_mode.content_revision != content_revision
        {
            return false;
        }
        copy_mode.cursor = cursor;
        self.reveal_copy_cursor(outcome, false);
        self.sync_copy_selection();
        outcome.repaint = true;
        true
    }

    pub(super) fn exit_copy_mode(&mut self, copy: bool, outcome: &mut ClientShellInput) {
        if copy
            && !self
                .selection
                .as_ref()
                .is_some_and(crate::selection::Selection::is_visible)
        {
            if let Some((pane_id, text_match)) = self.copy_mode.as_ref().and_then(|copy_mode| {
                copy_mode
                    .search_current
                    .and_then(|index| copy_mode.search_matches.get(index).copied())
                    .map(|text_match| (copy_mode.pane_id.clone(), text_match))
            }) {
                self.selection = Some(crate::selection::Selection::absolute_range(
                    pane_id,
                    (text_match.start.row, text_match.start.col),
                    (text_match.end.row, text_match.end.col),
                ));
            }
        }
        let Some(copy_mode) = self.copy_mode.take() else {
            return;
        };
        self.reset_copy_pipeline();
        if copy
            && self
                .selection
                .as_ref()
                .is_some_and(crate::selection::Selection::is_visible)
        {
            self.request_selection_copy(outcome);
        }
        self.selection = None;
        self.selection_highlight_clear_deadline = None;
        self.push_pane_scroll_offset(
            copy_mode.pane_id,
            copy_mode.entry_offset_from_bottom,
            outcome,
        );
        self.mode = ClientShellMode::Terminal;
        outcome.repaint = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyModeAction {
    Exit { copy: bool },
    MoveCursor { rows: i16, cols: i16 },
    Motion(crate::api::schema::PaneCopyMotion),
    StartOfLine,
    History { top: bool },
    ScrollPage { direction: i8, half_page: bool },
    BeginSelection { linewise: bool },
    OpenSearch(crate::api::schema::PaneCopySearchDirection),
    RepeatSearch { reverse: bool },
}

/// Resolves a copy mode key press to an action using the configured keybinds.
fn resolve_copy_mode_action(
    keybinds: &crate::config::CopyModeKeybinds,
    key: &crate::input::TerminalKey,
) -> Option<CopyModeAction> {
    use crate::api::schema::{PaneCopyMotion, PaneCopySearchDirection};

    macro_rules! resolve {
        ($field:ident, $action:expr) => {
            if keybinds.$field.matches_direct_key(key) {
                return Some($action);
            }
        };
    }

    resolve!(cancel, CopyModeAction::Exit { copy: false });
    resolve!(copy, CopyModeAction::Exit { copy: true });
    resolve!(
        cursor_left,
        CopyModeAction::MoveCursor { rows: 0, cols: -1 }
    );
    resolve!(cursor_down, CopyModeAction::MoveCursor { rows: 1, cols: 0 });
    resolve!(cursor_up, CopyModeAction::MoveCursor { rows: -1, cols: 0 });
    resolve!(
        cursor_right,
        CopyModeAction::MoveCursor { rows: 0, cols: 1 }
    );
    resolve!(
        next_word,
        CopyModeAction::Motion(PaneCopyMotion::NextWordStart)
    );
    resolve!(
        previous_word,
        CopyModeAction::Motion(PaneCopyMotion::PreviousWordStart)
    );
    resolve!(
        next_word_end,
        CopyModeAction::Motion(PaneCopyMotion::NextWordEnd)
    );
    resolve!(
        next_big_word,
        CopyModeAction::Motion(PaneCopyMotion::NextBigWordStart)
    );
    resolve!(
        previous_big_word,
        CopyModeAction::Motion(PaneCopyMotion::PreviousBigWordStart)
    );
    resolve!(
        next_big_word_end,
        CopyModeAction::Motion(PaneCopyMotion::NextBigWordEnd)
    );
    resolve!(
        first_non_blank,
        CopyModeAction::Motion(PaneCopyMotion::FirstNonBlank)
    );
    resolve!(start_of_line, CopyModeAction::StartOfLine);
    resolve!(end_of_line, CopyModeAction::Motion(PaneCopyMotion::LineEnd));
    resolve!(
        next_paragraph,
        CopyModeAction::Motion(PaneCopyMotion::NextParagraph)
    );
    resolve!(
        previous_paragraph,
        CopyModeAction::Motion(PaneCopyMotion::PreviousParagraph)
    );
    resolve!(scrollback_top, CopyModeAction::History { top: true });
    resolve!(scrollback_bottom, CopyModeAction::History { top: false });
    resolve!(
        page_up,
        CopyModeAction::ScrollPage {
            direction: -1,
            half_page: false,
        }
    );
    resolve!(
        page_down,
        CopyModeAction::ScrollPage {
            direction: 1,
            half_page: false,
        }
    );
    resolve!(
        half_page_up,
        CopyModeAction::ScrollPage {
            direction: -1,
            half_page: true,
        }
    );
    resolve!(
        half_page_down,
        CopyModeAction::ScrollPage {
            direction: 1,
            half_page: true,
        }
    );
    resolve!(
        begin_selection,
        CopyModeAction::BeginSelection { linewise: false }
    );
    resolve!(
        select_line,
        CopyModeAction::BeginSelection { linewise: true }
    );
    resolve!(
        search_forward,
        CopyModeAction::OpenSearch(PaneCopySearchDirection::Forward)
    );
    resolve!(
        search_backward,
        CopyModeAction::OpenSearch(PaneCopySearchDirection::Backward)
    );
    resolve!(search_next, CopyModeAction::RepeatSearch { reverse: false });
    resolve!(
        search_previous,
        CopyModeAction::RepeatSearch { reverse: true }
    );

    None
}
