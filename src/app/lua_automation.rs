use super::*;
use crate::{
    lua::automation::{Invocation, REQUEST_FIELD, parse_keys},
    speech::ReaderSpeechEventKind,
};
use anyhow::{Context, anyhow, bail};
use mlua::{Table, ThreadStatus, Value};
use std::{
    collections::{HashMap, VecDeque},
    hash::{Hash, Hasher},
    sync::Arc,
};

const MAX_VIEW_SNAPSHOTS: usize = 16;
const STABLE_SCREEN_TIMEOUT_MS: u128 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LuaViewKey {
    view_id: ViewId,
    screen: ScreenIdentity,
    revision: Option<ViewRevision>,
    fingerprint: u64,
    rows: u16,
    cols: u16,
}

struct LuaViewSnapshot {
    token: u64,
    key: LuaViewKey,
    content: LuaViewContent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LuaViewContent {
    lines: Arc<[String]>,
    wrapped: Arc<[bool]>,
}

struct LuaReader {
    token: u64,
    saved_auto_read: bool,
    support: crate::speech::ReaderSupport,
}

struct InputReceipt {
    token: u64,
    context: LuaViewKey,
    content: LuaViewContent,
    bell_count: u64,
    quiet_ms: u16,
}

struct StableWait {
    context: LuaViewKey,
    content: LuaViewContent,
    initial_bell_count: u64,
    last_bell_count: u64,
    last_observed: LuaViewKey,
    last_change_ms: u128,
    timeout_ms: u128,
    saw_post_input_presentation: bool,
    quiet_ms: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LuaCoordinate {
    row: u16,
    col: u16,
}

struct ActiveRead {
    context: LuaViewKey,
    submission: crate::speech::ReaderSubmission,
    offsets: Vec<(usize, LuaCoordinate)>,
    finish: LuaCoordinate,
    position: LuaCoordinate,
}

enum LuaWait {
    Ready,
    Reading(ActiveRead),
    Stable(StableWait),
}

enum LuaWake {
    Read {
        status: &'static str,
        cause: String,
        position: LuaCoordinate,
        close_reader: bool,
    },
    Stable {
        status: &'static str,
        context: LuaViewKey,
        content: LuaViewContent,
        bells: u64,
    },
    Abort,
}

pub(super) struct LuaTask {
    invocation: Invocation,
    wait: LuaWait,
    reader: Option<LuaReader>,
    views: VecDeque<LuaViewSnapshot>,
    receipts: HashMap<u64, InputReceipt>,
}

impl LuaTask {
    fn new(invocation: Invocation) -> Self {
        Self {
            invocation,
            wait: LuaWait::Ready,
            reader: None,
            views: VecDeque::new(),
            receipts: HashMap::new(),
        }
    }

    fn snapshot(&self, token: u64) -> anyhow::Result<&LuaViewSnapshot> {
        self.views
            .iter()
            .find(|snapshot| snapshot.token == token)
            .ok_or_else(|| anyhow!("view snapshot is no longer available"))
    }

    fn view(&self, token: u64) -> anyhow::Result<LuaViewKey> {
        Ok(self.snapshot(token)?.key)
    }
}

impl App {
    pub(super) fn start_lua_invocation(
        &mut self,
        sr: &mut ScreenReader,
        invocation: Invocation,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if self.lua_task.is_some() {
            sr.report_runtime_error(
                "Lua binding unavailable",
                "lua-binding",
                "another Lua key binding is waiting for Lector",
            );
            return Ok(());
        }
        let task = LuaTask::new(invocation);
        self.resume_lua_task(sr, task, Value::Nil, pty_out, term_out)
    }

    pub(super) fn drive_lua_automation(
        &mut self,
        sr: &mut ScreenReader,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let Some(mut task) = self.lua_task.take() else {
            return Ok(());
        };
        if task
            .reader
            .as_ref()
            .is_some_and(|reader| sr.reader_support() != reader.support)
        {
            self.finish_lua_task(sr, &mut task, true)?;
            return Ok(());
        }
        let wake_result = match &mut task.wait {
            LuaWait::Ready => Ok(None),
            LuaWait::Reading(read) => self.poll_lua_read(sr, read),
            LuaWait::Stable(wait) => self.poll_lua_stable_wait(wait),
        };
        let wake = match wake_result {
            Ok(wake) => wake,
            Err(error) => {
                return self.recover_lua_task_error(sr, &mut task, error);
            }
        };
        if let Some(wake) = wake {
            let response = match wake {
                LuaWake::Read {
                    status,
                    cause,
                    position,
                    close_reader,
                } => {
                    if close_reader {
                        self.finish_lua_reader(sr, &mut task);
                    }
                    Value::Table(read_result(&task.invocation.lua, status, &cause, position)?)
                }
                LuaWake::Stable {
                    status,
                    context,
                    content,
                    bells,
                } => Value::Table(
                    self.capture_lua_stable_response(&mut task, status, context, &content, bells)?,
                ),
                LuaWake::Abort => {
                    self.finish_lua_task(sr, &mut task, true)?;
                    return Ok(());
                }
            };
            task.wait = LuaWait::Ready;
            self.resume_lua_task(sr, task, response, pty_out, term_out)
        } else {
            self.lua_task = Some(task);
            Ok(())
        }
    }

    /// A physical key press has priority over every reader transition. The
    /// press and its matching release are consumed by the ordinary binding
    /// bookkeeping, while the suspended call is resumed with a cancellation
    /// result so Lua can run its own cleanup.
    pub(super) fn cancel_lua_reader_for_key(
        &mut self,
        sr: &mut ScreenReader,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        let reader_active = self
            .lua_task
            .as_ref()
            .is_some_and(|task| task.reader.is_some());
        if !reader_active {
            return Ok(false);
        }
        let mut task = self.lua_task.take().expect("reader task was present");
        let wait = std::mem::replace(&mut task.wait, LuaWait::Ready);
        let response = match wait {
            LuaWait::Reading(read) => {
                if let Err(error) = sr.speech_mut().cancel() {
                    self.finish_lua_reader(sr, &mut task);
                    return Err(error.into());
                }
                read_result(&task.invocation.lua, "cancelled", "key", read.position)
                    .map(Value::Table)
                    .map_err(Into::into)
            }
            LuaWait::Stable(wait) => self
                .capture_lua_stable_response(
                    &mut task,
                    "cancelled",
                    wait.context,
                    &wait.content,
                    self.presented_bell_count
                        .saturating_sub(wait.initial_bell_count),
                )
                .map(Value::Table),
            LuaWait::Ready => {
                self.lua_task = Some(task);
                return Ok(false);
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.finish_lua_task(sr, &mut task, false)?;
                return Err(error);
            }
        };
        self.resume_lua_task(sr, task, response, pty_out, term_out)?;
        Ok(true)
    }

    pub(super) fn lua_automation_deadline_ms(&self) -> Option<u128> {
        let task = self.lua_task.as_ref()?;
        match &task.wait {
            LuaWait::Stable(wait) => Some(if wait.saw_post_input_presentation {
                wait.last_change_ms
                    .saturating_add(u128::from(wait.quiet_ms))
                    .min(wait.timeout_ms)
            } else {
                wait.timeout_ms
            }),
            LuaWait::Ready | LuaWait::Reading(_) => None,
        }
    }

    fn resume_lua_task(
        &mut self,
        sr: &mut ScreenReader,
        mut task: LuaTask,
        response: Value,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        match self.resume_lua_task_inner(sr, &mut task, response, pty_out, term_out) {
            Ok(true) => {
                self.lua_task = Some(task);
                Ok(())
            }
            Ok(false) => self.finish_lua_task(sr, &mut task, false),
            Err(error) => self.recover_lua_task_error(sr, &mut task, error),
        }
    }

    /// A binding owns auto-read suppression and may own in-flight reader
    /// speech. Its error becomes recoverable only after both have been
    /// released successfully; a cleanup failure still escapes to shutdown.
    fn recover_lua_task_error(
        &mut self,
        sr: &mut ScreenReader,
        task: &mut LuaTask,
        error: anyhow::Error,
    ) -> Result<()> {
        self.finish_lua_task(sr, task, true)?;
        sr.report_runtime_error("Lua binding failed", "lua-binding", format!("{error:#}"));
        Ok(())
    }

    /// Return true when the coroutine yielded an asynchronous wait and false
    /// when it returned. The outer owner performs cleanup on every exit path.
    fn resume_lua_task_inner(
        &mut self,
        sr: &mut ScreenReader,
        task: &mut LuaTask,
        mut response: Value,
        pty_out: &mut dyn Write,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        loop {
            let yielded = match task.invocation.thread.resume::<Value>(response) {
                Ok(value) => value,
                Err(error) => return Err(anyhow!("Lua key binding: {error}")),
            };
            if task.invocation.thread.status() == ThreadStatus::Finished {
                return Ok(false);
            }
            let request = match yielded {
                Value::Table(table) => table,
                _ => bail!("Lua key binding yielded an invalid Lector request"),
            };
            let kind: String = request
                .get(REQUEST_FIELD)
                .context("Lua key binding yielded an untagged request")?;
            match kind.as_str() {
                "current_view" => {
                    response = Value::Table(self.capture_lua_view(task)?);
                }
                "reader_acquire" => {
                    let table = task.invocation.lua.create_table()?;
                    if task.reader.is_some() {
                        table.set("error", "this task already owns a reader")?;
                    } else if !sr.terminal_focused() {
                        table.set("error", "a reader requires the terminal to be focused")?;
                    } else if !sr.reader_support().is_supported() {
                        table.set(
                            "error",
                            "the active speech host does not provide reliable completion, UTF-8 word progress, and confirmed stop",
                        )?;
                    } else {
                        let token = self.next_lua_token();
                        let _ = sr.take_reader_speech_events();
                        task.reader = Some(LuaReader {
                            token,
                            saved_auto_read: sr.auto_read_enabled(),
                            support: sr.reader_support(),
                        });
                        sr.set_reader_auto_read_suppressed(true);
                        table.set("reader", token)?;
                    }
                    response = Value::Table(table);
                }
                "reader_close" => {
                    self.require_reader(task, request.get("reader")?)?;
                    self.finish_lua_reader(sr, task);
                    response = Value::Boolean(true);
                }
                "reader_read" => {
                    self.require_reader(task, request.get("reader")?)?;
                    let view = task.view(request.get("view")?)?;
                    self.require_current_lua_view(view)?;
                    let first = lua_coordinate(request.get("first")?)?;
                    let last = lua_coordinate(request.get("last")?)?;
                    let (text, offsets) = self.mapped_lua_text(view, first, last)?;
                    if text.is_empty() {
                        response = Value::Table(read_result(
                            &task.invocation.lua,
                            "completed",
                            "empty",
                            last,
                        )?);
                        continue;
                    }
                    let submission = sr.speak_for_reader(&text)?;
                    task.wait = LuaWait::Reading(ActiveRead {
                        context: view,
                        submission,
                        offsets,
                        finish: last,
                        position: first,
                    });
                    return Ok(true);
                }
                "send_keys" | "send_text" => {
                    let snapshot = task.snapshot(request.get("view")?)?;
                    let view = snapshot.key;
                    let content = snapshot.content.clone();
                    self.require_current_lua_view(view)?;
                    let bytes = if kind == "send_keys" {
                        parse_keys(&request.get::<String>("keys")?)?
                    } else {
                        request.get::<String>("text")?.into_bytes()
                    };
                    self.dispatch_to_view(sr, &bytes, pty_out, term_out)?;
                    let token = self.next_lua_token();
                    let context = self.current_lua_view_key()?;
                    let accessibility_context = AccessibilityContext {
                        view_id: context.view_id,
                        screen: context.screen,
                    };
                    let quiet_ms = self
                        .stabilization_profiles
                        .get(&accessibility_context)
                        .map_or(DIFF_DELAY, |profile| profile.delay_ms);
                    task.receipts.insert(
                        token,
                        InputReceipt {
                            token,
                            context,
                            content,
                            bell_count: self.presented_bell_count,
                            quiet_ms,
                        },
                    );
                    let table = task.invocation.lua.create_table()?;
                    table.set("receipt", token)?;
                    response = Value::Table(table);
                }
                "wait_for_stable_screen" => {
                    let token: u64 = request.get("receipt")?;
                    let receipt = task
                        .receipts
                        .remove(&token)
                        .ok_or_else(|| anyhow!("input receipt is no longer available"))?;
                    debug_assert_eq!(receipt.token, token);
                    let now = self.clock.now_ms();
                    task.wait = LuaWait::Stable(StableWait {
                        context: receipt.context,
                        content: receipt.content,
                        initial_bell_count: receipt.bell_count,
                        last_bell_count: receipt.bell_count,
                        last_observed: receipt.context,
                        last_change_ms: now,
                        timeout_ms: now.saturating_add(STABLE_SCREEN_TIMEOUT_MS),
                        saw_post_input_presentation: false,
                        quiet_ms: receipt.quiet_ms,
                    });
                    return Ok(true);
                }
                _ => bail!("unknown Lector coroutine request {kind:?}"),
            }
        }
    }

    fn poll_lua_read(
        &mut self,
        sr: &mut ScreenReader,
        read: &mut ActiveRead,
    ) -> Result<Option<LuaWake>> {
        let changed = !sr.reader_support().is_supported()
            || !sr.terminal_focused()
            || match self.current_lua_view_key() {
                Ok(current) => current != read.context,
                Err(_) => true,
            };
        if changed {
            sr.speech_mut().cancel()?;
            let position = read.position;
            return Ok(Some(LuaWake::Read {
                status: "cancelled",
                cause: "screen_changed".to_owned(),
                position,
                close_reader: true,
            }));
        }

        for event in sr.take_reader_speech_events() {
            if event.utterance_id != read.submission.utterance_id {
                continue;
            }
            if let Some(offset) = event
                .position
                .as_ref()
                .and_then(|position| position.utf8_offset())
            {
                let source_offset = read.submission.source_offset(offset);
                let position = coordinate_for_offset(&read.offsets, source_offset, read.position);
                self.move_reader_review_cursor(sr, read.context, position)?;
                read.position = position;
            }
            if event.kind == ReaderSpeechEventKind::Ended {
                let completed = event.reason.as_deref() == Some("completed");
                if completed {
                    self.move_reader_review_cursor(sr, read.context, read.finish)?;
                    read.position = read.finish;
                }
                return Ok(Some(LuaWake::Read {
                    status: if completed { "completed" } else { "cancelled" },
                    cause: event.reason.unwrap_or_else(|| "speech_ended".to_owned()),
                    position: read.position,
                    close_reader: !completed,
                }));
            }
        }
        Ok(None)
    }

    fn poll_lua_stable_wait(&mut self, wait: &mut StableWait) -> Result<Option<LuaWake>> {
        let now = self.clock.now_ms();
        if !self.logical_accessibility_view_is_presented() {
            let same_view_is_catching_up = self.output_scheduler.is_some()
                && self.presented_accessibility_view == Some(wait.context.view_id)
                && self.view_stack.logical_active_view_id() == wait.context.view_id;
            return Ok(if same_view_is_catching_up && now < wait.timeout_ms {
                None
            } else {
                Some(LuaWake::Abort)
            });
        }
        let Ok(current) = self.current_lua_view_key() else {
            return Ok(Some(LuaWake::Abort));
        };
        if current.view_id != wait.context.view_id {
            return Ok(Some(LuaWake::Abort));
        }
        if current.revision != wait.last_observed.revision
            || current.fingerprint != wait.last_observed.fingerprint
            || current.rows != wait.last_observed.rows
            || current.cols != wait.last_observed.cols
        {
            wait.last_observed = current;
            wait.last_change_ms = now;
            wait.saw_post_input_presentation = true;
        }
        if self.presented_bell_count != wait.last_bell_count {
            wait.last_bell_count = self.presented_bell_count;
            wait.last_change_ms = now;
            wait.saw_post_input_presentation = true;
        }
        let status = self.active_presented_update_status();
        let explicit_close = wait.saw_post_input_presentation && status.synchronized_output_closed;
        let quiet = wait.saw_post_input_presentation
            && !status.application_transaction_open
            && !status.parser_continuation
            && now.saturating_sub(wait.last_change_ms) >= u128::from(wait.quiet_ms);
        if explicit_close || quiet {
            return Ok(Some(stable_wake(
                wait,
                "presented",
                self.presented_bell_count,
            )));
        }
        if now >= wait.timeout_ms {
            return Ok(Some(stable_wake(
                wait,
                "no_response",
                self.presented_bell_count,
            )));
        }
        Ok(None)
    }

    fn capture_lua_view(&mut self, task: &mut LuaTask) -> Result<Table> {
        let key = self.current_lua_view_key()?;
        let token = self.next_lua_token();
        let view = self.presented_accessibility_model_mut();
        let review_position = view.review_cursor_position();
        let application_cursor = view.screen().cursor;
        let mut line_values = Vec::with_capacity(usize::from(key.rows));
        let mut wrapped_values = Vec::with_capacity(usize::from(key.rows));
        for row in 0..key.rows {
            line_values.push(view.line(row));
            wrapped_values.push(view.screen().row_wrapped(row));
        }
        let content = LuaViewContent {
            lines: line_values.into(),
            wrapped: wrapped_values.into(),
        };
        if task.views.len() == MAX_VIEW_SNAPSHOTS {
            let _ = task.views.pop_front();
        }
        task.views.push_back(LuaViewSnapshot {
            token,
            key,
            content: content.clone(),
        });
        let table = task.invocation.lua.create_table()?;
        table.set("token", token)?;
        table.set("rows", key.rows)?;
        table.set("cols", key.cols)?;
        let review = task.invocation.lua.create_table()?;
        review.set("row", review_position.0)?;
        review.set("col", review_position.1)?;
        table.set("review", review)?;
        let cursor = task.invocation.lua.create_table()?;
        cursor.set("row", application_cursor.row)?;
        cursor.set("col", application_cursor.col)?;
        cursor.set("visible", application_cursor.visible)?;
        table.set("cursor", cursor)?;
        let lines = task.invocation.lua.create_table()?;
        let wrapped = task.invocation.lua.create_table()?;
        for (index, line) in content.lines.iter().enumerate() {
            lines.set(index + 1, line.as_str())?;
            wrapped.set(index + 1, content.wrapped[index])?;
        }
        table.set("lines", lines)?;
        table.set("wrapped", wrapped)?;
        Ok(table)
    }

    fn capture_lua_stable_response(
        &mut self,
        task: &mut LuaTask,
        status: &str,
        context: LuaViewKey,
        previous_content: &LuaViewContent,
        bells: u64,
    ) -> Result<Table> {
        let view = self.capture_lua_view(task)?;
        let current = task.views.back().expect("captured Lua view exists");
        let content_changed = current.key.rows != context.rows
            || current.key.cols != context.cols
            || current.content != *previous_content;
        let response = task.invocation.lua.create_table()?;
        response.set("status", status)?;
        response.set("view", view)?;
        response.set("content_changed", content_changed)?;
        let effects = task.invocation.lua.create_table()?;
        effects.set("bells", bells)?;
        response.set("effects", effects)?;
        Ok(response)
    }

    fn current_lua_view_key(&mut self) -> Result<LuaViewKey> {
        if !self.logical_accessibility_view_is_presented() {
            bail!("the active view has not reached the physical terminal");
        }
        let view = self.presented_accessibility_model_mut();
        let (rows, cols) = view.size();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if view.accessibility_revision().is_none() {
            view.contents_full().hash(&mut hasher);
        }
        Ok(LuaViewKey {
            view_id: view.view_id(),
            screen: view.screen().screen,
            revision: view.accessibility_revision(),
            fingerprint: hasher.finish(),
            rows,
            cols,
        })
    }

    fn require_current_lua_view(&mut self, expected: LuaViewKey) -> Result<()> {
        if self.current_lua_view_key()? != expected {
            bail!("view snapshot is stale");
        }
        Ok(())
    }

    fn mapped_lua_text(
        &mut self,
        view_key: LuaViewKey,
        first: LuaCoordinate,
        last: LuaCoordinate,
    ) -> Result<(String, Vec<(usize, LuaCoordinate)>)> {
        validate_range(view_key, first, last)?;
        let view = self.presented_accessibility_model_mut();
        let screen = view.screen();
        let text = screen.contents_between(first.row, first.col, last.row, last.col);
        let mut offsets = Vec::new();
        let mut absolute = 0usize;
        for row in first.row..=last.row {
            let start = if row == first.row { first.col } else { 0 };
            let end = if row == last.row {
                last.col
            } else {
                view_key.cols
            };
            let segment = screen.contents_between(row, start, row, end);
            let mut segment_bytes = 0usize;
            for col in start..end {
                let contents = screen.cell(row, col).map_or("", |cell| cell.contents());
                if contents.is_empty() || segment_bytes >= segment.len() {
                    continue;
                }
                record_coordinate_offset(
                    &mut offsets,
                    absolute.saturating_add(segment_bytes),
                    LuaCoordinate { row, col },
                );
                segment_bytes = segment_bytes
                    .saturating_add(contents.len())
                    .min(segment.len());
            }
            absolute = absolute.saturating_add(segment.len());
            if row != last.row && !screen.row_wrapped(row) {
                absolute = absolute.saturating_add(1);
                record_coordinate_offset(
                    &mut offsets,
                    absolute,
                    LuaCoordinate {
                        row: row + 1,
                        col: 0,
                    },
                );
            }
        }
        Ok((text, offsets))
    }

    fn move_reader_review_cursor(
        &mut self,
        sr: &mut ScreenReader,
        context: LuaViewKey,
        position: LuaCoordinate,
    ) -> Result<()> {
        if self.current_lua_view_key()? != context {
            return Ok(());
        }
        let view = self.presented_accessibility_model_mut();
        let old = view.review_cursor_position();
        let new = (
            position.row.min(context.rows.saturating_sub(1)),
            position.col.min(context.cols.saturating_sub(1)),
        );
        view.set_review_cursor_position(new);
        sr.hook_on_review_cursor_move(old, new)?;
        Ok(())
    }

    fn require_reader(&self, task: &LuaTask, token: u64) -> Result<()> {
        if task
            .reader
            .as_ref()
            .is_none_or(|reader| reader.token != token)
        {
            bail!("reader is closed or belongs to another task");
        }
        Ok(())
    }

    fn finish_lua_reader(&mut self, sr: &mut ScreenReader, task: &mut LuaTask) {
        if let Some(reader) = task.reader.take() {
            sr.set_reader_auto_read_suppressed(false);
            sr.set_auto_read_enabled(reader.saved_auto_read);
        }
    }

    fn finish_lua_task(
        &mut self,
        sr: &mut ScreenReader,
        task: &mut LuaTask,
        cancel_speech: bool,
    ) -> Result<()> {
        let cancel_result = if cancel_speech && matches!(task.wait, LuaWait::Reading(_)) {
            sr.speech_mut().cancel()
        } else {
            Ok(())
        };
        self.finish_lua_reader(sr, task);
        cancel_result.map_err(Into::into)
    }

    fn next_lua_token(&mut self) -> u64 {
        let token = self.next_lua_token;
        self.next_lua_token = self.next_lua_token.wrapping_add(1).max(1);
        token
    }
}

fn lua_coordinate(table: Table) -> anyhow::Result<LuaCoordinate> {
    Ok(LuaCoordinate {
        row: table.get("row")?,
        col: table.get("col")?,
    })
}

fn validate_range(key: LuaViewKey, first: LuaCoordinate, last: LuaCoordinate) -> Result<()> {
    let valid = first.row < key.rows
        && first.col <= key.cols
        && last.row < key.rows
        && last.col <= key.cols
        && (first.row, first.col) <= (last.row, last.col);
    if !valid {
        bail!("reader range is outside the view snapshot");
    }
    Ok(())
}

fn coordinate_for_offset(
    offsets: &[(usize, LuaCoordinate)],
    offset: usize,
    fallback: LuaCoordinate,
) -> LuaCoordinate {
    match offsets.binary_search_by_key(&offset, |(candidate, _)| *candidate) {
        Ok(index) => offsets[index].1,
        Err(0) => fallback,
        Err(index) => offsets[index - 1].1,
    }
}

fn record_coordinate_offset(
    offsets: &mut Vec<(usize, LuaCoordinate)>,
    offset: usize,
    coordinate: LuaCoordinate,
) {
    if let Some((last_offset, last_coordinate)) = offsets.last_mut()
        && *last_offset == offset
    {
        *last_coordinate = coordinate;
    } else {
        offsets.push((offset, coordinate));
    }
}

fn stable_wake(wait: &StableWait, status: &'static str, bell_count: u64) -> LuaWake {
    LuaWake::Stable {
        status,
        context: wait.context,
        content: wait.content.clone(),
        bells: bell_count.saturating_sub(wait.initial_bell_count),
    }
}

fn read_result(
    lua: &mlua::Lua,
    status: &str,
    cause: &str,
    position: LuaCoordinate,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("status", status)?;
    table.set("cause", cause)?;
    let coordinate = lua.create_table()?;
    coordinate.set("row", position.row)?;
    coordinate.set("col", position.col)?;
    table.set("position", coordinate)?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        harness::FakeClock,
        speech::{
            self, ReaderSpeechEvent, ReaderSpeechEventKind, ReaderSupport, protocol::TextPosition,
        },
        views::{PtyView, ViewStack},
    };
    use mlua::{Function, Lua};
    use std::{cell::RefCell, rc::Rc};

    struct ReaderDriver {
        spoken: Rc<RefCell<Vec<String>>>,
        stops: Rc<RefCell<usize>>,
    }

    impl speech::Driver for ReaderDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.spoken.borrow_mut().push(text.to_owned());
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            *self.stops.borrow_mut() += 1;
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn reader_support() -> ReaderSupport {
        ReaderSupport {
            generation: 1,
            reliable_terminal_events: true,
            utf8_word_progress: true,
            confirmed_stop: true,
        }
    }

    type ReaderSetup = (
        App,
        ScreenReader,
        Rc<RefCell<Vec<String>>>,
        Rc<RefCell<usize>>,
        Invocation,
    );

    fn setup(source_after_read: &str) -> ReaderSetup {
        let spoken = Rc::new(RefCell::new(Vec::new()));
        let stops = Rc::new(RefCell::new(0));
        let speech = speech::Speech::new(Box::new(ReaderDriver {
            spoken: Rc::clone(&spoken),
            stops: Rc::clone(&stops),
        }));
        let mut sr = ScreenReader::new(speech);
        sr.set_reader_support(reader_support());
        let stack = ViewStack::new(Box::new(PtyView::new(2, 10)));
        let mut app = App::new(stack).unwrap();
        app.view_stack.root_mut().model().process_changes(b"alpha");

        let lua = Rc::new(Lua::new());
        let source = format!(
            r#"
                return function()
                    local view = coroutine.yield({{__lector_request = "current_view"}})
                    local reader = coroutine.yield({{__lector_request = "reader_acquire"}})
                    local result = coroutine.yield({{
                        __lector_request = "reader_read",
                        reader = reader.reader,
                        view = view.token,
                        first = {{row = 0, col = 0}},
                        last = {{row = 0, col = 5}},
                    }})
                    {source_after_read}
                end
            "#
        );
        let function: Function = lua.load(&source).eval().unwrap();
        let invocation = Invocation::new(Rc::clone(&lua), function).unwrap();
        (app, sr, spoken, stops, invocation)
    }

    fn active_utterance(app: &App) -> crate::speech::protocol::UtteranceId {
        let task = app.lua_task.as_ref().expect("active Lua task");
        let LuaWait::Reading(read) = &task.wait else {
            panic!("Lua task is not reading")
        };
        read.submission.utterance_id.clone()
    }

    #[test]
    fn reader_progress_moves_review_and_completion_restores_owned_state() {
        let (mut app, mut sr, spoken, stops, invocation) = setup(
            r#"
                assert(result.status == "completed")
                coroutine.yield({__lector_request = "reader_close", reader = reader.reader})
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();
        assert!(!sr.auto_read_enabled());
        assert_eq!(spoken.borrow().as_slice(), &["alpha"]);

        let utterance_id = active_utterance(&app);
        sr.push_reader_speech_events([ReaderSpeechEvent {
            utterance_id: utterance_id.clone(),
            kind: ReaderSpeechEventKind::Progress,
            position: Some(TextPosition::Utf8ByteOffset { offset: 2 }),
            reason: None,
        }]);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        assert_eq!(
            app.view_stack.root_mut().model().review_cursor_position(),
            (0, 2)
        );

        sr.push_reader_speech_events([ReaderSpeechEvent {
            utterance_id,
            kind: ReaderSpeechEventKind::Ended,
            position: Some(TextPosition::Utf8ByteOffset { offset: 5 }),
            reason: Some("completed".to_owned()),
        }]);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
        assert!(sr.auto_read_enabled());
        assert_eq!(*stops.borrow(), 0);
    }

    #[test]
    fn reader_symbol_expansion_keeps_review_progress_on_source_coordinates() {
        let (mut app, mut sr, spoken, _stops, invocation) = setup("return");
        sr.speech_mut()
            .set_symbol_level(crate::speech::symbols::Level::All);
        sr.speech_mut().set_symbol(
            ".",
            "dot",
            crate::speech::symbols::Level::Some,
            crate::speech::symbols::IncludeOriginal::Never,
            false,
        );
        app.view_stack
            .root_mut()
            .model()
            .process_changes(b"\r\x1b[2Ka.b");
        let mut pty = Vec::new();
        let mut terminal = Vec::new();

        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();

        assert_eq!(spoken.borrow().as_slice(), ["a dot b"]);
        let utterance_id = active_utterance(&app);
        sr.push_reader_speech_events([ReaderSpeechEvent {
            utterance_id: utterance_id.clone(),
            kind: ReaderSpeechEventKind::Progress,
            position: Some(TextPosition::Utf8ByteOffset { offset: 2 }),
            reason: None,
        }]);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        assert_eq!(
            app.view_stack.root_mut().model().review_cursor_position(),
            (0, 1)
        );

        sr.push_reader_speech_events([ReaderSpeechEvent {
            utterance_id,
            kind: ReaderSpeechEventKind::Ended,
            position: Some(TextPosition::Utf8ByteOffset { offset: 7 }),
            reason: Some("completed".to_owned()),
        }]);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        assert!(app.lua_task.is_none());
    }

    #[test]
    fn lua_error_after_reader_completion_restores_state_and_opens_error_popup() {
        let (mut app, mut sr, spoken, stops, invocation) = setup("error('script exploded')");
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.on_resize(8, 80, &mut terminal).unwrap();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();
        let utterance_id = active_utterance(&app);
        sr.push_reader_speech_events([ReaderSpeechEvent {
            utterance_id,
            kind: ReaderSpeechEventKind::Ended,
            position: Some(TextPosition::Utf8ByteOffset { offset: 5 }),
            reason: Some("completed".to_owned()),
        }]);

        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
        assert!(sr.auto_read_enabled());
        assert_eq!(*stops.borrow(), 0);
        assert!(
            app.present_pending_runtime_error(&mut sr, &mut terminal)
                .unwrap()
        );
        assert_eq!(app.view_stack.active_mut().kind(), views::ViewKind::Popup);
        assert!(spoken.borrow().join(" ").contains("script exploded"));
    }

    #[test]
    fn public_lua_objects_hide_coroutine_requests_from_the_script() {
        let (mut app, mut sr, _spoken, _stops, _unused) = setup("return");
        let lua = Rc::new(Lua::new());
        let sr_ptr = Rc::new(RefCell::new(&mut sr as *mut ScreenReader));
        crate::lua::setup_repl(&lua, sr_ptr).unwrap();
        let function: Function = lua
            .load(
                r#"
                    return function()
                        local view = lector.api.view()
                        assert(view:line(0) == "alpha")
                        local reader = lector.api.reader()
                        local result = reader:read(
                            view,
                            view:top(),
                            {row = 0, col = 5}
                        )
                        assert(result.status == "completed")
                        reader:close()
                    end
                "#,
            )
            .eval()
            .unwrap();
        let invocation = Invocation::new(Rc::clone(&lua), function).unwrap();
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();
        let utterance_id = active_utterance(&app);
        sr.push_reader_speech_events([ReaderSpeechEvent {
            utterance_id,
            kind: ReaderSpeechEventKind::Ended,
            position: Some(TextPosition::Utf8ByteOffset { offset: 5 }),
            reason: Some("completed".to_owned()),
        }]);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
        assert!(sr.auto_read_enabled());
    }

    #[test]
    fn empty_reader_range_completes_without_submitting_speech() {
        let (mut app, mut sr, spoken, _stops, _unused) = setup("return");
        let lua = Rc::new(Lua::new());
        let sr_ptr = Rc::new(RefCell::new(&mut sr as *mut ScreenReader));
        crate::lua::setup_repl(&lua, sr_ptr).unwrap();
        let function: Function = lua
            .load(
                r#"
                    return function()
                        local view = lector.api.view()
                        local reader = lector.api.reader()
                        local result = reader:read(view, view:top(), view:top())
                        assert(result.status == "completed")
                        assert(result.cause == "empty")
                        reader:close()
                    end
                "#,
            )
            .eval()
            .unwrap();
        let invocation = Invocation::new(Rc::clone(&lua), function).unwrap();

        app.start_lua_invocation(&mut sr, invocation, &mut Vec::new(), &mut Vec::new())
            .unwrap();

        assert!(app.lua_task.is_none());
        assert!(spoken.borrow().is_empty());
        assert!(sr.auto_read_enabled());
    }

    #[test]
    fn physical_key_cancellation_resumes_lua_then_restores_owned_state() {
        let (mut app, mut sr, _spoken, stops, invocation) = setup(
            r#"
                assert(result.status == "cancelled")
                assert(result.cause == "key")
                assert(result.position.row == 0)
                assert(result.position.col == 2)
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        sr.set_auto_read_enabled(false);
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();
        sr.set_auto_read_enabled(true);
        assert!(!sr.auto_read_enabled());
        let utterance_id = active_utterance(&app);
        sr.push_reader_speech_events([ReaderSpeechEvent {
            utterance_id,
            kind: ReaderSpeechEventKind::Progress,
            position: Some(TextPosition::Utf8ByteOffset { offset: 2 }),
            reason: None,
        }]);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(
            app.cancel_lua_reader_for_key(&mut sr, &mut pty, &mut terminal)
                .unwrap()
        );
        assert!(app.lua_task.is_none());
        assert!(!sr.auto_read_enabled());
        assert_eq!(*stops.borrow(), 1);
        assert!(pty.is_empty());
    }

    #[test]
    fn physical_key_cancellation_lets_lua_restore_runtime_speech_rate() {
        struct RateDriver {
            rate: Rc<std::cell::Cell<f32>>,
            stops: Rc<std::cell::Cell<usize>>,
        }

        impl speech::Driver for RateDriver {
            fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
                Ok(())
            }

            fn stop(&mut self) -> anyhow::Result<()> {
                self.stops.set(self.stops.get().saturating_add(1));
                Ok(())
            }

            fn get_rate(&self) -> f32 {
                self.rate.get()
            }

            fn set_rate(&mut self, rate: f32) -> anyhow::Result<()> {
                self.rate.set(rate);
                Ok(())
            }
        }

        let rate = Rc::new(std::cell::Cell::new(65.0));
        let stops = Rc::new(std::cell::Cell::new(0));
        let speech = speech::Speech::new(Box::new(RateDriver {
            rate: Rc::clone(&rate),
            stops: Rc::clone(&stops),
        }));
        let mut sr = ScreenReader::new(speech);
        sr.set_reader_support(reader_support());
        let stack = ViewStack::new(Box::new(PtyView::new(2, 10)));
        let mut app = App::new(stack).unwrap();
        app.view_stack.root_mut().model().process_changes(b"alpha");
        let lua = Rc::new(Lua::new());
        let sr_ptr = Rc::new(RefCell::new(&mut sr as *mut ScreenReader));
        crate::lua::setup_repl(&lua, sr_ptr).unwrap();
        let function: Function = lua
            .load(
                r#"
                    return function()
                        local original_rate = lector.o.speech.rate
                        lector.o.speech.rate = 55
                        local view = lector.api.view()
                        local reader = lector.api.reader()
                        local result = reader:read(
                            view,
                            view:top(),
                            {row = 0, col = 5}
                        )
                        assert(result.status == "cancelled")
                        assert(result.cause == "key")
                        lector.o.speech.rate = original_rate
                        reader:close()
                    end
                "#,
            )
            .eval()
            .unwrap();
        let invocation = Invocation::new(Rc::clone(&lua), function).unwrap();
        let mut pty = Vec::new();
        let mut terminal = Vec::new();

        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();
        assert_eq!(rate.get(), 55.0);
        assert!(
            app.cancel_lua_reader_for_key(&mut sr, &mut pty, &mut terminal)
                .unwrap()
        );

        assert!(app.lua_task.is_none());
        assert_eq!(rate.get(), 65.0);
        assert_eq!(stops.get(), 1);
    }

    #[test]
    fn presented_content_change_returns_conservative_reader_cancellation() {
        let (mut app, mut sr, _spoken, stops, invocation) = setup(
            r#"
                assert(result.status == "cancelled")
                assert(result.cause == "screen_changed")
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();
        app.view_stack
            .root_mut()
            .model()
            .process_changes(b"\rbravo");
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
        assert!(sr.auto_read_enabled());
        assert_eq!(*stops.borrow(), 1);
    }

    fn setup_stable_wait(assertions: &str) -> (App, ScreenReader, FakeClock, Invocation) {
        let spoken = Rc::new(RefCell::new(Vec::new()));
        let stops = Rc::new(RefCell::new(0));
        let speech = speech::Speech::new(Box::new(ReaderDriver { spoken, stops }));
        let sr = ScreenReader::new(speech);
        let stack = ViewStack::new(Box::new(PtyView::new(2, 10)));
        let clock = FakeClock::default();
        let mut app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
        app.enable_output_scheduler(crate::output_scheduler::OutputSchedulerConfig::default());
        app.view_stack.root_mut().model().process_changes(b"alpha");
        let mut initial_terminal = Vec::new();
        present_live_view(&mut app, &mut initial_terminal);

        let lua = Rc::new(Lua::new());
        let source = format!(
            r#"
                return function()
                    local view = coroutine.yield({{__lector_request = "current_view"}})
                    local receipt = coroutine.yield({{
                        __lector_request = "send_text",
                        view = view.token,
                        text = "x",
                    }})
                    local stable = coroutine.yield({{
                        __lector_request = "wait_for_stable_screen",
                        receipt = receipt.receipt,
                    }})
                    {assertions}
                end
            "#
        );
        let function: Function = lua.load(&source).eval().unwrap();
        let invocation = Invocation::new(Rc::clone(&lua), function).unwrap();
        (app, sr, clock, invocation)
    }

    fn present_live_view(app: &mut App, terminal: &mut Vec<u8>) {
        app.render_active_view(terminal).unwrap();
        app.drain_scheduled_output(terminal, true).unwrap();
    }

    #[test]
    fn configuration_reload_does_not_replace_a_vm_with_a_suspended_task() {
        let (mut app, mut sr, _clock, invocation) =
            setup_stable_wait("error('task must remain suspended')");
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();
        assert!(app.lua_task.is_some());
        pty.clear();

        app.handle_stdin(&mut sr, b"\x1bR", &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_some());
        assert_eq!(
            app.view_stack.active_mut().kind(),
            views::ViewKind::Terminal
        );
        assert!(pty.is_empty());
    }

    #[test]
    fn stable_wait_requires_a_post_input_screen_and_the_shared_quiet_window() {
        let (mut app, mut sr, clock, invocation) = setup_stable_wait(
            r#"
                assert(stable.status == "presented")
                assert(stable.content_changed)
                assert(stable.effects.bells == 0)
                assert(stable.view.lines[1] == "bravo")
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();
        assert_eq!(pty, b"x");

        // A complete line is evidence of a changed candidate, but unlike
        // auto-read it is not evidence that a whole pager screen is ready.
        app.view_stack
            .root_mut()
            .model()
            .process_changes(b"\rbravo\r\n");
        present_live_view(&mut app, &mut terminal);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        assert!(app.lua_task.is_some());

        clock.advance_ms(u128::from(DIFF_DELAY - 1));
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        assert!(app.lua_task.is_some());

        clock.advance_ms(1);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        assert!(app.lua_task.is_none());
    }

    #[test]
    fn stable_wait_survives_the_expected_parser_to_presentation_gap() {
        let (mut app, mut sr, clock, invocation) = setup_stable_wait(
            r#"
                assert(stable.status == "presented")
                assert(stable.content_changed)
                assert(stable.view.lines[1] == "bravo")
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();

        // A PTY read advances the authoritative parser before the compositor
        // can produce and flush its physical frame. This is the response the
        // receipt is waiting for, not evidence that its view disappeared.
        app.view_stack
            .root_mut()
            .model()
            .process_changes(b"\rbravo");
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        assert!(app.lua_task.is_some());

        present_live_view(&mut app, &mut terminal);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        clock.advance_ms(u128::from(DIFF_DELAY));
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
    }

    #[test]
    fn stable_wait_returns_a_same_view_screen_transition() {
        let (mut app, mut sr, clock, invocation) = setup_stable_wait(
            r#"
                assert(stable.status == "presented")
                assert(stable.content_changed)
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();

        // Entering a full-screen interface selects another terminal screen,
        // but it remains the same input-owning view and is a valid response.
        app.view_stack
            .root_mut()
            .model()
            .process_changes(b"\x1b[?1049hbravo");
        present_live_view(&mut app, &mut terminal);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        clock.advance_ms(u128::from(DIFF_DELAY));
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
    }

    #[test]
    fn stable_wait_reports_a_confirmed_redraw_with_unchanged_readable_content() {
        let (mut app, mut sr, clock, invocation) = setup_stable_wait(
            r#"
                assert(stable.status == "presented")
                assert(not stable.content_changed)
                assert(stable.effects.bells == 0)
                assert(stable.view.lines[1] == "alpha")
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();

        // Model an application which acknowledges the input by repainting the
        // same readable screen. A presentation happened even though paging
        // made no progress.
        app.view_stack
            .root_mut()
            .model()
            .process_changes(b"\r\x1b[2Kalpha");
        present_live_view(&mut app, &mut terminal);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        clock.advance_ms(u128::from(DIFF_DELAY));
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
    }

    #[test]
    fn stable_wait_reports_silence_without_guessing_that_it_is_a_presentation() {
        let (mut app, mut sr, clock, invocation) = setup_stable_wait(
            r#"
                assert(stable.status == "no_response")
                assert(not stable.content_changed)
                assert(stable.effects.bells == 0)
                assert(stable.view.lines[1] == "alpha")
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();

        clock.advance_ms(STABLE_SCREEN_TIMEOUT_MS);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
    }

    #[test]
    fn stable_wait_correlates_physically_flushed_bells_without_changing_content() {
        let (mut app, mut sr, clock, invocation) = setup_stable_wait(
            r#"
                assert(stable.status == "presented")
                assert(not stable.content_changed)
                assert(stable.effects.bells == 2)
            "#,
        );
        let mut pty = Vec::new();
        let mut terminal = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();

        app.emit_physical_bells(&mut terminal, 2).unwrap();
        app.drain_scheduled_output(&mut terminal, true).unwrap();
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        clock.advance_ms(u128::from(DIFF_DELAY));
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
    }

    #[test]
    fn public_screen_response_wraps_its_view_and_compares_readable_content() {
        let spoken = Rc::new(RefCell::new(Vec::new()));
        let stops = Rc::new(RefCell::new(0));
        let speech = speech::Speech::new(Box::new(ReaderDriver { spoken, stops }));
        let mut sr = ScreenReader::new(speech);
        let stack = ViewStack::new(Box::new(PtyView::new(2, 10)));
        let clock = FakeClock::default();
        let mut app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
        app.enable_output_scheduler(crate::output_scheduler::OutputSchedulerConfig::default());
        app.view_stack.root_mut().model().process_changes(b"alpha");
        let mut terminal = Vec::new();
        present_live_view(&mut app, &mut terminal);

        let lua = Rc::new(Lua::new());
        let sr_ptr = Rc::new(RefCell::new(&mut sr as *mut ScreenReader));
        crate::lua::setup_repl(&lua, sr_ptr).unwrap();
        let function: Function = lua
            .load(
                r#"
                    return function()
                        local view = lector.api.view()
                        assert(view.cursor.visible)
                        assert(view.cursor.row == 0 and view.cursor.col == 5)
                        local response = view:send_text("x"):wait_for_stable_screen()
                        assert(response.status == "presented")
                        assert(not response.content_changed)
                        assert(response.effects.bells == 0)
                        assert(response.view:same_content(view))
                    end
                "#,
            )
            .eval()
            .unwrap();
        let invocation = Invocation::new(Rc::clone(&lua), function).unwrap();
        let mut pty = Vec::new();
        app.start_lua_invocation(&mut sr, invocation, &mut pty, &mut terminal)
            .unwrap();

        app.view_stack
            .root_mut()
            .model()
            .process_changes(b"\r\x1b[2Kalpha");
        present_live_view(&mut app, &mut terminal);
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();
        clock.advance_ms(u128::from(DIFF_DELAY));
        app.drive_lua_automation(&mut sr, &mut pty, &mut terminal)
            .unwrap();

        assert!(app.lua_task.is_none());
    }
}
