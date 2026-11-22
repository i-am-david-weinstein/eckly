use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;
use ratatui::widgets::ListState;
use ratatui_image::picker::Picker;
use ratatui_image::sliced::{SignedPosition, SlicedProtocol};
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{ApiError, OAuthDevice, OAuthPoll, ReadeckClient};
use crate::article::{Article, parse_article};
use crate::cache::Cache;
use crate::config::{Config, normalize_server};
use crate::event::{ApiEvent, Event, PendingSyncResult};
use crate::graphics::{self, Graphic};
use crate::input::TextInput;
use crate::models::{
    Annotation, Bookmark, BookmarkPatch, BookmarkQuery, Collection, Filter,
    Label, Profile, SearchQuery, Sort,
};
use crate::speed::{MAX_WPM, MIN_WPM, SpeedReader, WPM_STEP};

const PAGE_SIZE: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Setup,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Sidebar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupField {
    Server,
    Token,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    pub expires: u64,
}

#[derive(Debug, Clone)]
pub struct SetupState {
    pub server: TextInput,
    pub token: TextInput,
    pub focus: SetupField,
    pub error: Option<String>,
    pub busy: bool,
}

impl SetupState {
    pub fn new(config: &Config, error: Option<String>) -> Self {
        let server = TextInput::new(&config.server);
        let mut token = TextInput::masked(&config.token);
        token.cursor = token.char_len();
        let focus = if config.server.trim().is_empty() {
            SetupField::Server
        } else {
            SetupField::Token
        };
        Self {
            server,
            token,
            focus,
            error,
            busy: false,
        }
    }

    fn next_field(&mut self) {
        self.focus = match self.focus {
            SetupField::Server => SetupField::Token,
            SetupField::Token => SetupField::Server,
        };
    }

    fn previous_field(&mut self) {
        self.next_field();
    }

    fn active_input_mut(&mut self) -> &mut TextInput {
        match self.focus {
            SetupField::Server => &mut self.server,
            SetupField::Token => &mut self.token,
        }
    }
}

/// A word marked in the article for a moment, after speed reading stops.
#[derive(Debug, Clone, Copy)]
pub struct FlashWord {
    pub line: usize,
    /// Character offsets in the line text, including its block indent.
    pub start: usize,
    pub end: usize,
    pub until: Instant,
}

/// How long the last speed reading word stays marked while it fades out.
pub const FLASH_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct OAuthState {
    pub server: String,
    pub device: Option<OAuthDevice>,
    pub error: Option<String>,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct Reader {
    pub bookmark: Bookmark,
    pub article: Option<Article>,
    pub lines: Vec<crate::article::WrappedLine>,
    pub width: usize,
    pub scroll: usize,
    pub viewport: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub initial_progress: u8,
    /// Whether the user moved the reading position in this session.
    pub scrolled: bool,
    /// Set after an explicit read/unread toggle; don't overwrite it on exit.
    pub manual_read_state: bool,
    pub images: HashMap<String, DynamicImage>,
    pub image_errors: HashMap<String, String>,
    pub requested_images: HashSet<String>,
    pub show_images: bool,
    /// Maximum wrapping width; 0 uses the full reader viewport.
    pub max_text_width: usize,
    /// Center the constrained reading column when it is narrower.
    pub center_text: bool,
    /// Active in-article search query.
    pub search_query: Option<String>,
    /// Active speed reading session.
    pub speed: Option<SpeedReader>,
    /// Word marked after leaving speed reading, so the reader can find it.
    pub flash: Option<FlashWord>,
    /// Highlights on the article.
    pub annotations: Vec<Annotation>,
    /// Text selection in progress.
    pub selection: Option<Selection>,
    /// Set when `terminal_graphics` is on.
    pub graphics: Option<Picker>,
    graphics_encoded: HashMap<String, Graphic>,
    graphics_encoding: HashSet<(String, usize)>,
}

/// Position in the rendered article text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub line: usize,
    /// Character offset within the line's content.
    pub offset: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub cursor: TextPos,
    pub anchor: Option<TextPos>,
}

impl Reader {
    pub fn new(
        bookmark: Bookmark,
        show_images: bool,
        max_text_width: usize,
        center_text: bool,
    ) -> Self {
        let loading = bookmark.has_article;
        Self {
            initial_progress: bookmark.read_progress,
            scrolled: false,
            manual_read_state: false,
            bookmark,
            article: None,
            lines: Vec::new(),
            width: 0,
            scroll: 0,
            viewport: 20,
            loading,
            error: None,
            images: HashMap::new(),
            image_errors: HashMap::new(),
            requested_images: HashSet::new(),
            show_images,
            max_text_width,
            center_text,
            search_query: None,
            speed: None,
            flash: None,
            annotations: Vec::new(),
            selection: None,
            graphics: None,
            graphics_encoded: HashMap::new(),
            graphics_encoding: HashSet::new(),
        }
    }

    /// Images that need encoding for the current column width. Each one is
    /// returned once until its result arrives.
    pub fn pending_encodes(&mut self) -> Vec<(String, DynamicImage)> {
        if self.graphics.is_none() || !self.show_images {
            return Vec::new();
        }
        let width = self.width;
        let mut pending = Vec::new();
        for (url, image) in &self.images {
            let encoded = self
                .graphics_encoded
                .get(url)
                .is_some_and(|graphic| graphic.width == width);
            if !encoded && self.graphics_encoding.insert((url.clone(), width)) {
                pending.push((url.clone(), image.clone()));
            }
        }
        pending
    }

    pub fn set_graphic(&mut self, url: String, graphic: Graphic) {
        self.graphics_encoding.remove(&(url.clone(), graphic.width));
        if graphic.width != self.width {
            return;
        }
        self.graphics_encoded.insert(url, graphic);
        self.invalidate_layout();
    }

    pub fn set_graphic_error(
        &mut self,
        url: String,
        width: usize,
        error: String,
    ) {
        self.graphics_encoding.remove(&(url.clone(), width));
        self.set_image_error(url, error);
    }

    /// Encoded images overlapping the viewport, positioned relative to the
    /// reading column.
    pub fn visible_graphics(&self) -> Vec<(SignedPosition, &SlicedProtocol)> {
        let Some(article) = &self.article else {
            return Vec::new();
        };
        if !self.show_images {
            return Vec::new();
        }
        let top = self.scroll;
        let bottom = self.scroll + self.viewport;
        let mut visible = Vec::new();
        let mut previous = None;
        for (index, line) in self.lines.iter().enumerate().take(bottom) {
            let first = line.block.is_some() && line.block != previous;
            previous = line.block;
            if !first
                || line.kind != Some(crate::article::BlockKind::Image)
                || line.image.is_some()
            {
                continue;
            }
            let Some(graphic) = line
                .block
                .and_then(|block| article.images.get(&block))
                .and_then(|url| self.graphics_encoded.get(url))
                .filter(|graphic| graphic.width == self.width)
            else {
                continue;
            };
            let size = graphic.protocol.size();
            if index + size.height as usize <= top {
                continue;
            }
            let x = self.width.saturating_sub(size.width as usize) / 2;
            let y = index as isize - top as isize;
            if let (Ok(x), Ok(y)) = (i16::try_from(x), i16::try_from(y)) {
                visible.push((SignedPosition::from((x, y)), &graphic.protocol));
            }
        }
        visible
    }

    fn layout_lines(&self, width: usize) -> Vec<crate::article::WrappedLine> {
        let Some(article) = &self.article else {
            return Vec::new();
        };
        if !self.show_images
            || (self.images.is_empty() && self.image_errors.is_empty())
        {
            return article.layout(width);
        }
        if self.graphics.is_some() {
            let sizes = self
                .graphics_encoded
                .iter()
                .filter(|(_, graphic)| graphic.width == width)
                .map(|(url, graphic)| (url.clone(), graphic.protocol.size()))
                .collect();
            article.layout_with_graphics(width, &sizes, &self.image_errors)
        } else {
            article.layout_with_images(width, &self.images, &self.image_errors)
        }
    }

    pub fn set_article(&mut self, article: Article) {
        self.article = Some(article);
        self.loading = false;
        self.error = None;
        self.speed = None;
        self.flash = None;
        self.selection = None;
        self.images.clear();
        self.image_errors.clear();
        self.requested_images.clear();
        self.graphics_encoded.clear();
        self.graphics_encoding.clear();
        self.lines.clear();
        self.reflow(self.width.max(60));
        self.restore_progress_position();
    }

    fn restore_progress_position(&mut self) {
        let max = self.max_scroll();
        let ratio = self.initial_progress.min(100) as f32 / 100.0;
        self.scroll = (max as f32 * ratio).round() as usize;
        self.scrolled = false;
    }

    pub fn set_image(&mut self, url: String, image: DynamicImage) {
        self.images.insert(url.clone(), image);
        self.image_errors.remove(&url);
        self.invalidate_layout();
    }

    pub fn set_image_error(&mut self, url: String, error: String) {
        self.image_errors.insert(url, error);
        self.invalidate_layout();
    }

    fn position_ratio(&self) -> f32 {
        let max = self.max_scroll();
        if max > 0 {
            self.scroll as f32 / max as f32
        } else {
            0.0
        }
    }

    fn invalidate_layout(&mut self) {
        self.selection = None;
        self.flash = None;
        let ratio = self.position_ratio();
        let width = self.effective_width(self.width.max(60));
        self.width = width;
        self.lines = self.layout_lines(width);
        let max = self.max_scroll();
        self.scroll = ((ratio * max as f32) as usize).min(max);
    }

    fn effective_width(&self, width: usize) -> usize {
        if self.max_text_width == 0 {
            width
        } else {
            width.min(self.max_text_width.max(12))
        }
    }

    pub fn reflow(&mut self, width: usize) {
        let width = self.effective_width(width);
        if self.article.is_none() {
            self.width = width;
            return;
        }
        if width == self.width && !self.lines.is_empty() {
            return;
        }
        self.selection = None;
        let ratio = self.position_ratio();
        self.width = width;
        self.lines = self.layout_lines(width);
        let max = self.max_scroll();
        self.scroll = ((ratio * max as f32) as usize).min(max);
    }

    pub fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport.max(1))
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll() as isize;
        let next = (self.scroll as isize + delta).clamp(0, max) as usize;
        if next != self.scroll {
            self.scroll = next;
            self.scrolled = true;
        }
    }

    /// Jump to a position without treating it as user scrolling.
    pub fn scroll_to(&mut self, position: usize) {
        self.scroll = position.min(self.max_scroll());
    }

    /// Jump to a position and remember that the user moved the reading point.
    pub fn jump_to(&mut self, position: usize) {
        let next = position.min(self.max_scroll());
        if next != self.scroll {
            self.scroll = next;
            self.scrolled = true;
        }
    }

    /// Move to the next block/paragraph start, falling back to a page when
    /// there are no more block boundaries below the current position.
    pub fn scroll_to_next_paragraph(&mut self) {
        let next = self
            .lines
            .iter()
            .enumerate()
            .skip(self.scroll.saturating_add(1))
            .find(|(index, line)| {
                line.kind.is_some()
                    && (*index == 0 || self.lines[*index - 1].kind.is_none())
            })
            .map(|(index, _)| index);

        match next {
            Some(position) => self.jump_to(position),
            None => self.scroll_by(self.page_amount() as isize),
        }
    }

    pub fn page_amount(&self) -> usize {
        self.viewport.saturating_sub(1).max(1)
    }

    pub fn half_page_amount(&self) -> usize {
        (self.viewport / 2).max(1)
    }

    /// Reading position as a percentage, used in the reader footer.
    pub fn progress(&self) -> u8 {
        if self.lines.is_empty() {
            return self.initial_progress;
        }
        let max = self.max_scroll();
        if max == 0 {
            return 100;
        }
        ((self.scroll as f32 / max as f32) * 100.0).round() as u8
    }

    pub fn speed_reading(&self) -> bool {
        self.speed.is_some()
    }

    /// Leave speed reading and move the article view to the word being read.
    pub fn stop_speed_reading(&mut self) {
        let Some(speed) = self.speed.take() else {
            return;
        };
        match speed.current_position() {
            Some((line, start, end)) => {
                let target = line.saturating_sub(self.viewport.max(1) / 2);
                self.jump_to(target);
                self.flash = Some(FlashWord {
                    line,
                    start,
                    end,
                    until: Instant::now() + FLASH_DURATION,
                });
            }
            None => {
                let max = self.max_scroll();
                let position = (speed.ratio() * max as f32).round() as usize;
                self.jump_to(position);
            }
        }
    }

    /// When the next speed reading word is due, if it is playing.
    pub fn speed_frame_deadline(&self) -> Option<Instant> {
        self.speed
            .as_ref()
            .filter(|speed| speed.playing())
            .map(SpeedReader::next_due)
    }

    fn selectable_line(&self, line: usize) -> bool {
        self.lines
            .get(line)
            .is_some_and(|line| line.range.is_some() && line.block.is_some())
    }

    fn line_len(&self, line: usize) -> usize {
        self.lines
            .get(line)
            .and_then(|line| line.range.as_ref())
            .map(|range| range.end - range.start)
            .unwrap_or(0)
    }

    /// Start selecting text at the top of the viewport.
    pub fn start_selection(&mut self) {
        let Some(line) = (self.scroll..self.lines.len())
            .find(|&index| self.selectable_line(index))
        else {
            return;
        };
        self.selection = Some(Selection {
            cursor: TextPos { line, offset: 0 },
            anchor: None,
        });
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        let Some(selection) = self.selection else {
            return;
        };
        let line = selection.cursor.line;
        let viewport = self.viewport.max(1);
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + viewport {
            self.scroll = line + 1 - viewport;
        }
        let max = self.max_scroll();
        self.scroll = self.scroll.min(max);
    }

    pub fn selection_left(&mut self) {
        let Some(mut selection) = self.selection else {
            return;
        };
        if selection.cursor.offset > 0 {
            selection.cursor.offset -= 1;
        } else if let Some(line) = (0..selection.cursor.line)
            .rev()
            .find(|&i| self.selectable_line(i))
        {
            selection.cursor = TextPos {
                line,
                offset: self.line_len(line),
            };
        }
        self.selection = Some(selection);
        self.ensure_selection_visible();
    }

    pub fn selection_right(&mut self) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let len = self.line_len(selection.cursor.line);
        if selection.cursor.offset < len {
            selection.cursor.offset += 1;
        } else if let Some(line) = (selection.cursor.line + 1..self.lines.len())
            .find(|&i| self.selectable_line(i))
        {
            selection.cursor = TextPos { line, offset: 0 };
        }
        self.selection = Some(selection);
        self.ensure_selection_visible();
    }

    fn line_chars(&self, line: usize) -> Vec<char> {
        self.lines
            .get(line)
            .map(|wrapped| wrapped.text.chars().skip(wrapped.indent).collect())
            .unwrap_or_default()
    }

    fn pos_class(&self, pos: TextPos, big: bool) -> u8 {
        match self.line_chars(pos.line).get(pos.offset) {
            None => 0,
            Some(ch) if ch.is_whitespace() => 0,
            Some(_) if big => 1,
            Some(ch) if ch.is_alphanumeric() || *ch == '_' => 1,
            Some(_) => 2,
        }
    }

    fn next_text_pos(&self, pos: TextPos) -> Option<TextPos> {
        if pos.offset < self.line_len(pos.line) {
            return Some(TextPos {
                line: pos.line,
                offset: pos.offset + 1,
            });
        }
        (pos.line + 1..self.lines.len())
            .find(|&line| self.selectable_line(line))
            .map(|line| TextPos { line, offset: 0 })
    }

    fn prev_text_pos(&self, pos: TextPos) -> Option<TextPos> {
        if pos.offset > 0 {
            return Some(TextPos {
                line: pos.line,
                offset: pos.offset - 1,
            });
        }
        (0..pos.line)
            .rev()
            .find(|&line| self.selectable_line(line))
            .map(|line| TextPos {
                line,
                offset: self.line_len(line),
            })
    }

    /// Move by words (`w`/`b`) or WORDS (`W`/`B`).
    pub fn selection_word(&mut self, forward: bool, big: bool) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let mut pos = selection.cursor;
        if forward {
            if self.pos_class(pos, big) != 0 {
                let class = self.pos_class(pos, big);
                while let Some(next) = self.next_text_pos(pos) {
                    pos = next;
                    if self.pos_class(pos, big) != class {
                        break;
                    }
                }
            }
            while self.pos_class(pos, big) == 0 {
                match self.next_text_pos(pos) {
                    Some(next) => pos = next,
                    None => break,
                }
            }
        } else {
            let Some(back) = self.prev_text_pos(pos) else {
                return;
            };
            pos = back;
            while self.pos_class(pos, big) == 0 {
                match self.prev_text_pos(pos) {
                    Some(prev) => pos = prev,
                    None => break,
                }
            }
            let class = self.pos_class(pos, big);
            while let Some(prev) = self.prev_text_pos(pos) {
                if self.pos_class(prev, big) != class {
                    break;
                }
                pos = prev;
            }
        }
        selection.cursor = pos;
        self.selection = Some(selection);
        self.ensure_selection_visible();
    }

    /// Move to the end of the current or next word (`e`/`E`).
    pub fn selection_word_end(&mut self, big: bool) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let mut pos = selection.cursor;
        let class = self.pos_class(pos, big);
        if class != 0 {
            let at_end = match self.next_text_pos(pos) {
                Some(next) => self.pos_class(next, big) != class,
                None => true,
            };
            if at_end {
                if let Some(next) = self.next_text_pos(pos) {
                    pos = next;
                }
                while self.pos_class(pos, big) == 0 {
                    match self.next_text_pos(pos) {
                        Some(next) => pos = next,
                        None => break,
                    }
                }
            }
        } else {
            while self.pos_class(pos, big) == 0 {
                match self.next_text_pos(pos) {
                    Some(next) => pos = next,
                    None => break,
                }
            }
        }
        let class = self.pos_class(pos, big);
        if class != 0 {
            while let Some(next) = self.next_text_pos(pos) {
                if self.pos_class(next, big) != class {
                    break;
                }
                pos = next;
            }
        }
        selection.cursor = pos;
        self.selection = Some(selection);
        self.ensure_selection_visible();
    }

    /// Jump to the start (`0`) or end (`$`) of the line.
    pub fn selection_line_edge(&mut self, end: bool) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let length = self.line_len(selection.cursor.line);
        selection.cursor.offset = if end { length } else { 0 };
        self.selection = Some(selection);
        self.ensure_selection_visible();
    }

    /// Jump to the first non-blank character (`^`).
    pub fn selection_first_non_blank(&mut self) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let offset = self
            .line_chars(selection.cursor.line)
            .iter()
            .position(|ch| !ch.is_whitespace())
            .unwrap_or(0);
        selection.cursor.offset = offset;
        self.selection = Some(selection);
        self.ensure_selection_visible();
    }

    /// Jump to the previous (`{`) or next (`}`) paragraph.
    pub fn selection_block(&mut self, delta: isize) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let current = self.lines[selection.cursor.line].block;
        let target = if delta > 0 {
            (selection.cursor.line + 1..self.lines.len()).find(|&line| {
                self.selectable_line(line) && self.lines[line].block != current
            })
        } else {
            (0..selection.cursor.line).rev().find(|&line| {
                self.selectable_line(line) && self.lines[line].block != current
            })
        };
        match target {
            Some(line) => {
                selection.cursor = TextPos { line, offset: 0 };
            }
            None if delta > 0 => {
                if let Some(line) = (0..self.lines.len())
                    .rev()
                    .find(|&line| self.selectable_line(line))
                {
                    selection.cursor = TextPos {
                        line,
                        offset: self.line_len(line),
                    };
                }
            }
            None => {
                if let Some(line) = (0..self.lines.len())
                    .find(|&line| self.selectable_line(line))
                {
                    selection.cursor = TextPos { line, offset: 0 };
                }
            }
        }
        self.selection = Some(selection);
        self.ensure_selection_visible();
    }

    pub fn selection_move(&mut self, delta: isize) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let mut line = selection.cursor.line;
        for _ in 0..delta.unsigned_abs() {
            let next = if delta >= 0 {
                (line + 1..self.lines.len()).find(|&i| self.selectable_line(i))
            } else {
                (0..line).rev().find(|&i| self.selectable_line(i))
            };
            match next {
                Some(next) => line = next,
                None => break,
            }
        }
        let offset = selection.cursor.offset.min(self.line_len(line));
        selection.cursor = TextPos { line, offset };
        self.selection = Some(selection);
        self.ensure_selection_visible();
    }

    /// Block character for a text position.
    fn block_char(&self, pos: TextPos) -> Option<(usize, usize)> {
        let line = self.lines.get(pos.line)?;
        let range = line.range.as_ref()?;
        let offset = pos.offset.min(range.end - range.start);
        Some((line.block?, range.start + offset))
    }

    /// Ordered selection bounds.
    pub fn selected_bounds(&self) -> Option<(TextPos, TextPos)> {
        let selection = self.selection?;
        let anchor = selection.anchor?;
        Some(if selection.cursor < anchor {
            (selection.cursor, anchor)
        } else {
            (anchor, selection.cursor)
        })
    }

    /// Readeck selectors and offsets for the current selection.
    pub fn annotation_selection(
        &self,
    ) -> Option<(String, usize, String, usize)> {
        let (start, end) = self.selected_bounds()?;
        if start == end {
            return None;
        }
        let article = self.article.as_ref()?;
        let (start_block, start_char) = self.block_char(start)?;
        let (end_block, end_char) = self.block_char(end)?;
        let start_source = article.sources.get(start_block)?.as_ref()?;
        let end_source = article.sources.get(end_block)?.as_ref()?;
        let start_offset = start_source
            .offsets
            .get(start_char)
            .copied()
            .unwrap_or_else(|| {
                start_source.offsets.last().copied().unwrap_or(0)
            });
        let end_offset = match end_source.offsets.get(end_char) {
            Some(offset) => *offset,
            None => end_source.len,
        };
        if start_source.path == end_source.path && start_offset >= end_offset {
            return None;
        }
        Some((
            start_source.path.clone(),
            start_offset,
            end_source.path.clone(),
            end_offset,
        ))
    }

    /// Scroll to the first line of a block.
    pub fn jump_to_block(&mut self, block: usize) -> bool {
        match self.lines.iter().position(|line| line.block == Some(block)) {
            Some(index) => {
                self.jump_to(index);
                true
            }
            None => false,
        }
    }

    /// Character ranges an annotation covers, by block.
    ///
    /// Returns `None` when the annotation targets an element nested inside a
    /// block; callers then fall back to tinting the whole block.
    pub fn annotation_ranges(
        &self,
        annotation: &Annotation,
    ) -> Option<Vec<(usize, usize, usize)>> {
        let article = self.article.as_ref()?;
        let start_index = article.sources.iter().position(|source| {
            source
                .as_ref()
                .is_some_and(|source| source.path == annotation.start_selector)
        })?;
        let start_source = article.sources[start_index].as_ref()?;
        let start_char = start_source
            .offsets
            .iter()
            .position(|offset| *offset >= annotation.start_offset)
            .unwrap_or(start_source.offsets.len());
        let end_index = article
            .sources
            .iter()
            .position(|source| {
                source.as_ref().is_some_and(|source| {
                    source.path == annotation.end_selector
                })
            })
            .unwrap_or(start_index);
        let end_source = article.sources[end_index].as_ref()?;
        let end_char = end_source
            .offsets
            .iter()
            .position(|offset| *offset >= annotation.end_offset)
            .unwrap_or(end_source.offsets.len());

        if start_index == end_index {
            let end_char =
                trim_trailing_space(article, start_index, start_char, end_char);
            if start_char >= end_char {
                return None;
            }
            return Some(vec![(start_index, start_char, end_char)]);
        }

        let (first, first_from, last, last_to) = if start_index < end_index {
            (start_index, start_char, end_index, end_char)
        } else {
            (end_index, end_char, start_index, start_char)
        };
        let last_to = trim_trailing_space(article, last, 0, last_to);
        let mut ranges = Vec::new();
        for block in first..=last {
            let length = article
                .sources
                .get(block)
                .and_then(|source| source.as_ref())
                .map(|source| source.offsets.len())
                .unwrap_or(0);
            let (from, to) = if block == first {
                (first_from, length)
            } else if block == last {
                (0, last_to)
            } else {
                (0, length)
            };
            if from < to {
                ranges.push((block, from, to));
            }
        }
        if ranges.is_empty() {
            None
        } else {
            Some(ranges)
        }
    }

    /// Block index an annotation belongs to.
    pub fn annotation_block(&self, annotation: &Annotation) -> Option<usize> {
        let article = self.article.as_ref()?;
        article.sources.iter().position(|source| {
            source.as_ref().is_some_and(|source| {
                annotation.start_selector == source.path
                    || annotation
                        .start_selector
                        .starts_with(&format!("{}/", source.path))
            })
        })
    }

    fn line_contains(text: &str, query: &str) -> bool {
        text.to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
    }

    pub fn search_match_lines(&self) -> Vec<usize> {
        let Some(query) = self.search_query.as_deref() else {
            return Vec::new();
        };
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| Self::line_contains(&line.text, query))
            .map(|(index, _)| index)
            .collect()
    }

    /// Search the article and jump to the first match at or below the
    /// current reading position. Returns the number of matching lines.
    pub fn search(&mut self, query: &str) -> usize {
        let query = query.trim();
        if query.is_empty() {
            self.search_query = None;
            return 0;
        }
        self.search_query = Some(query.to_string());
        let matches = self.search_match_lines();
        let target = matches
            .iter()
            .copied()
            .find(|&index| index >= self.scroll)
            .or_else(|| matches.first().copied());
        if let Some(index) = target {
            self.jump_to(index);
        }
        matches.len()
    }

    pub fn next_search_match(&mut self, forward: bool) {
        let matches = self.search_match_lines();
        if matches.is_empty() {
            return;
        }
        let target = if forward {
            matches.iter().copied().find(|&index| index > self.scroll)
        } else {
            matches
                .iter()
                .rev()
                .copied()
                .find(|&index| index < self.scroll)
        };
        if let Some(index) = target {
            self.jump_to(index);
        }
    }
}

#[derive(Debug, Clone)]
pub struct SidebarItem {
    pub filter: Option<Filter>,
    pub text: String,
    pub count: Option<usize>,
    pub header: bool,
}

#[derive(Debug)]
pub enum Modal {
    Help,
    Confirm(ConfirmModal),
    Input(InputModal),
    ThemePicker(ThemePicker),
    Annotations(AnnotationsModal),
}

#[derive(Debug)]
pub struct AnnotationsModal {
    pub bookmark_id: String,
    pub selected: usize,
}

#[derive(Debug)]
pub struct ThemePicker {
    pub selected: usize,
    pub original: String,
}

#[derive(Debug)]
pub struct ConfirmModal {
    pub title: String,
    pub message: String,
    pub action: ConfirmAction,
}

#[derive(Debug)]
pub enum ConfirmAction {
    DeleteBookmarks { ids: Vec<String> },
}

#[derive(Debug)]
pub struct InputModal {
    pub title: String,
    pub prompt: String,
    pub input: TextInput,
    pub kind: InputKind,
    pub suggestions: Vec<String>,
    pub completion: Option<LabelCompletion>,
}

impl InputModal {
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        input: TextInput,
        kind: InputKind,
    ) -> Self {
        Self {
            title: title.into(),
            prompt: prompt.into(),
            input,
            kind,
            suggestions: Vec::new(),
            completion: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LabelCompletion {
    pub candidates: Vec<String>,
    pub index: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug)]
pub enum InputKind {
    Search,
    ReaderSearch,
    NewBookmark,
    EditLabels {
        ids: Vec<String>,
    },
    EditNote {
        id: String,
    },
    EditTitle {
        id: String,
    },
    RenameLabel {
        from: String,
    },
    AnnotationNote {
        bookmark_id: String,
        id: String,
        color: String,
    },
}

pub struct App {
    pub screen: Screen,
    pub focus: Focus,
    pub config: Config,
    pub config_path: PathBuf,
    pub client: Option<ReadeckClient>,
    pub setup: Option<SetupState>,
    pub oauth: Option<OAuthState>,
    pub profile: Option<Profile>,
    profile_denied: bool,

    pub bookmarks: Vec<Bookmark>,
    pub selected_ids: HashSet<String>,
    pub list_state: ListState,
    /// Number of two-line bookmark items visible in the list viewport.
    pub list_viewport: usize,
    pub labels: Vec<Label>,
    pub collections: Vec<Collection>,
    pub sidebar_items: Vec<SidebarItem>,
    pub sidebar_state: ListState,
    pub sidebar_viewport: usize,
    pub sidebar_search: TextInput,
    pub sidebar_search_active: bool,
    pub sidebar_hidden: bool,
    pub filter: Filter,
    pub search: String,
    pub sort: Sort,
    pub total: usize,
    pub loading_list: bool,
    pub loading_more: bool,
    pub list_error: Option<String>,

    pub reader: Option<Reader>,
    pub modal: Option<Modal>,
    pub toast: Option<Toast>,
    pub tick: u64,
    pub quit: bool,
    pub images_enabled: bool,
    /// Detected at startup when `terminal_graphics` is on.
    pub picker: Option<Picker>,
    pub offline: bool,
    pub offline_mode: bool,

    cache: Option<Cache>,
    /// Library annotations by bookmark, for the bookmark list.
    annotation_summaries: Option<HashMap<String, Vec<Annotation>>>,
    /// All highlights shown in the Highlights view.
    pub highlights: Vec<Annotation>,
    /// Annotation to scroll to once a bookmark's reader is ready.
    pending_highlight: Option<String>,
    tx: UnboundedSender<Event>,
    list_seq: u64,
    pending_select: Option<String>,
    oauth_seq: u64,
    pending_sync_running: bool,
}

impl App {
    #[cfg(test)]
    pub fn new(
        config: Config,
        config_path: PathBuf,
        tx: UnboundedSender<Event>,
    ) -> Self {
        Self::with_cache(config, config_path, tx, None, false)
    }

    pub fn with_cache(
        config: Config,
        config_path: PathBuf,
        tx: UnboundedSender<Event>,
        cache: Option<Cache>,
        offline_mode: bool,
    ) -> Self {
        let images_enabled = config.images;
        let mut app = Self {
            screen: Screen::Setup,
            focus: Focus::List,
            config,
            config_path,
            client: None,
            setup: None,
            oauth: None,
            profile: None,
            profile_denied: false,
            bookmarks: Vec::new(),
            selected_ids: HashSet::new(),
            list_state: ListState::default(),
            list_viewport: 10,
            labels: Vec::new(),
            collections: Vec::new(),
            sidebar_items: Vec::new(),
            sidebar_state: ListState::default(),
            sidebar_viewport: 10,
            sidebar_search: TextInput::new(""),
            sidebar_search_active: false,
            sidebar_hidden: false,
            filter: Filter::Unread,
            search: String::new(),
            sort: Sort::Newest,
            total: 0,
            loading_list: false,
            loading_more: false,
            list_error: None,
            reader: None,
            modal: None,
            toast: None,
            tick: 0,
            quit: false,
            images_enabled,
            picker: None,
            offline: offline_mode,
            offline_mode,
            cache,
            annotation_summaries: None,
            highlights: Vec::new(),
            pending_highlight: None,
            tx,
            list_seq: 0,
            pending_select: None,
            oauth_seq: 0,
            pending_sync_running: false,
        };
        app.preload_cache();
        app
    }

    fn rebuild_highlights(&mut self) {
        let mut highlights = match &self.annotation_summaries {
            Some(summaries) => summaries.values().flatten().cloned().collect(),
            None => self
                .cache
                .as_ref()
                .map(Cache::all_annotations)
                .unwrap_or_default(),
        };
        for annotation in &mut highlights {
            if annotation.bookmark_title.is_empty() {
                if let Some(bookmark) = self.cache.as_ref().and_then(|cache| {
                    cache.bookmark_ref(&annotation.bookmark_id)
                }) {
                    annotation.bookmark_title = bookmark.display_title();
                    if annotation.bookmark_url.is_empty() {
                        annotation.bookmark_url = bookmark.url.clone();
                    }
                    if annotation.bookmark_site_name.is_empty() {
                        annotation.bookmark_site_name = bookmark.host();
                    }
                }
            }
        }
        // Group by bookmark, most recently highlighted first, and order each
        // group by the highlight's position in the article.
        let mut groups: HashMap<String, Vec<Annotation>> = HashMap::new();
        for annotation in highlights {
            groups
                .entry(annotation.bookmark_id.clone())
                .or_default()
                .push(annotation);
        }
        let mut groups: Vec<Vec<Annotation>> = groups.into_values().collect();
        for group in &mut groups {
            group.sort_by_key(|annotation| {
                (
                    annotation.start_selector.trim().is_empty(),
                    selector_position(&annotation.start_selector),
                    annotation.start_offset,
                    annotation.created,
                )
            });
        }
        groups.sort_by_key(|group| {
            std::cmp::Reverse(group.iter().filter_map(|a| a.created).max())
        });
        self.highlights = groups.into_iter().flatten().collect();
    }

    fn list_len(&self) -> usize {
        if self.filter == Filter::Highlights {
            self.highlights.len()
        } else {
            self.bookmarks.len()
        }
    }

    fn preload_cache(&mut self) {
        let Some(cache) = &self.cache else {
            return;
        };
        let labels = cache.labels();
        let collections = cache.collections();
        let profile = cache.profile();
        if self.filter == Filter::Highlights {
            self.rebuild_highlights();
            self.bookmarks.clear();
            self.total = self.highlights.len();
            self.list_state
                .select((!self.highlights.is_empty()).then_some(0));
            self.list_error = None;
            self.selected_ids.clear();
            return;
        }
        let search = SearchQuery::parse(&self.search);
        let bookmarks = cache.local_bookmarks(&self.filter, &search, self.sort);

        self.labels = labels;
        self.collections = collections;
        if let Some(profile) = profile {
            self.profile = Some(profile);
        }
        if self.filter == Filter::Highlights {
            self.bookmarks.clear();
            self.rebuild_highlights();
            self.total = self.highlights.len();
            self.list_state
                .select((!self.highlights.is_empty()).then_some(0));
            self.rebuild_sidebar();
            return;
        }
        self.total = bookmarks.len();
        self.bookmarks = bookmarks;
        self.list_state
            .select((!self.bookmarks.is_empty()).then_some(0));
        self.rebuild_sidebar();
    }

    fn reload_cache(&mut self) {
        let Some(dir) =
            self.cache.as_ref().map(|cache| cache.dir().to_path_buf())
        else {
            return;
        };
        if let Ok(cache) = Cache::load(dir) {
            self.cache = Some(cache);
            self.preload_cache();
        }
    }

    /// Connect using saved/environment credentials or present the setup form.
    pub fn bootstrap(&mut self) {
        if self.offline_mode {
            self.enter_offline();
            return;
        }
        if !self.config.has_credentials() {
            self.show_setup(None);
            return;
        }

        match ReadeckClient::new(&self.config.server, &self.config.token) {
            Ok(client) => {
                self.client = Some(client);
                self.enter_main();
            }
            Err(err) => self.show_setup(Some(err.to_string())),
        }
    }

    fn enter_offline(&mut self) {
        self.screen = Screen::Main;
        self.setup = None;
        self.focus = Focus::List;
        self.offline = true;
        self.show_cached_bookmarks(false);
        self.rebuild_sidebar();
        if self.bookmarks.is_empty() {
            self.list_error = Some(
                "Offline mode — cache is empty. Run `eckly sync …` to populate it."
                    .into(),
            );
        }
        self.toast_info("Offline mode — using local cache");
    }

    fn enter_main(&mut self) {
        self.screen = Screen::Main;
        self.setup = None;
        self.focus = Focus::List;
        self.request_bookmarks(false, false);
        self.request_labels();
        self.request_collections();
        self.request_annotation_summaries();
        if self.profile.is_none() && !self.profile_denied {
            self.request_profile();
        }
        self.rebuild_sidebar();
        self.flush_pending_changes();
    }

    fn show_setup(&mut self, error: Option<String>) {
        self.screen = Screen::Setup;
        self.client = None;
        if let Some(oauth) = self.oauth.take() {
            oauth.cancel.store(true, Ordering::Relaxed);
        }
        self.oauth_seq = self.oauth_seq.wrapping_add(1);
        self.reader = None;
        self.modal = None;
        self.profile = None;
        self.profile_denied = false;
        self.setup = Some(SetupState::new(&self.config, error));
        self.bookmarks.clear();
        self.selected_ids.clear();
        self.labels.clear();
        self.collections.clear();
        self.sidebar_items.clear();
        self.total = 0;
        self.loading_list = false;
        self.loading_more = false;
        self.list_error = None;
        self.list_state = ListState::default();
        self.sidebar_state = ListState::default();
        self.sidebar_search = TextInput::new("");
        self.sidebar_search_active = false;
    }

    // ------------------------------------------------------------------
    // Event handling
    // ------------------------------------------------------------------

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Tick => {
                self.tick = self.tick.wrapping_add(1);
                if self
                    .toast
                    .as_ref()
                    .is_some_and(|toast| self.tick >= toast.expires)
                {
                    self.toast = None;
                }
            }
            Event::Resize => {}
            Event::Paste(text) => self.handle_paste(&text),
            Event::Key(key) => self.handle_key(key),
            Event::Api(api) => self.handle_api(*api),
        }
    }

    fn handle_paste(&mut self, text: &str) {
        if let Some(Modal::Input(input)) = &mut self.modal {
            input.input.insert_str(text);
            return;
        }
        if self.screen == Screen::Setup {
            if let Some(setup) = &mut self.setup {
                if !setup.busy {
                    setup.active_input_mut().insert_str(text);
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            self.quit = true;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('t')
        {
            if self.modal.is_none() {
                self.open_theme_picker();
            }
            return;
        }

        if self.modal.is_some() {
            self.handle_modal_key(key);
            return;
        }

        if self.screen == Screen::Setup {
            self.handle_setup_key(key);
            return;
        }

        if key.code == KeyCode::Char('?') {
            self.modal = Some(Modal::Help);
            return;
        }

        if self.reader.is_some() {
            self.handle_reader_key(key);
            return;
        }

        match self.focus {
            Focus::Sidebar => self.handle_sidebar_key(key),
            Focus::List => self.handle_list_key(key),
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else {
            return;
        };

        match modal {
            Modal::Help => {
                if !matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
                ) {
                    self.modal = Some(Modal::Help);
                }
            }
            Modal::Confirm(confirm) => match key.code {
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {}
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm(confirm.action);
                }
                _ => self.modal = Some(Modal::Confirm(confirm)),
            },
            Modal::Input(mut input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.submit_input(input),
                KeyCode::Tab
                    if matches!(input.kind, InputKind::EditLabels { .. }) =>
                {
                    self.complete_label(&mut input);
                    self.modal = Some(Modal::Input(input));
                }
                _ => {
                    let label_input =
                        matches!(input.kind, InputKind::EditLabels { .. });
                    let jump = if label_input
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        match key.code {
                            KeyCode::Left => Some(false),
                            KeyCode::Right => Some(true),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    match jump {
                        Some(forward) => {
                            input.input.cursor = label_jump(
                                &input.input.value,
                                input.input.cursor,
                                forward,
                            );
                        }
                        None => {
                            input.input.handle_edit_key(&key);
                        }
                    }
                    if label_input {
                        self.refresh_label_suggestions(&mut input);
                    }
                    self.modal = Some(Modal::Input(input));
                }
            },
            Modal::Annotations(mut annotations) => {
                let list = self
                    .reader
                    .as_ref()
                    .map(|reader| reader.annotations.clone())
                    .unwrap_or_default();
                let count = list.len();
                let mut reopen = true;
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => reopen = false,
                    KeyCode::Down | KeyCode::Char('j') => {
                        if count > 0 {
                            annotations.selected =
                                (annotations.selected + 1).min(count - 1);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        annotations.selected =
                            annotations.selected.saturating_sub(1);
                    }
                    KeyCode::Char('n') => {
                        if let Some(reader) = &mut self.reader {
                            reader.start_selection();
                        }
                        reopen = false;
                    }
                    KeyCode::Char('e') => {
                        if let Some(annotation) = list.get(annotations.selected)
                        {
                            self.edit_annotation_note(
                                &annotations.bookmark_id,
                                annotation,
                            );
                            reopen = false;
                        }
                    }
                    KeyCode::Char('c') => {
                        if let Some(annotation) = list.get(annotations.selected)
                        {
                            let color =
                                next_annotation_color(annotation.color());
                            self.update_annotation(
                                annotations.bookmark_id.clone(),
                                annotation.id.clone(),
                                color.to_string(),
                                annotation.note.clone(),
                            );
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(annotation) = list.get(annotations.selected)
                        {
                            self.delete_annotation(
                                annotations.bookmark_id.clone(),
                                annotation.id.clone(),
                            );
                            annotations.selected =
                                annotations.selected.saturating_sub(1);
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(annotation) = list.get(annotations.selected)
                        {
                            let block =
                                self.reader.as_ref().and_then(|reader| {
                                    reader.annotation_block(annotation)
                                });
                            match block {
                                Some(block) => {
                                    if let Some(reader) = &mut self.reader {
                                        reader.jump_to_block(block);
                                    }
                                    reopen = false;
                                }
                                None => self.toast_info(
                                    "This annotation is not in the article",
                                ),
                            }
                        }
                    }
                    _ => {}
                }
                if reopen {
                    self.modal = Some(Modal::Annotations(annotations));
                }
            }
            Modal::ThemePicker(mut picker) => {
                let themes = crate::ui::theme::all();
                let len = themes.len().max(1);
                let mut close = false;
                let mut preview = false;
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        let _ = crate::ui::theme::set_name(&picker.original);
                        close = true;
                    }
                    KeyCode::Enter => {
                        if let Some((id, _)) = themes.get(picker.selected) {
                            let _ = crate::ui::theme::set_name(id);
                            self.config.theme = (*id).to_string();
                            let name = themes[picker.selected].1;
                            self.toast_info(format!("Theme: {name}"));
                        }
                        close = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        picker.selected = (picker.selected + 1) % len;
                        preview = true;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        picker.selected = (picker.selected + len - 1) % len;
                        preview = true;
                    }
                    KeyCode::PageDown => {
                        picker.selected = (picker.selected + 10).min(len - 1);
                        preview = true;
                    }
                    KeyCode::PageUp => {
                        picker.selected = picker.selected.saturating_sub(10);
                        preview = true;
                    }
                    KeyCode::Home | KeyCode::Char('g') => {
                        picker.selected = 0;
                        preview = true;
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        picker.selected = len - 1;
                        preview = true;
                    }
                    _ => {}
                }

                if preview {
                    if let Some((id, _)) = themes.get(picker.selected) {
                        let _ = crate::ui::theme::set_name(id);
                    }
                }
                if !close {
                    self.modal = Some(Modal::ThemePicker(picker));
                }
            }
        }
    }

    fn handle_setup_key(&mut self, key: KeyEvent) {
        if self.oauth.is_some() {
            if key.code == KeyCode::Esc {
                self.cancel_oauth();
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('o')
        {
            self.start_oauth_login();
            return;
        }

        let mut submit = false;
        let mut quit = false;
        if let Some(setup) = &mut self.setup {
            if setup.busy {
                return;
            }
            match key.code {
                KeyCode::Esc => quit = true,
                KeyCode::Tab | KeyCode::Down => setup.next_field(),
                KeyCode::BackTab | KeyCode::Up => setup.previous_field(),
                KeyCode::Enter => submit = true,
                _ => {
                    setup.active_input_mut().handle_edit_key(&key);
                }
            }
        }
        if quit {
            self.quit = true;
        }
        if submit {
            self.submit_setup();
        }
    }

    fn submit_setup(&mut self) {
        let Some(setup) = &self.setup else {
            return;
        };
        if setup.busy {
            return;
        }
        let server = setup.server.value.trim().to_string();
        let token = setup.token.value.trim().to_string();

        if server.is_empty() {
            self.set_setup_error("Enter your Readeck server URL.");
            if let Some(setup) = &mut self.setup {
                setup.focus = SetupField::Server;
            }
            return;
        }
        if token.is_empty() {
            self.set_setup_error("Enter an API token.");
            if let Some(setup) = &mut self.setup {
                setup.focus = SetupField::Token;
            }
            return;
        }

        match ReadeckClient::new(&server, &token) {
            Ok(client) => {
                self.client = Some(client.clone());
                if let Some(setup) = &mut self.setup {
                    setup.busy = true;
                    setup.error = None;
                }
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Event::api(ApiEvent::Setup(
                        client.profile().await,
                    )));
                });
            }
            Err(err) => self.set_setup_error(err.to_string()),
        }
    }

    fn start_oauth_login(&mut self) {
        if self.oauth.is_some() {
            return;
        }
        let Some(setup) = &self.setup else {
            return;
        };
        if setup.busy {
            return;
        }
        let server_input = setup.server.value.trim().to_string();
        if server_input.is_empty() {
            self.set_setup_error("Enter your Readeck server URL first.");
            if let Some(setup) = &mut self.setup {
                setup.focus = SetupField::Server;
            }
            return;
        }
        let server = match normalize_server(&server_input) {
            Ok(server) => server,
            Err(err) => {
                self.set_setup_error(err.to_string());
                return;
            }
        };
        let client = match ReadeckClient::new_oauth(&server) {
            Ok(client) => client,
            Err(err) => {
                self.set_setup_error(err.to_string());
                return;
            }
        };

        self.oauth_seq = self.oauth_seq.wrapping_add(1);
        let seq = self.oauth_seq;
        let cancel = Arc::new(AtomicBool::new(false));
        self.oauth = Some(OAuthState {
            server,
            device: None,
            error: None,
            cancel: cancel.clone(),
        });
        if let Some(setup) = &mut self.setup {
            setup.busy = true;
            setup.error = None;
        }

        let tx = self.tx.clone();
        tokio::spawn(async move {
            run_oauth_login(client, seq, cancel, tx).await;
        });
    }

    fn cancel_oauth(&mut self) {
        if let Some(oauth) = self.oauth.take() {
            oauth.cancel.store(true, Ordering::Relaxed);
        }
        self.oauth_seq = self.oauth_seq.wrapping_add(1);
        if let Some(setup) = &mut self.setup {
            setup.busy = false;
        }
    }

    fn set_setup_error(&mut self, message: impl Into<String>) {
        if let Some(setup) = &mut self.setup {
            setup.error = Some(message.into());
        }
    }

    fn complete_setup(
        &mut self,
        profile: Option<Profile>,
        profile_denied: bool,
    ) {
        let (server, token) = self
            .setup
            .as_ref()
            .map(|setup| {
                (
                    setup.server.value.trim().to_string(),
                    setup.token.value.trim().to_string(),
                )
            })
            .unwrap_or_default();
        self.finish_login(server, token, profile, profile_denied);
    }

    fn finish_login(
        &mut self,
        server: String,
        token: String,
        profile: Option<Profile>,
        profile_denied: bool,
    ) {
        let server = normalize_server(&server).unwrap_or(server);
        self.config.server = server;
        self.config.token = token;
        match ReadeckClient::new(&self.config.server, &self.config.token) {
            Ok(client) => self.client = Some(client),
            Err(err) => {
                self.show_setup(Some(err.to_string()));
                return;
            }
        }
        self.profile = profile;
        self.profile_denied = profile_denied;
        let save_result = self.config.save(&self.config_path);
        self.setup = None;
        self.oauth = None;
        self.enter_main();
        match save_result {
            Ok(()) if !self.profile_denied => {
                self.toast_success("Connected to Readeck")
            }
            Ok(()) => {
                self.toast_info("Connected (token has no profile access)")
            }
            Err(err) => self.toast_error(format!(
                "Connected, but could not save config: {err:#}"
            )),
        }
    }

    fn handle_sidebar_key(&mut self, key: KeyEvent) {
        if self.sidebar_search_active {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let half = (self.sidebar_viewport / 2).max(1);
                match key.code {
                    KeyCode::Char('d') => {
                        self.sidebar_move(half as isize);
                        return;
                    }
                    KeyCode::Char('u') => {
                        self.sidebar_move(-(half as isize));
                        return;
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Esc => {
                    self.clear_sidebar_search();
                    return;
                }
                KeyCode::Enter => {
                    self.sidebar_search_active = false;
                    self.apply_sidebar_selection();
                    return;
                }
                KeyCode::Down => {
                    self.sidebar_move(1);
                    return;
                }
                KeyCode::Up => {
                    self.sidebar_move(-1);
                    return;
                }
                KeyCode::PageDown => {
                    let page = self.sidebar_viewport.max(1);
                    self.sidebar_move(page as isize);
                    return;
                }
                KeyCode::PageUp => {
                    let page = self.sidebar_viewport.max(1);
                    self.sidebar_move(-(page as isize));
                    return;
                }
                _ => {}
            }

            if self.sidebar_search.handle_edit_key(&key) {
                self.rebuild_sidebar();
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let half = (self.sidebar_viewport / 2).max(1);
            match key.code {
                KeyCode::Char('d') => {
                    self.sidebar_move(half as isize);
                    return;
                }
                KeyCode::Char('u') => {
                    self.sidebar_move(-(half as isize));
                    return;
                }
                _ => return,
            }
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc
            | KeyCode::Tab
            | KeyCode::Char('h')
            | KeyCode::Left => {
                if !self.sidebar_search.value.is_empty() {
                    self.clear_sidebar_search();
                } else {
                    self.focus = Focus::List;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.sidebar_move(1),
            KeyCode::Up | KeyCode::Char('k') => self.sidebar_move(-1),
            KeyCode::PageDown => {
                let page = self.sidebar_viewport.max(1);
                self.sidebar_move(page as isize);
            }
            KeyCode::PageUp => {
                let page = self.sidebar_viewport.max(1);
                self.sidebar_move(-(page as isize));
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.sidebar_select_edge(true)
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.sidebar_select_edge(false)
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                self.apply_sidebar_selection()
            }
            KeyCode::Char('r') => self.refresh_all(),
            KeyCode::Char('R') => self.rename_selected_label(),
            KeyCode::Char('L') => self.toggle_sidebar(),
            KeyCode::Char('/') => {
                self.sidebar_search_active = true;
                self.sidebar_search.cursor = self.sidebar_search.char_len();
            }
            _ => {}
        }
    }

    fn focus_sidebar(&mut self) {
        if self.sidebar_hidden {
            return;
        }
        self.focus = Focus::Sidebar;
        if !self.sidebar_items.is_empty()
            && self.sidebar_state.selected().is_none()
        {
            self.sidebar_select_edge(true);
        }
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_hidden = !self.sidebar_hidden;
        if self.sidebar_hidden {
            self.focus = Focus::List;
        } else {
            self.focus_sidebar();
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let half = (self.list_viewport / 2).max(1);
            match key.code {
                KeyCode::Char('d') => {
                    self.list_move(half as isize);
                    return;
                }
                KeyCode::Char('u') => {
                    self.list_move(-(half as isize));
                    return;
                }
                KeyCode::Char('a') => {
                    self.select_all_bookmarks();
                    return;
                }
                _ => return,
            }
        }

        if self.filter == Filter::Highlights {
            match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('L') => self.toggle_sidebar(),
                KeyCode::Esc
                | KeyCode::Tab
                | KeyCode::Char('h')
                | KeyCode::Left => self.focus_sidebar(),
                KeyCode::Down | KeyCode::Char('j') => self.list_move(1),
                KeyCode::Up | KeyCode::Char('k') => self.list_move(-1),
                KeyCode::PageDown => {
                    let page = self.list_viewport.max(1);
                    self.list_move(page as isize);
                }
                KeyCode::PageUp => {
                    let page = self.list_viewport.max(1);
                    self.list_move(-(page as isize));
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    self.list_select_edge(true)
                }
                KeyCode::Char('G') | KeyCode::End => {
                    self.list_select_edge(false)
                }
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                    self.open_selected_highlight()
                }
                KeyCode::Char('D') => self.delete_selected_highlight(),
                KeyCode::Char('r') => self.refresh_all(),
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('L') => self.toggle_sidebar(),
            KeyCode::Esc => {
                if !self.search.is_empty() {
                    self.search.clear();
                    self.selected_ids.clear();
                    self.request_bookmarks(false, false);
                    self.toast_info("Search cleared");
                } else if !self.selected_ids.is_empty() {
                    self.selected_ids.clear();
                    self.toast_info("Selection cleared");
                } else {
                    self.focus_sidebar();
                }
            }
            KeyCode::Tab | KeyCode::Char('h') | KeyCode::Left => {
                self.focus_sidebar()
            }
            KeyCode::Down | KeyCode::Char('j') => self.list_move(1),
            KeyCode::Up | KeyCode::Char('k') => self.list_move(-1),
            KeyCode::Char(' ') => self.toggle_selection(),
            KeyCode::PageDown => {
                let page = self.list_viewport.max(1);
                self.list_move(page as isize);
            }
            KeyCode::PageUp => {
                let page = self.list_viewport.max(1);
                self.list_move(-(page as isize));
            }
            KeyCode::Char('g') | KeyCode::Home => self.list_select_edge(true),
            KeyCode::Char('G') | KeyCode::End => self.list_select_edge(false),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                self.open_reader()
            }
            KeyCode::Char('/') => self.open_search(),
            KeyCode::Char('s') => self.cycle_sort(true),
            KeyCode::Char('S') => self.cycle_sort(false),
            KeyCode::Char('r') => self.refresh_all(),
            KeyCode::Char('n') => self.open_new_bookmark(),
            KeyCode::Char('o') => self.open_in_browser(),
            KeyCode::Char('m') => self.toggle_read(),
            KeyCode::Char('f') => self.toggle_favorite(),
            KeyCode::Char('a') => self.toggle_archive(),
            KeyCode::Char('D') => self.confirm_delete(),
            KeyCode::Char('t') => self.edit_labels(),
            KeyCode::Char('T') => self.edit_title(),
            KeyCode::Char('E') => self.edit_note(),
            _ => {}
        }
    }

    fn handle_reader_key(&mut self, key: KeyEvent) {
        if self
            .reader
            .as_ref()
            .is_some_and(|reader| reader.selection.is_some())
        {
            self.handle_selection_key(key);
            return;
        }
        if self.reader.as_ref().is_some_and(Reader::speed_reading) {
            self.handle_speed_read_key(key);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let half = self
                .reader
                .as_ref()
                .map(Reader::half_page_amount)
                .unwrap_or(10);
            match key.code {
                KeyCode::Char('d') => {
                    self.reader_scroll(half as isize);
                    return;
                }
                KeyCode::Char('u') => {
                    self.reader_scroll(-(half as isize));
                    return;
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc => {
                let has_results = self.reader.as_ref().is_some_and(|reader| {
                    reader.search_query.is_some()
                        && !reader.search_match_lines().is_empty()
                });
                if has_results {
                    if let Some(reader) = &mut self.reader {
                        reader.search_query = None;
                    }
                    self.toast_info("Article search cleared");
                } else {
                    self.close_reader();
                }
            }
            KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
                self.close_reader();
            }
            KeyCode::Char('v') => self.start_highlight_selection(),
            KeyCode::Down | KeyCode::Char('j') => self.reader_scroll(1),
            KeyCode::Up | KeyCode::Char('k') => self.reader_scroll(-1),
            KeyCode::Char(' ') => {
                if let Some(reader) = &mut self.reader {
                    reader.scroll_to_next_paragraph();
                }
            }
            KeyCode::PageDown => {
                let amount =
                    self.reader.as_ref().map(Reader::page_amount).unwrap_or(20);
                self.reader_scroll(amount as isize);
            }
            KeyCode::PageUp | KeyCode::Char('b') => {
                let amount =
                    self.reader.as_ref().map(Reader::page_amount).unwrap_or(20);
                self.reader_scroll(-(amount as isize));
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(reader) = &mut self.reader {
                    reader.jump_to(0);
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(reader) = &mut self.reader {
                    let max = reader.max_scroll();
                    reader.jump_to(max);
                }
            }
            KeyCode::Char('m') => self.toggle_read(),
            KeyCode::Char('f') => self.toggle_favorite(),
            KeyCode::Char('a') => {
                let archived = self
                    .reader
                    .as_ref()
                    .is_some_and(|reader| reader.bookmark.is_archived);
                if self.config.archive_closes_reader && !archived {
                    self.archive_and_close_reader();
                } else {
                    self.toggle_archive();
                }
            }
            KeyCode::Char('o') => self.open_in_browser(),
            KeyCode::Char('t') => self.edit_labels(),
            KeyCode::Char('T') => self.edit_title(),
            KeyCode::Char('E') => self.edit_note(),
            KeyCode::Char('i') => self.toggle_reader_images(),
            KeyCode::Char('A') => self.toggle_reader_alignment(),
            KeyCode::Char('s') => self.toggle_speed_reading(),
            KeyCode::Char('H') => self.open_annotations(),
            KeyCode::Char('D') => self.confirm_delete(),
            KeyCode::Char('/') => self.open_reader_search(),
            KeyCode::Char('n') => {
                if let Some(reader) = &mut self.reader {
                    reader.next_search_match(true);
                }
            }
            KeyCode::Char('N') => {
                if let Some(reader) = &mut self.reader {
                    reader.next_search_match(false);
                }
            }
            KeyCode::Char('r') => self.retry_article(),
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Bookmarks and reader
    // ------------------------------------------------------------------

    pub fn selected_bookmark(&self) -> Option<Bookmark> {
        let index = self.list_state.selected()?;
        self.bookmarks.get(index).cloned()
    }

    fn current_bookmark(&self) -> Option<Bookmark> {
        self.reader
            .as_ref()
            .map(|reader| reader.bookmark.clone())
            .or_else(|| self.selected_bookmark())
    }

    /// Bookmarks targeted by an action: the reader's bookmark, the current
    /// bulk selection, or the highlighted bookmark as a fallback.
    fn action_targets(&self) -> Vec<Bookmark> {
        if let Some(reader) = &self.reader {
            return vec![reader.bookmark.clone()];
        }
        if !self.selected_ids.is_empty() {
            let selected: Vec<Bookmark> = self
                .bookmarks
                .iter()
                .filter(|bookmark| self.selected_ids.contains(&bookmark.id))
                .cloned()
                .collect();
            if !selected.is_empty() {
                return selected;
            }
        }
        self.selected_bookmark().into_iter().collect()
    }

    fn toggle_selection(&mut self) {
        let Some(bookmark) = self.selected_bookmark() else {
            return;
        };
        if !self.selected_ids.remove(&bookmark.id) {
            self.selected_ids.insert(bookmark.id);
        }
    }

    fn select_all_bookmarks(&mut self) {
        if self.bookmarks.is_empty() {
            return;
        }
        self.selected_ids = self
            .bookmarks
            .iter()
            .map(|bookmark| bookmark.id.clone())
            .collect();
        self.toast_info(format!(
            "Selected {} bookmarks",
            self.selected_ids.len()
        ));
    }

    fn list_move(&mut self, delta: isize) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.list_state.select(Some(next));
        self.maybe_load_more();
    }

    fn list_select_edge(&mut self, start: bool) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        if start {
            self.list_state.select(Some(0));
            return;
        }
        self.list_state.select(Some(len - 1));
        if self.filter != Filter::Highlights
            && self.bookmarks.len() < self.total
        {
            self.request_bookmarks(true, false);
        }
    }

    fn maybe_load_more(&mut self) {
        if self.filter == Filter::Highlights {
            return;
        }
        let Some(index) = self.list_state.selected() else {
            return;
        };
        if self.bookmarks.len() < self.total
            && index + 3 >= self.bookmarks.len()
            && !self.loading_more
            && !self.loading_list
        {
            self.request_bookmarks(true, false);
        }
    }

    fn request_bookmarks(&mut self, append: bool, preserve_selection: bool) {
        if self.filter == Filter::Highlights {
            self.loading_list = false;
            self.loading_more = false;
            self.list_error = None;
            self.rebuild_highlights();
            self.bookmarks.clear();
            self.total = self.highlights.len();
            self.list_state
                .select((!self.highlights.is_empty()).then_some(0));
            return;
        }
        if self.offline_mode {
            if !append {
                self.list_seq = self.list_seq.wrapping_add(1);
                self.pending_select = if preserve_selection {
                    self.selected_bookmark().map(|bookmark| bookmark.id)
                } else {
                    None
                };
            }
            self.loading_list = false;
            self.loading_more = false;
            self.show_cached_bookmarks(false);
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        if append {
            if self.loading_more || self.loading_list {
                return;
            }
            if self.total == 0 || self.bookmarks.len() >= self.total {
                return;
            }
        } else {
            self.list_seq += 1;
            self.pending_select = if preserve_selection {
                self.selected_bookmark().map(|bookmark| bookmark.id)
            } else {
                None
            };
            if !preserve_selection {
                self.selected_ids.clear();
            }
            self.loading_list = true;
            self.loading_more = false;
            self.list_error = None;
        }

        let offset = if append { self.bookmarks.len() } else { 0 };
        let seq = self.list_seq;
        let search = SearchQuery::parse(&self.search);
        let query = BookmarkQuery::new(
            &self.filter,
            &search,
            self.sort,
            PAGE_SIZE,
            offset,
        );
        if append {
            self.loading_more = true;
        }

        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.list_bookmarks(&query).await;
            let _ = tx.send(Event::api(ApiEvent::Bookmarks {
                seq,
                append,
                result,
            }));
        });
    }

    fn request_labels(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ =
                tx.send(Event::api(ApiEvent::Labels(client.labels().await)));
        });
    }

    fn request_annotation_summaries(&mut self) {
        if self.offline_mode || self.offline {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut annotations = Vec::new();
            let mut offset = 0;
            loop {
                match client.annotations_page(100, offset).await {
                    Ok(page) => {
                        if page.items.is_empty() {
                            break;
                        }
                        let count = page.items.len();
                        annotations.extend(page.items);
                        offset += count;
                        if offset >= page.total {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Event::api(
                            ApiEvent::AnnotationSummaries(Err(err)),
                        ));
                        return;
                    }
                }
            }
            let _ = tx.send(Event::api(ApiEvent::AnnotationSummaries(Ok(
                annotations,
            ))));
        });
    }

    pub fn annotations_for(&self, bookmark_id: &str) -> &[Annotation] {
        if let Some(map) = &self.annotation_summaries {
            return map.get(bookmark_id).map(Vec::as_slice).unwrap_or(&[]);
        }
        self.cache
            .as_ref()
            .and_then(|cache| cache.annotations_ref(bookmark_id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn request_collections(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Event::api(ApiEvent::Collections(
                client.collections().await,
            )));
        });
    }

    fn request_profile(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ =
                tx.send(Event::api(ApiEvent::Profile(client.profile().await)));
        });
    }

    fn open_reader(&mut self) {
        let Some(bookmark) = self.selected_bookmark() else {
            return;
        };
        self.open_bookmark(bookmark, None);
    }

    fn open_selected_highlight(&mut self) {
        let Some(annotation) = self
            .list_state
            .selected()
            .and_then(|index| self.highlights.get(index))
            .cloned()
        else {
            return;
        };
        let bookmark = self.bookmark_for_annotation(&annotation);
        if bookmark.id.is_empty() {
            self.toast_error("This highlight has no bookmark");
            return;
        }
        self.open_bookmark(bookmark, Some(annotation.id));
    }

    fn delete_selected_highlight(&mut self) {
        let Some(annotation) = self
            .list_state
            .selected()
            .and_then(|index| self.highlights.get(index))
            .cloned()
        else {
            return;
        };
        if annotation.bookmark_id.is_empty() {
            self.toast_error("This highlight has no bookmark");
            return;
        }
        self.delete_annotation(annotation.bookmark_id, annotation.id);
    }

    fn remove_annotation_summary(&mut self, bookmark_id: &str, id: &str) {
        let Some(map) = &mut self.annotation_summaries else {
            return;
        };
        let Some(annotations) = map.get_mut(bookmark_id) else {
            return;
        };
        annotations.retain(|annotation| annotation.id != id);
        if annotations.is_empty() {
            map.remove(bookmark_id);
        }
    }

    fn remove_cached_annotation(&mut self, bookmark_id: &str, id: &str) {
        let Some(cache) = &mut self.cache else {
            return;
        };
        let Some(annotations) = cache.annotations_ref(bookmark_id).cloned()
        else {
            return;
        };
        let kept: Vec<Annotation> = annotations
            .into_iter()
            .filter(|annotation| annotation.id != id)
            .collect();
        cache.set_annotations(bookmark_id, kept);
        let _ = cache.save();
    }

    fn bookmark_for_annotation(&self, annotation: &Annotation) -> Bookmark {
        if let Some(bookmark) = self
            .cache
            .as_ref()
            .and_then(|cache| cache.bookmark_ref(&annotation.bookmark_id))
        {
            return bookmark.clone();
        }
        if let Some(bookmark) = self
            .bookmarks
            .iter()
            .find(|bookmark| bookmark.id == annotation.bookmark_id)
        {
            return bookmark.clone();
        }
        Bookmark {
            id: annotation.bookmark_id.clone(),
            title: (!annotation.bookmark_title.is_empty())
                .then(|| annotation.bookmark_title.clone()),
            url: annotation.bookmark_url.clone(),
            site_name: (!annotation.bookmark_site_name.is_empty())
                .then(|| annotation.bookmark_site_name.clone()),
            has_article: true,
            ..Default::default()
        }
    }

    fn apply_pending_highlight(&mut self) {
        let Some(id) = self.pending_highlight.clone() else {
            return;
        };
        let block = {
            let Some(reader) = &mut self.reader else {
                return;
            };
            if reader.lines.is_empty() {
                return;
            }
            reader
                .annotations
                .iter()
                .find(|annotation| annotation.id == id)
                .and_then(|annotation| reader.annotation_block(annotation))
        };
        let Some(block) = block else {
            return;
        };
        if self
            .reader
            .as_mut()
            .is_some_and(|reader| reader.jump_to_block(block))
        {
            self.pending_highlight = None;
        }
    }

    fn open_bookmark(&mut self, bookmark: Bookmark, highlight: Option<String>) {
        self.pending_highlight = highlight;
        let mut reader = Reader::new(
            bookmark.clone(),
            self.images_enabled,
            self.config.text_width,
            !self.config.text_align.trim().eq_ignore_ascii_case("left"),
        );
        reader.graphics = self.picker.clone();
        reader.annotations = self
            .cache
            .as_ref()
            .map(|cache| cache.annotations(&bookmark.id))
            .unwrap_or_default();
        if self.offline_mode {
            let cached = self
                .cache
                .as_ref()
                .and_then(|cache| cache.read_article(&bookmark.id));
            if let Some(html) = cached {
                reader.set_article(parse_article(&html));
            } else if bookmark
                .description
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
                || bookmark
                    .note
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty())
            {
                reader.set_article(fallback_article(&bookmark));
            } else {
                reader.loading = false;
                reader.error = Some(
                    "Offline mode: no cached copy of this article is available."
                        .into(),
                );
            }
            self.reader = Some(reader);
            self.apply_pending_highlight();
            return;
        }
        if bookmark.has_article {
            self.reader = Some(reader);
            self.request_article(&bookmark.id);
        } else {
            reader.set_article(fallback_article(&bookmark));
            self.reader = Some(reader);
        }
        self.request_annotations(&bookmark.id);
        self.apply_pending_highlight();
    }

    fn request_article(&mut self, id: &str) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if let Some(reader) = &mut self.reader {
            reader.loading = true;
            reader.error = None;
        }
        let id = id.to_string();
        let tx = self.tx.clone();
        let cache_dir =
            self.cache.as_ref().map(|cache| cache.dir().to_path_buf());
        let requested = id.clone();
        tokio::spawn(async move {
            let result = match client.article(&requested).await {
                Ok(html) => {
                    if let Some(dir) = &cache_dir {
                        let _ =
                            Cache::write_article_to_dir(dir, &requested, &html);
                    }
                    Ok(parse_article(&html))
                }
                Err(err) => Err(err),
            };
            let _ = tx.send(Event::api(ApiEvent::Article { id, result }));
        });
    }

    fn request_annotations(&mut self, bookmark_id: &str) {
        if self.offline_mode || self.offline {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let bookmark_id = bookmark_id.to_string();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.annotations(&bookmark_id).await;
            let _ = tx.send(Event::api(ApiEvent::Annotations {
                bookmark_id,
                result,
            }));
        });
    }

    fn start_highlight_selection(&mut self) {
        let started = {
            let Some(reader) = &mut self.reader else {
                return;
            };
            reader.start_selection();
            reader.selection.is_some()
        };
        if !started {
            self.toast_info("Nothing to highlight here");
        }
    }

    fn open_annotations(&mut self) {
        let Some(reader) = &self.reader else {
            return;
        };
        let bookmark_id = reader.bookmark.id.clone();
        self.modal = Some(Modal::Annotations(AnnotationsModal {
            bookmark_id,
            selected: 0,
        }));
    }

    fn update_annotation(
        &mut self,
        bookmark_id: String,
        id: String,
        color: String,
        note: String,
    ) {
        if self.offline_mode || self.offline {
            self.toast_error("Annotations require a connection to Readeck");
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client
                .update_annotation(&bookmark_id, &id, &color, &note)
                .await;
            let _ = tx.send(Event::api(ApiEvent::AnnotationUpdated {
                bookmark_id,
                id,
                color,
                note,
                result,
            }));
        });
    }

    fn delete_annotation(&mut self, bookmark_id: String, id: String) {
        if self.offline_mode || self.offline {
            self.toast_error("Annotations require a connection to Readeck");
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.delete_annotation(&bookmark_id, &id).await;
            let _ = tx.send(Event::api(ApiEvent::AnnotationDeleted {
                bookmark_id,
                id,
                result,
            }));
        });
    }

    fn store_annotation_summaries(&mut self) {
        let Some(reader) = &self.reader else {
            return;
        };
        let bookmark = reader.bookmark.clone();
        let mut annotations = reader.annotations.clone();
        // A bookmark's annotation list does not repeat the bookmark, so fill
        // in what the library-wide list provides.
        for annotation in &mut annotations {
            if annotation.bookmark_id.is_empty() {
                annotation.bookmark_id = bookmark.id.clone();
            }
            if annotation.bookmark_title.is_empty() {
                annotation.bookmark_title = bookmark.display_title();
            }
            if annotation.bookmark_url.is_empty() {
                annotation.bookmark_url = bookmark.url.clone();
            }
            if annotation.bookmark_site_name.is_empty() {
                annotation.bookmark_site_name = bookmark.host();
            }
        }
        if let Some(map) = &mut self.annotation_summaries {
            if annotations.is_empty() {
                map.remove(&bookmark.id);
            } else {
                map.insert(bookmark.id, annotations);
            }
        }
    }

    fn store_reader_annotations(&mut self) {
        let Some(reader) = &self.reader else {
            return;
        };
        let bookmark_id = reader.bookmark.id.clone();
        let annotations = reader.annotations.clone();
        if let Some(cache) = &mut self.cache {
            cache.set_annotations(&bookmark_id, annotations);
            let _ = cache.save();
        }
    }

    fn edit_annotation_note(
        &mut self,
        bookmark_id: &str,
        annotation: &Annotation,
    ) {
        self.modal = Some(Modal::Input(InputModal::new(
            "Annotation note",
            "Note for this annotation (empty clears it)",
            TextInput::new(annotation.note.clone()),
            InputKind::AnnotationNote {
                bookmark_id: bookmark_id.to_string(),
                id: annotation.id.clone(),
                color: annotation.color().to_string(),
            },
        )));
    }

    fn request_images_for_reader(
        &mut self,
        bookmark_id: String,
        urls: Vec<String>,
    ) {
        let missing = {
            let Some(reader) = &mut self.reader else {
                return;
            };
            let missing: Vec<String> = urls
                .into_iter()
                .filter(|url| !reader.requested_images.contains(url))
                .collect();
            for url in &missing {
                reader.requested_images.insert(url.clone());
            }
            missing
        };
        if !missing.is_empty() {
            self.spawn_image_requests(bookmark_id, missing);
        }
    }

    fn spawn_image_requests(&mut self, bookmark_id: String, urls: Vec<String>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        for url in urls {
            let client = client.clone();
            let tx = self.tx.clone();
            let bookmark_id = bookmark_id.clone();
            let requested_url = url.clone();
            tokio::spawn(async move {
                let result = match client.fetch_media(&requested_url).await {
                    Ok(bytes) => image::load_from_memory(&bytes)
                        .map_err(|err| ApiError::Image(err.to_string())),
                    Err(err) => Err(err),
                };
                let _ = tx.send(Event::api(ApiEvent::Image {
                    bookmark_id,
                    url,
                    result,
                }));
            });
        }
    }

    /// Encode loaded images for the terminal graphics protocol in the
    /// background, at the reader's current column width.
    pub fn spawn_image_encodes(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        let Some(picker) = reader.graphics.clone() else {
            return;
        };
        let bookmark_id = reader.bookmark.id.clone();
        let width = reader.width;
        for (url, image) in reader.pending_encodes() {
            let tx = self.tx.clone();
            let picker = picker.clone();
            let bookmark_id = bookmark_id.clone();
            tokio::task::spawn_blocking(move || {
                let result = graphics::encode(&picker, image, width);
                let _ = tx.send(Event::api(ApiEvent::ImageEncoded {
                    bookmark_id,
                    url,
                    width,
                    result,
                }));
            });
        }
    }

    fn toggle_reader_alignment(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        reader.center_text = !reader.center_text;
        let center = reader.center_text;
        self.config.text_align = if center {
            "center".to_string()
        } else {
            "left".to_string()
        };
        self.toast_info(format!(
            "Text alignment: {}",
            if center { "center" } else { "left" }
        ));
    }

    fn toggle_reader_images(&mut self) {
        self.images_enabled = !self.images_enabled;
        let enabled = self.images_enabled;

        let (bookmark_id, urls) = {
            let Some(reader) = &mut self.reader else {
                return;
            };
            reader.show_images = enabled;
            reader.invalidate_layout();
            let bookmark_id = reader.bookmark.id.clone();
            let urls = if enabled {
                reader
                    .article
                    .as_ref()
                    .map(|article| {
                        let mut urls = article
                            .images
                            .values()
                            .cloned()
                            .collect::<HashSet<_>>()
                            .into_iter()
                            .collect::<Vec<_>>();
                        urls.sort();
                        urls.truncate(12);
                        urls
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            (bookmark_id, urls)
        };

        if enabled {
            self.request_images_for_reader(bookmark_id, urls);
        }
        self.toast_info(if enabled {
            "Images enabled"
        } else {
            "Images disabled"
        });
    }

    fn retry_article(&mut self) {
        let Some(reader) = &self.reader else {
            return;
        };
        if reader.loading {
            return;
        }
        let id = reader.bookmark.id.clone();
        self.request_article(&id);
    }

    fn close_reader(&mut self) {
        if let Some((id, patch)) = self.take_reader() {
            self.update_bookmark(id, patch, "", true);
        }
    }

    fn archive_and_close_reader(&mut self) {
        let Some((id, mut patch)) = self.take_reader() else {
            return;
        };
        patch.is_archived = Some(true);
        self.update_bookmark(id, patch, "Bookmark archived", false);
    }

    /// Closes the reader and returns its bookmark id with the reading
    /// progress patch to send, which is empty when nothing changed.
    fn take_reader(&mut self) -> Option<(String, BookmarkPatch)> {
        let mut reader = self.reader.take()?;
        reader.stop_speed_reading();
        self.pending_highlight = None;
        if self.filter == Filter::Highlights {
            self.rebuild_highlights();
            self.total = self.highlights.len();
            let selected = self.list_state.selected().unwrap_or(0);
            self.list_state.select(
                (!self.highlights.is_empty())
                    .then_some(selected.min(self.highlights.len() - 1)),
            );
        }
        let progress = reader.progress();
        let mut patch = BookmarkPatch::default();
        if reader.scrolled
            && !reader.manual_read_state
            && !reader.loading
            && reader.error.is_none()
            && progress != reader.initial_progress
        {
            patch.read_progress = Some(progress);
        }
        Some((reader.bookmark.id, patch))
    }

    fn reader_scroll(&mut self, delta: isize) {
        if let Some(reader) = &mut self.reader {
            reader.scroll_by(delta);
        }
    }

    fn handle_selection_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                if let Some(reader) = &mut self.reader {
                    reader.selection = None;
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_left();
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_right();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_move(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_move(-1);
                }
            }
            KeyCode::Char('w') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_word(true, false);
                }
            }
            KeyCode::Char('W') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_word(true, true);
                }
            }
            KeyCode::Char('b') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_word(false, false);
                }
            }
            KeyCode::Char('e') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_word_end(false);
                }
            }
            KeyCode::Char('E') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_word_end(true);
                }
            }
            KeyCode::Char('B') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_word(false, true);
                }
            }
            KeyCode::Char('0') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_line_edge(false);
                }
            }
            KeyCode::Char('$') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_line_edge(true);
                }
            }
            KeyCode::Char('^') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_first_non_blank();
                }
            }
            KeyCode::Char('{') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_block(-1);
                }
            }
            KeyCode::Char('}') => {
                if let Some(reader) = &mut self.reader {
                    reader.selection_block(1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.selection_press(),
            _ => {}
        }
    }

    fn selection_press(&mut self) {
        let (range, bookmark_id, started) = {
            let Some(reader) = &mut self.reader else {
                return;
            };
            let Some(mut selection) = reader.selection else {
                return;
            };
            if selection.anchor.is_none() {
                selection.anchor = Some(selection.cursor);
                reader.selection = Some(selection);
                (None, String::new(), true)
            } else {
                let range = reader.annotation_selection();
                let bookmark_id = reader.bookmark.id.clone();
                reader.selection = None;
                (range, bookmark_id, false)
            }
        };
        if started {
            self.toast_info(
                "Select text with h/j/k/l, then press Enter to annotate",
            );
            return;
        }
        match range {
            Some((start_selector, start_offset, end_selector, end_offset)) => {
                self.create_annotation_span(
                    &bookmark_id,
                    (start_selector, start_offset),
                    (end_selector, end_offset),
                );
            }
            None => self.toast_error("Select at least one character"),
        }
    }

    fn create_annotation_span(
        &mut self,
        bookmark_id: &str,
        start: (String, usize),
        end: (String, usize),
    ) {
        if self.offline_mode || self.offline {
            self.toast_error("Annotations require a connection to Readeck");
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let bookmark_id = bookmark_id.to_string();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client
                .create_annotation(
                    &bookmark_id,
                    (&start.0, start.1),
                    (&end.0, end.1),
                    "yellow",
                    "",
                )
                .await;
            let _ = tx.send(Event::api(ApiEvent::AnnotationCreated {
                bookmark_id,
                result,
            }));
        });
    }

    fn handle_speed_read_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('s') => self.stop_speed_reading(),
            KeyCode::Char(' ') => self.toggle_speed_play(),
            KeyCode::Char('h') | KeyCode::Left => self.step_speed_reading(-1),
            KeyCode::Char('l') | KeyCode::Right => self.step_speed_reading(1),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.adjust_speed_wpm(WPM_STEP)
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                self.adjust_speed_wpm(-WPM_STEP)
            }
            _ => {}
        }
    }

    fn toggle_speed_reading(&mut self) {
        let wpm = self.config.speed_read_wpm;
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        if reader.speed.is_some() {
            reader.stop_speed_reading();
        } else if !reader.lines.is_empty() {
            reader.flash = None;
            let speed = SpeedReader::new(&reader.lines, reader.scroll, wpm);
            reader.speed = speed;
        }
    }

    fn stop_speed_reading(&mut self) {
        if let Some(reader) = self.reader.as_mut() {
            reader.stop_speed_reading();
        }
    }

    fn toggle_speed_play(&mut self) {
        if let Some(speed) = self.reader.as_mut().and_then(|r| r.speed.as_mut())
        {
            speed.toggle_play(Instant::now());
        }
    }

    fn step_speed_reading(&mut self, delta: isize) {
        if let Some(speed) = self.reader.as_mut().and_then(|r| r.speed.as_mut())
        {
            speed.step(delta);
        }
    }

    fn adjust_speed_wpm(&mut self, delta: i16) {
        let wpm = {
            let Some(speed) =
                self.reader.as_mut().and_then(|r| r.speed.as_mut())
            else {
                return;
            };
            let wpm = (i32::from(speed.wpm()) + i32::from(delta))
                .clamp(i32::from(MIN_WPM), i32::from(MAX_WPM))
                as u16;
            speed.set_wpm(wpm, Instant::now());
            wpm
        };
        self.config.speed_read_wpm = wpm;
    }

    /// Advance the speed reader when its next word is due.
    pub fn advance_speed_reading(&mut self) {
        if let Some(speed) = self.reader.as_mut().and_then(|r| r.speed.as_mut())
        {
            speed.advance(Instant::now());
        }
    }

    /// When the speed reader needs to redraw, if it is playing.
    pub fn speed_frame_deadline(&self) -> Option<Instant> {
        self.reader.as_ref().and_then(Reader::speed_frame_deadline)
    }

    // ------------------------------------------------------------------
    // Bookmark actions
    // ------------------------------------------------------------------

    fn require_online(&mut self) -> bool {
        if self.offline_mode || self.offline {
            self.toast_error(
                "Adding bookmarks requires a connection to Readeck.",
            );
            false
        } else {
            true
        }
    }

    fn toggle_read(&mut self) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let mark_unread = targets.iter().all(Bookmark::is_read);
        let count = targets.len();
        let patch = BookmarkPatch {
            read_progress: Some(if mark_unread { 0 } else { 100 }),
            ..Default::default()
        };
        let message = match (mark_unread, count) {
            (true, 1) => "Marked as unread".to_string(),
            (true, n) => format!("Marked {n} bookmarks as unread"),
            (false, 1) => "Marked as read".to_string(),
            (false, n) => format!("Marked {n} bookmarks as read"),
        };
        let ids = targets.into_iter().map(|bookmark| bookmark.id).collect();
        self.update_ids(ids, patch, message, false);
    }

    fn toggle_favorite(&mut self) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let remove = targets.iter().all(|bookmark| bookmark.is_marked);
        let count = targets.len();
        let patch = BookmarkPatch {
            is_marked: Some(!remove),
            ..Default::default()
        };
        let message = match (remove, count) {
            (true, 1) => "Removed from favorites".to_string(),
            (true, n) => format!("Removed {n} bookmarks from favorites"),
            (false, 1) => "Added to favorites".to_string(),
            (false, n) => format!("Added {n} bookmarks to favorites"),
        };
        let ids = targets.into_iter().map(|bookmark| bookmark.id).collect();
        self.update_ids(ids, patch, message, false);
    }

    fn toggle_archive(&mut self) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let unarchive = targets.iter().all(|bookmark| bookmark.is_archived);
        let count = targets.len();
        let patch = BookmarkPatch {
            is_archived: Some(!unarchive),
            ..Default::default()
        };
        let message = match (unarchive, count) {
            (true, 1) => "Bookmark restored from archive".to_string(),
            (true, n) => format!("Restored {n} bookmarks from archive"),
            (false, 1) => "Bookmark archived".to_string(),
            (false, n) => format!("Archived {n} bookmarks"),
        };
        let ids = targets.into_iter().map(|bookmark| bookmark.id).collect();
        self.update_ids(ids, patch, message, false);
    }

    fn update_ids(
        &mut self,
        ids: Vec<String>,
        patch: BookmarkPatch,
        message: impl Into<String>,
        quiet: bool,
    ) {
        let message = message.into();
        if ids.is_empty() || patch.is_empty() {
            return;
        }
        if ids.len() == 1 && self.selected_ids.is_empty() {
            self.update_bookmark(
                ids.into_iter().next().unwrap(),
                patch,
                message,
                quiet,
            );
        } else {
            self.update_bookmarks_bulk(ids, patch, message, quiet);
        }
    }

    fn update_bookmarks_bulk(
        &mut self,
        ids: Vec<String>,
        patch: BookmarkPatch,
        message: String,
        quiet: bool,
    ) {
        if self.offline_mode || self.offline {
            let count = ids.len();
            self.apply_patch_to_ids(&ids, &patch);
            if let Some(cache) = &mut self.cache {
                for id in &ids {
                    cache.queue_patch(id, &patch);
                }
                let _ = cache.save();
            }
            if !quiet {
                self.toast_info(format!(
                    "Offline — {count} change(s) saved locally"
                ));
            }
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        if !quiet {
            self.toast_info(format!("Updating {} bookmarks…", ids.len()));
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.update_bookmarks(&ids, &patch).await;
            let _ = tx.send(Event::api(ApiEvent::BulkUpdated {
                ids,
                patch,
                message,
                quiet,
                result,
            }));
        });
    }

    fn update_bookmark(
        &mut self,
        id: String,
        patch: BookmarkPatch,
        message: impl Into<String>,
        quiet: bool,
    ) {
        if patch.is_empty() {
            return;
        }
        if self.offline_mode || self.offline {
            self.apply_patch(&id, &patch);
            if let Some(cache) = &mut self.cache {
                cache.queue_patch(&id, &patch);
                let _ = cache.save();
            }
            if !quiet {
                self.toast_info("Offline — change saved locally");
            }
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let message = message.into();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.update_bookmark(&id, &patch).await;
            let _ = tx.send(Event::api(ApiEvent::Updated {
                id,
                patch,
                message,
                quiet,
                result,
            }));
        });
    }

    fn delete_offline(&mut self, ids: Vec<String>) {
        let count = ids.len();
        self.remove_bookmarks(&ids);
        if let Some(cache) = &mut self.cache {
            for id in &ids {
                cache.queue_delete(id);
            }
            let _ = cache.save();
        }
        if count == 1 {
            self.toast_info("Offline — deletion queued");
        } else {
            self.toast_info(format!("Offline — {count} deletions queued"));
        }
    }

    fn confirm_delete(&mut self) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let count = targets.len();
        let ids = targets
            .iter()
            .map(|bookmark| bookmark.id.clone())
            .collect::<Vec<_>>();
        let (title, message) = if count == 1 {
            let title = targets[0].display_title();
            (
                "Delete bookmark".to_string(),
                format!(
                    "Delete “{title}”?\nThis removes the bookmark and its article from Readeck."
                ),
            )
        } else {
            (
                format!("Delete {count} bookmarks"),
                format!(
                    "Delete {count} selected bookmarks?\nThis removes them and their articles from Readeck."
                ),
            )
        };
        self.modal = Some(Modal::Confirm(ConfirmModal {
            title,
            message,
            action: ConfirmAction::DeleteBookmarks { ids },
        }));
    }

    fn confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteBookmarks { ids } => self.start_delete(ids),
        }
    }

    fn start_delete(&mut self, ids: Vec<String>) {
        if self.offline_mode || self.offline {
            self.delete_offline(ids);
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        if ids.len() == 1 && self.selected_ids.is_empty() {
            let id = ids.into_iter().next().unwrap();
            let title = self
                .bookmarks
                .iter()
                .find(|bookmark| bookmark.id == id)
                .map(Bookmark::display_title)
                .unwrap_or_else(|| "bookmark".to_string());
            let tx = self.tx.clone();
            let deleted_id = id.clone();
            tokio::spawn(async move {
                let result = client.delete_bookmark(&deleted_id).await;
                let _ = tx.send(Event::api(ApiEvent::Deleted { id, result }));
            });
            self.toast_info(format!("Deleting “{title}”…"));
        } else {
            let count = ids.len();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let result = client.delete_bookmarks(&ids).await;
                let _ =
                    tx.send(Event::api(ApiEvent::BulkDeleted { ids, result }));
            });
            if count == 1 {
                self.toast_info("Deleting bookmark…");
            } else {
                self.toast_info(format!("Deleting {count} bookmarks…"));
            }
        }
    }

    fn open_in_browser(&mut self) {
        let Some(bookmark) = self.current_bookmark() else {
            return;
        };
        if bookmark.url.trim().is_empty() {
            self.toast_error("This bookmark has no URL.");
            return;
        }
        if let Err(err) = open::that(&bookmark.url) {
            self.toast_error(format!("Could not open browser: {err}"));
        }
    }

    fn open_new_bookmark(&mut self) {
        if !self.require_online() {
            return;
        }
        self.modal = Some(Modal::Input(InputModal::new(
            "Add bookmark",
            "URL to save in Readeck",
            TextInput::new(""),
            InputKind::NewBookmark,
        )));
    }

    fn open_search(&mut self) {
        self.modal = Some(Modal::Input(InputModal::new(
            "Search",
            "Full text search (empty clears)",
            TextInput::new(&self.search),
            InputKind::Search,
        )));
    }

    fn open_reader_search(&mut self) {
        let Some(reader) = &self.reader else {
            return;
        };
        let prefill = reader.search_query.clone().unwrap_or_default();
        self.modal = Some(Modal::Input(InputModal::new(
            "Search article",
            "Search this article (empty clears)",
            TextInput::new(prefill),
            InputKind::ReaderSearch,
        )));
    }

    fn edit_labels(&mut self) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let count = targets.len();
        let ids = targets
            .iter()
            .map(|bookmark| bookmark.id.clone())
            .collect::<Vec<_>>();

        let sorted_labels = targets
            .iter()
            .map(|bookmark| {
                let mut labels = bookmark.labels.clone();
                labels.sort();
                labels
            })
            .collect::<Vec<_>>();
        let labels_match =
            sorted_labels.windows(2).all(|pair| pair[0] == pair[1]);
        let prefill = if labels_match {
            targets[0].labels.join(", ")
        } else {
            String::new()
        };
        let prompt = if count > 1 && !labels_match {
            format!(
                "Replace labels on {count} bookmarks (current labels differ)"
            )
        } else {
            "Comma separated labels".to_string()
        };
        let title = if count > 1 {
            format!("Edit labels for {count} bookmarks")
        } else {
            "Edit labels".to_string()
        };

        let mut modal = InputModal::new(
            title,
            prompt,
            TextInput::new(prefill),
            InputKind::EditLabels { ids },
        );
        self.refresh_label_suggestions(&mut modal);
        self.modal = Some(Modal::Input(modal));
    }

    fn rename_selected_label(&mut self) {
        let name = self
            .sidebar_items
            .get(self.sidebar_state.selected().unwrap_or(0))
            .and_then(|item| match &item.filter {
                Some(Filter::Label(name)) => Some(name.clone()),
                _ => None,
            });
        let Some(name) = name else {
            self.toast_info("Select a label to rename");
            return;
        };
        self.modal = Some(Modal::Input(InputModal::new(
            "Rename label",
            "New label name",
            TextInput::new(name.clone()),
            InputKind::RenameLabel { from: name },
        )));
    }

    fn rename_label(&mut self, from: String, to: String) {
        if self.offline_mode || self.offline {
            self.toast_error(
                "Renaming a label requires a connection to Readeck",
            );
            return;
        }
        let Some(client) = self.client.clone() else {
            self.toast_error(
                "Renaming a label requires a connection to Readeck",
            );
            return;
        };
        self.toast_info(format!("Renaming #{from}…"));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.rename_label(&from, &to).await;
            let _ = tx.send(Event::api(ApiEvent::LabelRenamed {
                from,
                to,
                result,
            }));
        });
    }

    fn apply_label_rename(&mut self, from: &str, to: &str) {
        let mut labels: Vec<Label> = Vec::with_capacity(self.labels.len());
        for mut label in self.labels.drain(..) {
            if label.name == from {
                label.name = to.to_string();
            }
            match labels.iter_mut().find(|kept| kept.name == label.name) {
                Some(existing) => existing.count += label.count,
                None => labels.push(label),
            }
        }
        self.labels = labels;

        for bookmark in &mut self.bookmarks {
            for label in &mut bookmark.labels {
                if label == from {
                    *label = to.to_string();
                }
            }
            let mut seen = HashSet::new();
            bookmark.labels.retain(|label| seen.insert(label.clone()));
        }

        if let Filter::Label(name) = &mut self.filter {
            if name == from {
                *name = to.to_string();
            }
        }

        if let Some(cache) = &mut self.cache {
            cache.rename_label(from, to);
            let _ = cache.save();
        }

        self.rebuild_sidebar();
        let renamed = Filter::Label(to.to_string());
        if let Some(index) = self
            .sidebar_items
            .iter()
            .position(|item| item.filter.as_ref() == Some(&renamed))
        {
            self.sidebar_state.select(Some(index));
            *self.sidebar_state.offset_mut() = 0;
        }
    }

    fn refresh_label_suggestions(&self, modal: &mut InputModal) {
        if !matches!(modal.kind, InputKind::EditLabels { .. }) {
            modal.suggestions.clear();
            modal.completion = None;
            return;
        }
        modal.suggestions = label_suggestions(
            &self.labels,
            &modal.input.value,
            modal.input.cursor,
        );
        modal.completion = None;
    }

    fn complete_label(&self, modal: &mut InputModal) {
        if let Some(completion) = modal.completion.clone() {
            if completion.candidates.is_empty() {
                return;
            }
            let next = (completion.index + 1) % completion.candidates.len();
            let label = completion.candidates[next].clone();
            modal
                .input
                .replace_range(completion.start, completion.end, &label);
            let end = completion.start + label.chars().count();
            modal.completion = Some(LabelCompletion {
                candidates: completion.candidates,
                index: next,
                start: completion.start,
                end,
            });
            return;
        }

        let candidates = label_suggestions(
            &self.labels,
            &modal.input.value,
            modal.input.cursor,
        );
        if candidates.is_empty() {
            return;
        }
        let (start, end, _) =
            label_token(&modal.input.value, modal.input.cursor);
        let label = candidates[0].clone();
        modal.input.replace_range(start, end, &label);
        let end = start + label.chars().count();
        modal.suggestions = candidates.clone();
        modal.completion = Some(LabelCompletion {
            candidates,
            index: 0,
            start,
            end,
        });
    }

    fn edit_note(&mut self) {
        let Some(bookmark) = self.current_bookmark() else {
            return;
        };
        let note = bookmark.note.clone().unwrap_or_default();
        if note.contains('\n') {
            self.toast_info(
                "Multi-line notes can be edited in the Readeck web UI.",
            );
            return;
        }
        self.modal = Some(Modal::Input(InputModal::new(
            "Edit note",
            "Note (empty clears it)",
            TextInput::new(note),
            InputKind::EditNote { id: bookmark.id },
        )));
    }

    fn edit_title(&mut self) {
        let Some(bookmark) = self.current_bookmark() else {
            return;
        };
        self.modal = Some(Modal::Input(InputModal::new(
            "Edit title",
            "Bookmark title",
            TextInput::new(bookmark.display_title()),
            InputKind::EditTitle { id: bookmark.id },
        )));
    }

    fn submit_input(&mut self, modal: InputModal) {
        let value = modal.input.value.trim().to_string();
        match modal.kind {
            InputKind::Search => {
                let search = SearchQuery::parse(&value);
                if let Some(error) = &search.error {
                    self.toast_error(error.clone());
                    return;
                }
                self.search = value.clone();
                self.request_bookmarks(false, false);
                if self.search.is_empty() {
                    self.toast_info("Search cleared");
                } else {
                    self.toast_info(format!("Search: “{}”", self.search));
                }
            }
            InputKind::ReaderSearch => {
                let Some(reader) = &mut self.reader else {
                    return;
                };
                let count = reader.search(&value);
                if value.is_empty() {
                    self.toast_info("Article search cleared");
                } else if count == 0 {
                    self.toast_info(format!("No matches for “{value}”"));
                } else {
                    self.toast_info(format!("{count} matches for “{value}”"));
                }
            }
            InputKind::NewBookmark => {
                if value.is_empty() {
                    self.toast_error("A URL is required.");
                    return;
                }
                self.create_bookmark(&value);
            }
            InputKind::EditLabels { ids } => {
                let labels = parse_labels(&value);
                let count = ids.len();
                let patch = BookmarkPatch {
                    labels: Some(labels.clone()),
                    ..Default::default()
                };
                let message = if labels.is_empty() {
                    if count > 1 {
                        format!("Labels cleared for {count} bookmarks")
                    } else {
                        "Labels cleared".to_string()
                    }
                } else if count > 1 {
                    format!("Labels updated for {count} bookmarks")
                } else {
                    format!("Labels: {}", labels.join(", "))
                };
                self.update_ids(ids, patch, message, false);
            }
            InputKind::EditNote { id } => {
                let patch = BookmarkPatch {
                    note: Some(value),
                    ..Default::default()
                };
                self.update_bookmark(id, patch, "Note updated", false);
            }
            InputKind::EditTitle { id } => {
                if value.is_empty() {
                    self.toast_error("The title cannot be empty.");
                    return;
                }
                let patch = BookmarkPatch {
                    title: Some(value),
                    ..Default::default()
                };
                self.update_bookmark(id, patch, "Title updated", false);
            }
            InputKind::AnnotationNote {
                bookmark_id,
                id,
                color,
            } => {
                self.update_annotation(bookmark_id, id, color, value);
            }
            InputKind::RenameLabel { from } => {
                if value.is_empty() {
                    self.toast_error("The label name cannot be empty.");
                    return;
                }
                if value == from {
                    return;
                }
                self.rename_label(from, value);
            }
        }
    }

    fn create_bookmark(&mut self, input: &str) {
        if !self.require_online() {
            return;
        }
        let url = match normalize_url(input) {
            Ok(url) => url,
            Err(message) => {
                self.toast_error(message);
                return;
            }
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Event::api(ApiEvent::Created(
                client.create_bookmark(&url).await,
            )));
        });
        self.toast_info("Saving bookmark…");
    }

    fn cycle_sort(&mut self, forward: bool) {
        self.sort = if forward {
            self.sort.next()
        } else {
            self.sort.previous()
        };
        self.request_bookmarks(false, false);
        self.toast_info(format!("Sort: {}", self.sort.label()));
    }

    fn flush_pending_changes(&mut self) {
        if self.offline_mode || self.offline || self.pending_sync_running {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(cache) = &self.cache else {
            return;
        };
        let updates = cache.pending_updates();
        let deletes = cache.pending_deletes();
        if updates.is_empty() && deletes.is_empty() {
            return;
        }
        self.pending_sync_running = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut result = PendingSyncResult::default();
            for (id, patch) in updates {
                match client.update_bookmark(&id, &patch).await {
                    Ok(()) => result.updated.push(id),
                    Err(err) => {
                        if err.is_network() {
                            result.network_error = true;
                            break;
                        }
                        result.errors.push(format!("{id}: {err}"));
                    }
                }
            }
            if !result.network_error {
                for id in deletes {
                    match client.delete_bookmark(&id).await {
                        Ok(()) => result.deleted.push(id),
                        Err(err) => {
                            if err.is_network() {
                                result.network_error = true;
                                break;
                            }
                            result.errors.push(format!("{id}: {err}"));
                        }
                    }
                }
            }
            let _ = tx.send(Event::api(ApiEvent::PendingSynced(result)));
        });
    }

    fn handle_pending_synced(&mut self, result: PendingSyncResult) {
        self.pending_sync_running = false;
        if let Some(cache) = &mut self.cache {
            for id in &result.updated {
                cache.remove_pending_update(id);
            }
            for id in &result.deleted {
                cache.remove_pending_delete(id);
            }
            let _ = cache.save();
        }
        if result.network_error {
            self.offline = true;
        }
        let synced = result.updated.len() + result.deleted.len();
        if synced > 0 {
            self.toast_success(format!("Synced {synced} offline change(s)"));
            self.request_bookmarks(false, true);
        }
        if let Some(error) = result.errors.first() {
            let extra = result.errors.len().saturating_sub(1);
            self.toast_error(if extra > 0 {
                format!("{error} (+{extra} more)")
            } else {
                error.clone()
            });
        }
    }

    fn refresh_all(&mut self) {
        if self.offline_mode {
            self.reload_cache();
            self.toast_info("Offline mode — reloaded local cache");
            return;
        }
        self.request_bookmarks(false, true);
        self.request_labels();
        self.request_collections();
        self.request_annotation_summaries();
        if !self.profile_denied {
            self.request_profile();
        }
        self.toast_info("Refreshing…");
    }

    // ------------------------------------------------------------------
    // Sidebar
    // ------------------------------------------------------------------

    fn rebuild_sidebar(&mut self) {
        let current = self
            .sidebar_items
            .get(self.sidebar_state.selected().unwrap_or(0))
            .and_then(|item| item.filter.clone());
        let query = self.sidebar_search.value.trim().to_lowercase();
        let matches = |text: &str| {
            query.is_empty() || text.to_lowercase().contains(&query)
        };

        let mut items = Vec::new();
        for (filter, text) in [
            (Filter::Unread, "●  Unread"),
            (Filter::Reading, "◐  Reading"),
            (Filter::Marked, "★  Favorites"),
            (Filter::Archived, "■  Archived"),
            (Filter::All, "≡  All Bookmarks"),
            (Filter::Highlights, "✎  Highlights"),
        ] {
            if matches(text) {
                items.push(SidebarItem {
                    filter: Some(filter),
                    text: text.into(),
                    count: None,
                    header: false,
                });
            }
        }

        let mut collections: Vec<&Collection> = self
            .collections
            .iter()
            .filter(|collection| {
                !collection.name.trim().is_empty()
                    && !collection.is_deleted
                    && matches(&collection.name)
            })
            .collect();
        collections.sort_by_key(|collection| {
            (!collection.is_pinned, collection.name.to_lowercase())
        });
        if !collections.is_empty() {
            items.push(SidebarItem {
                filter: None,
                text: "COLLECTIONS".into(),
                count: None,
                header: true,
            });
            for collection in collections {
                items.push(SidebarItem {
                    filter: Some(Filter::Collection {
                        id: collection.id.clone(),
                        name: collection.name.clone(),
                    }),
                    text: format!("▤  {}", collection.name),
                    count: None,
                    header: false,
                });
            }
        }

        let labels: Vec<&Label> = self
            .labels
            .iter()
            .filter(|label| {
                !label.name.trim().is_empty() && matches(&label.name)
            })
            .collect();
        if !labels.is_empty() {
            items.push(SidebarItem {
                filter: None,
                text: "LABELS".into(),
                count: None,
                header: true,
            });
            for label in labels {
                items.push(SidebarItem {
                    filter: Some(Filter::Label(label.name.clone())),
                    text: format!("# {}", label.name),
                    count: Some(label.count),
                    header: false,
                });
            }
        }

        let target = current
            .and_then(|filter| {
                items
                    .iter()
                    .position(|item| item.filter.as_ref() == Some(&filter))
            })
            .or_else(|| {
                items
                    .iter()
                    .position(|item| item.filter.as_ref() == Some(&self.filter))
            })
            .or_else(|| items.iter().position(|item| !item.header));

        self.sidebar_items = items;
        self.sidebar_state.select(target);
        // The previous offset can point past the new, shorter list and would
        // leave entries scrolled out of view. Start from the top and let the
        // list scroll to the selection when it does not fit.
        *self.sidebar_state.offset_mut() = 0;
    }

    fn clear_sidebar_search(&mut self) {
        self.sidebar_search = TextInput::new("");
        self.sidebar_search_active = false;
        self.rebuild_sidebar();
    }

    fn sidebar_move(&mut self, delta: isize) {
        if self.sidebar_items.is_empty() {
            return;
        }
        let len = self.sidebar_items.len() as isize;
        let mut index = self.sidebar_state.selected().unwrap_or(0) as isize;
        for _ in 0..len {
            index = (index + delta).rem_euclid(len);
            if !self.sidebar_items[index as usize].header {
                self.sidebar_state.select(Some(index as usize));
                return;
            }
        }
    }

    fn sidebar_select_edge(&mut self, start: bool) {
        if start {
            if let Some(index) =
                self.sidebar_items.iter().position(|item| !item.header)
            {
                self.sidebar_state.select(Some(index));
            }
        } else if let Some(index) =
            self.sidebar_items.iter().rposition(|item| !item.header)
        {
            self.sidebar_state.select(Some(index));
        }
    }

    fn apply_sidebar_selection(&mut self) {
        let Some(index) = self.sidebar_state.selected() else {
            return;
        };
        let Some(filter) = self
            .sidebar_items
            .get(index)
            .and_then(|item| item.filter.clone())
        else {
            return;
        };
        let changed = filter != self.filter;
        self.filter = filter;
        self.focus = Focus::List;
        self.sidebar_search = TextInput::new("");
        self.sidebar_search_active = false;
        self.rebuild_sidebar();
        if changed {
            self.request_bookmarks(false, false);
        }
    }

    // ------------------------------------------------------------------
    // API results
    // ------------------------------------------------------------------

    fn handle_api(&mut self, event: ApiEvent) {
        if self.screen == Screen::Setup
            && !matches!(
                event,
                ApiEvent::Setup(_)
                    | ApiEvent::OAuthDevice { .. }
                    | ApiEvent::OAuthToken { .. }
            )
        {
            return;
        }

        match event {
            ApiEvent::Bookmarks {
                seq,
                append,
                result,
            } => self.handle_bookmarks(seq, append, result),
            ApiEvent::Labels(result) => match result {
                Ok(labels) => {
                    self.offline = false;
                    self.labels = labels
                        .into_iter()
                        .filter(|label| !label.name.trim().is_empty())
                        .collect();
                    if let Some(cache) = &mut self.cache {
                        cache.set_labels(self.labels.clone());
                        let _ = cache.save();
                    }
                    self.rebuild_sidebar();
                }
                Err(err) => {
                    if err.is_network() {
                        self.offline = true;
                    }
                    if !self.handle_unauthorized(&err) {
                        self.toast_error(err.to_string());
                    }
                }
            },
            ApiEvent::Collections(result) => match result {
                Ok(collections) => {
                    self.offline = false;
                    self.collections = collections;
                    if let Some(cache) = &mut self.cache {
                        cache.set_collections(self.collections.clone());
                        let _ = cache.save();
                    }
                    self.rebuild_sidebar();
                }
                Err(err) => {
                    if err.is_network() {
                        self.offline = true;
                    }
                    if !self.handle_unauthorized(&err) {
                        self.toast_error(err.to_string());
                    }
                }
            },
            ApiEvent::Profile(result) => {
                match result {
                    Ok(profile) => {
                        self.offline = false;
                        self.profile = Some(profile);
                        self.profile_denied = false;
                        if let Some(profile) = &self.profile {
                            if let Some(cache) = &mut self.cache {
                                cache.set_profile(profile.clone());
                                let _ = cache.save();
                            }
                        }
                    }
                    Err(err) => {
                        if err.is_network() {
                            self.offline = true;
                        }
                        if err.status() == Some(403) {
                            self.profile_denied = true;
                            self.toast_info("API token has no profile access; username hidden.");
                        } else if !self.handle_unauthorized(&err) {
                            self.toast_error(err.to_string());
                        }
                    }
                }
            }
            ApiEvent::Article { id, result } => {
                if !self
                    .reader
                    .as_ref()
                    .is_some_and(|reader| reader.bookmark.id == id)
                {
                    return;
                }
                match result {
                    Ok(article) => {
                        let urls = article
                            .images
                            .values()
                            .cloned()
                            .collect::<HashSet<_>>()
                            .into_iter()
                            .take(12)
                            .collect::<Vec<_>>();
                        if let Some(reader) = &mut self.reader {
                            reader.set_article(article);
                        }
                        if self.images_enabled {
                            self.request_images_for_reader(id, urls);
                        }
                        self.apply_pending_highlight();
                    }
                    Err(err) => {
                        if self.handle_unauthorized(&err) {
                            return;
                        }
                        if err.is_network() {
                            self.offline = true;
                            let cached = self
                                .cache
                                .as_ref()
                                .and_then(|cache| cache.read_article(&id));
                            let bookmark = self
                                .reader
                                .as_ref()
                                .map(|reader| reader.bookmark.clone());
                            if let Some(html) = cached {
                                if let Some(reader) = &mut self.reader {
                                    reader.set_article(parse_article(&html));
                                }
                                self.toast_info(
                                    "Offline — loaded cached article",
                                );
                            } else if let Some(bookmark) =
                                bookmark.filter(|bookmark| {
                                    bookmark.description.as_deref().is_some_and(
                                        |text| !text.trim().is_empty(),
                                    ) || bookmark.note.as_deref().is_some_and(
                                        |text| !text.trim().is_empty(),
                                    )
                                })
                            {
                                if let Some(reader) = &mut self.reader {
                                    reader.set_article(fallback_article(
                                        &bookmark,
                                    ));
                                }
                                self.toast_info(
                                    "Offline — showing saved description/note",
                                );
                            } else if let Some(reader) = &mut self.reader {
                                reader.loading = false;
                                reader.error = Some(
                                    "Offline and no cached copy of this article is available."
                                        .into(),
                                );
                            }
                        } else if let Some(reader) = &mut self.reader {
                            reader.loading = false;
                            reader.error = Some(article_error(&err));
                        }
                    }
                }
            }
            ApiEvent::Annotations {
                bookmark_id,
                result,
            } => {
                if !self
                    .reader
                    .as_ref()
                    .is_some_and(|reader| reader.bookmark.id == bookmark_id)
                {
                    return;
                }
                match result {
                    Ok(annotations) => {
                        if let Some(reader) = &mut self.reader {
                            reader.annotations = annotations;
                        }
                        self.store_reader_annotations();
                        self.store_annotation_summaries();
                        self.apply_pending_highlight();
                    }
                    Err(err) => {
                        if !self.handle_unauthorized(&err) {
                            self.toast_error(format!(
                                "Could not load annotations: {err}"
                            ));
                        }
                    }
                }
            }
            ApiEvent::AnnotationCreated {
                bookmark_id,
                result,
            } => match result {
                Ok(annotation) => {
                    if let Some(reader) = &mut self.reader {
                        if reader.bookmark.id == bookmark_id {
                            reader.annotations.push(annotation);
                        }
                    }
                    self.store_reader_annotations();
                    self.store_annotation_summaries();
                    self.toast_success("Annotation added");
                }
                Err(err) => {
                    if err.is_network() {
                        self.offline = true;
                        self.toast_error(
                            "Annotations require a connection to Readeck",
                        );
                    } else if !self.handle_unauthorized(&err) {
                        self.toast_error(format!(
                            "Could not add the annotation: {err}"
                        ));
                    }
                }
            },
            ApiEvent::AnnotationUpdated {
                bookmark_id,
                id,
                color,
                note,
                result,
            } => match result {
                Ok(()) => {
                    if let Some(reader) = &mut self.reader {
                        if reader.bookmark.id == bookmark_id {
                            if let Some(annotation) = reader
                                .annotations
                                .iter_mut()
                                .find(|annotation| annotation.id == id)
                            {
                                annotation.color = color;
                                annotation.note = note;
                            }
                        }
                    }
                    self.store_reader_annotations();
                    self.store_annotation_summaries();
                    self.toast_success("Annotation updated");
                }
                Err(err) => {
                    if !self.handle_unauthorized(&err) {
                        self.toast_error(format!(
                            "Could not update the annotation: {err}"
                        ));
                    }
                }
            },
            ApiEvent::AnnotationSummaries(result) => match result {
                Ok(annotations) => {
                    let mut summaries: HashMap<String, Vec<Annotation>> =
                        HashMap::new();
                    for annotation in annotations {
                        if !annotation.bookmark_id.is_empty() {
                            summaries
                                .entry(annotation.bookmark_id.clone())
                                .or_default()
                                .push(annotation);
                        }
                    }
                    self.annotation_summaries = Some(summaries);
                    if self.filter == Filter::Highlights {
                        self.rebuild_highlights();
                        self.total = self.highlights.len();
                        if self.list_state.selected().is_none() {
                            self.list_state.select(
                                (!self.highlights.is_empty()).then_some(0),
                            );
                        }
                    }
                }
                Err(err) => {
                    let _ = self.handle_unauthorized(&err);
                }
            },
            ApiEvent::AnnotationDeleted {
                bookmark_id,
                id,
                result,
            } => match result {
                Ok(()) => {
                    if let Some(reader) = &mut self.reader {
                        if reader.bookmark.id == bookmark_id {
                            reader
                                .annotations
                                .retain(|annotation| annotation.id != id);
                        }
                    }
                    self.remove_annotation_summary(&bookmark_id, &id);
                    self.remove_cached_annotation(&bookmark_id, &id);
                    self.store_reader_annotations();
                    self.store_annotation_summaries();
                    if self.filter == Filter::Highlights {
                        self.rebuild_highlights();
                        self.total = self.highlights.len();
                        let index = self.list_state.selected().unwrap_or(0);
                        self.list_state.select(
                            (!self.highlights.is_empty()).then_some(
                                index.min(self.highlights.len() - 1),
                            ),
                        );
                    }
                    self.toast_success("Annotation deleted");
                }
                Err(err) => {
                    if !self.handle_unauthorized(&err) {
                        self.toast_error(format!(
                            "Could not delete the annotation: {err}"
                        ));
                    }
                }
            },
            ApiEvent::Image {
                bookmark_id,
                url,
                result,
            } => {
                if !self
                    .reader
                    .as_ref()
                    .is_some_and(|reader| reader.bookmark.id == bookmark_id)
                {
                    return;
                }
                if let Some(reader) = &mut self.reader {
                    match result {
                        Ok(image) => reader.set_image(url, image),
                        Err(err) => {
                            reader.set_image_error(url, err.to_string())
                        }
                    }
                }
            }
            ApiEvent::ImageEncoded {
                bookmark_id,
                url,
                width,
                result,
            } => {
                let Some(reader) = &mut self.reader else {
                    return;
                };
                if reader.bookmark.id != bookmark_id {
                    return;
                }
                match result {
                    Ok(graphic) => reader.set_graphic(url, graphic),
                    Err(err) => reader.set_graphic_error(url, width, err),
                }
            }
            ApiEvent::Updated {
                id,
                patch,
                message,
                quiet,
                result,
            } => match result {
                Ok(()) => {
                    self.apply_patch(&id, &patch);
                    if !quiet {
                        self.toast_success(message);
                    }
                }
                Err(err) => {
                    if err.is_network() {
                        self.offline = true;
                        self.apply_patch(&id, &patch);
                        if let Some(cache) = &mut self.cache {
                            cache.queue_patch(&id, &patch);
                            let _ = cache.save();
                        }
                        if !quiet {
                            self.toast_info("Offline — change saved locally");
                        }
                    } else if !self.handle_unauthorized(&err) {
                        self.toast_error(err.to_string());
                        self.request_bookmarks(false, true);
                    }
                }
            },
            ApiEvent::LabelRenamed { from, to, result } => match result {
                Ok(()) => {
                    self.apply_label_rename(&from, &to);
                    self.toast_success(format!("Renamed #{from} to #{to}"));
                }
                Err(err) => {
                    if !self.handle_unauthorized(&err) {
                        self.toast_error(format!(
                            "Could not rename the label: {err}"
                        ));
                    }
                }
            },
            ApiEvent::BulkUpdated {
                ids,
                patch,
                message,
                quiet,
                result,
            } => match result {
                Ok(_) => {
                    self.apply_patch_to_ids(&ids, &patch);
                    if !quiet {
                        self.toast_success(message);
                    }
                }
                Err(err) => {
                    if err.is_network() {
                        self.offline = true;
                        self.apply_patch_to_ids(&ids, &patch);
                        if let Some(cache) = &mut self.cache {
                            for id in &ids {
                                cache.queue_patch(id, &patch);
                            }
                            let _ = cache.save();
                        }
                        if !quiet {
                            self.toast_info(format!(
                                "Offline — {} change(s) saved locally",
                                ids.len()
                            ));
                        }
                    } else if !self.handle_unauthorized(&err) {
                        self.toast_error(err.to_string());
                        self.request_bookmarks(false, true);
                    }
                }
            },
            ApiEvent::Created(result) => match result {
                Ok(_) => {
                    self.toast_success("Bookmark saved");
                    self.request_bookmarks(false, true);
                }
                Err(err) => {
                    if !self.handle_unauthorized(&err) {
                        self.toast_error(err.to_string());
                    }
                }
            },
            ApiEvent::Deleted { id, result } => match result {
                Ok(()) => {
                    self.remove_bookmark(&id);
                    self.toast_success("Bookmark deleted");
                }
                Err(err) => {
                    if err.is_network() {
                        self.offline = true;
                        self.delete_offline(vec![id]);
                    } else if !self.handle_unauthorized(&err) {
                        self.toast_error(err.to_string());
                    }
                }
            },
            ApiEvent::BulkDeleted { ids, result } => match result {
                Ok(_) => {
                    let count = ids.len();
                    self.remove_bookmarks(&ids);
                    if count == 1 {
                        self.toast_success("Bookmark deleted");
                    } else {
                        self.toast_success(format!(
                            "Deleted {count} bookmarks"
                        ));
                    }
                }
                Err(err) => {
                    if err.is_network() {
                        self.offline = true;
                        self.delete_offline(ids);
                    } else if !self.handle_unauthorized(&err) {
                        self.toast_error(err.to_string());
                        self.request_bookmarks(false, true);
                    }
                }
            },
            ApiEvent::OAuthDevice { seq, result } => {
                if seq != self.oauth_seq {
                    return;
                }
                if let Some(oauth) = &mut self.oauth {
                    match result {
                        Ok(device) => oauth.device = Some(device),
                        Err(err) => {
                            oauth.error = Some(err.to_string());
                            if let Some(setup) = &mut self.setup {
                                setup.busy = false;
                            }
                        }
                    }
                }
            }
            ApiEvent::PendingSynced(result) => {
                self.handle_pending_synced(result);
            }
            ApiEvent::OAuthToken { seq, result } => {
                if seq != self.oauth_seq {
                    return;
                }
                match result {
                    Ok(token) => {
                        let server = self
                            .oauth
                            .as_ref()
                            .map(|oauth| oauth.server.clone())
                            .unwrap_or_default();
                        self.finish_login(server, token, None, false);
                    }
                    Err(err) => {
                        if let Some(oauth) = &mut self.oauth {
                            oauth.error = Some(err.to_string());
                        }
                        if let Some(setup) = &mut self.setup {
                            setup.busy = false;
                        }
                    }
                }
            }
            ApiEvent::Setup(result) => match result {
                Ok(profile) => self.complete_setup(Some(profile), false),
                Err(err) if err.status() == Some(403) => {
                    // A valid token may be restricted to bookmarks only. The
                    // profile is used solely for the username in the title bar.
                    self.complete_setup(None, true);
                }
                Err(err) => {
                    self.client = None;
                    if let Some(setup) = &mut self.setup {
                        setup.busy = false;
                        setup.error = Some(err.to_string());
                    }
                }
            },
        }
    }

    fn handle_bookmarks(
        &mut self,
        seq: u64,
        append: bool,
        result: Result<crate::models::Page<Bookmark>, ApiError>,
    ) {
        if seq != self.list_seq {
            return;
        }
        self.loading_list = false;
        self.loading_more = false;

        match result {
            Ok(page) => {
                let was_offline = self.offline;
                self.offline = false;
                self.list_error = None;
                if let Some(cache) = &mut self.cache {
                    cache.upsert_bookmarks(&page.items);
                    let _ = cache.save();
                }
                if append {
                    for bookmark in page.items {
                        if !self
                            .bookmarks
                            .iter()
                            .any(|existing| existing.id == bookmark.id)
                        {
                            self.bookmarks.push(bookmark);
                        }
                    }
                    self.total =
                        self.total.max(page.total).max(self.bookmarks.len());
                } else {
                    self.bookmarks = page.items;
                    self.total = page.total.max(self.bookmarks.len());
                    let index = self
                        .pending_select
                        .take()
                        .and_then(|id| {
                            self.bookmarks.iter().position(|b| b.id == id)
                        })
                        .or_else(|| (!self.bookmarks.is_empty()).then_some(0));
                    self.list_state.select(index);
                }
                self.selected_ids.retain(|id| {
                    self.bookmarks.iter().any(|bookmark| &bookmark.id == id)
                });
                if was_offline {
                    self.toast_info("Back online");
                }
                self.flush_pending_changes();
            }
            Err(err) => {
                if self.handle_unauthorized(&err) {
                    return;
                }
                if err.is_network() {
                    self.offline = true;
                    if append {
                        self.toast_error(
                            "Offline — could not load more bookmarks.",
                        );
                    } else {
                        self.show_cached_bookmarks(true);
                    }
                } else if append {
                    self.toast_error(err.to_string());
                } else {
                    let message = err.to_string();
                    self.list_error = Some(message.clone());
                    self.toast_error(message);
                }
            }
        }
    }

    fn show_cached_bookmarks(&mut self, notify: bool) {
        let pending = self.pending_select.take();
        let Some(cache) = &self.cache else {
            self.bookmarks.clear();
            self.total = 0;
            self.list_state.select(None);
            self.list_error = Some(if self.offline_mode {
                "Offline mode — no local cache available.".into()
            } else {
                "Offline — no cached bookmarks available.".into()
            });
            return;
        };

        if self.filter == Filter::Highlights {
            self.rebuild_highlights();
            self.bookmarks.clear();
            self.total = self.highlights.len();
            self.list_state
                .select((!self.highlights.is_empty()).then_some(0));
            self.list_error = None;
            self.selected_ids.clear();
            return;
        }
        let search = SearchQuery::parse(&self.search);
        let bookmarks = cache.local_bookmarks(&self.filter, &search, self.sort);
        self.total = bookmarks.len();
        self.bookmarks = bookmarks;
        let index = pending
            .and_then(|id| self.bookmarks.iter().position(|b| b.id == id))
            .or_else(|| (!self.bookmarks.is_empty()).then_some(0));
        self.list_state.select(index);
        self.selected_ids.retain(|id| {
            self.bookmarks.iter().any(|bookmark| &bookmark.id == id)
        });
        self.list_error = Some(if self.bookmarks.is_empty() {
            if self.offline_mode {
                "Offline mode — no cached bookmarks for this view.".into()
            } else {
                "Offline — no cached bookmarks for this view.".into()
            }
        } else if self.offline_mode {
            "Offline mode — showing cached bookmarks.".into()
        } else {
            "Offline — showing cached bookmarks.".into()
        });
        if notify {
            self.toast_info("Offline — using local cache");
        }
    }

    fn apply_patch(&mut self, id: &str, patch: &BookmarkPatch) {
        if let Some(bookmark) = self.bookmarks.iter_mut().find(|b| b.id == id) {
            bookmark.apply_patch(patch);
        }
        if let Some(reader) = &mut self.reader {
            if reader.bookmark.id == id {
                reader.bookmark.apply_patch(patch);
                if let Some(progress) = patch.read_progress {
                    reader.initial_progress = progress;
                    reader.manual_read_state = true;
                }
            }
        }
        if let Some(cache) = &mut self.cache {
            cache.apply_patch(id, patch);
            let _ = cache.save();
        }

        let filter = self.filter.clone();
        let before = self.bookmarks.len();
        self.bookmarks
            .retain(|bookmark| bookmark.id != id || filter.matches(bookmark));
        if self.bookmarks.len() < before {
            self.total =
                self.total.saturating_sub(before - self.bookmarks.len());
            self.clamp_selection();
        }
        self.selected_ids.retain(|selected| {
            self.bookmarks
                .iter()
                .any(|bookmark| &bookmark.id == selected)
        });
    }

    fn apply_patch_to_ids(&mut self, ids: &[String], patch: &BookmarkPatch) {
        let id_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        for bookmark in &mut self.bookmarks {
            if id_set.contains(bookmark.id.as_str()) {
                bookmark.apply_patch(patch);
            }
        }
        if let Some(reader) = &mut self.reader {
            if id_set.contains(reader.bookmark.id.as_str()) {
                reader.bookmark.apply_patch(patch);
                if let Some(progress) = patch.read_progress {
                    reader.initial_progress = progress;
                    reader.manual_read_state = true;
                }
            }
        }
        if let Some(cache) = &mut self.cache {
            for id in ids {
                cache.apply_patch(id, patch);
            }
            let _ = cache.save();
        }

        let filter = self.filter.clone();
        let before = self.bookmarks.len();
        self.bookmarks.retain(|bookmark| {
            !id_set.contains(bookmark.id.as_str()) || filter.matches(bookmark)
        });
        if self.bookmarks.len() < before {
            self.total =
                self.total.saturating_sub(before - self.bookmarks.len());
            self.clamp_selection();
        }
        let visible: HashSet<&str> = self
            .bookmarks
            .iter()
            .map(|bookmark| bookmark.id.as_str())
            .collect();
        self.selected_ids
            .retain(|selected| visible.contains(selected.as_str()));
    }

    fn remove_bookmark(&mut self, id: &str) {
        self.remove_bookmarks(&[id.to_string()]);
    }

    fn remove_bookmarks(&mut self, ids: &[String]) {
        let id_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let before = self.bookmarks.len();
        self.bookmarks
            .retain(|bookmark| !id_set.contains(bookmark.id.as_str()));
        if self.bookmarks.len() < before {
            self.total =
                self.total.saturating_sub(before - self.bookmarks.len());
            self.clamp_selection();
        }
        self.selected_ids
            .retain(|selected| !id_set.contains(selected.as_str()));
        if let Some(cache) = &mut self.cache {
            for id in ids {
                cache.remove_bookmark(id);
            }
            let _ = cache.save();
        }
        if self
            .reader
            .as_ref()
            .is_some_and(|reader| id_set.contains(reader.bookmark.id.as_str()))
        {
            self.reader = None;
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.bookmarks.len();
        let index = match self.list_state.selected() {
            Some(index) if len > 0 => Some(index.min(len - 1)),
            Some(_) => None,
            None if len > 0 => Some(0),
            None => None,
        };
        self.list_state.select(index);
    }

    fn handle_unauthorized(&mut self, err: &ApiError) -> bool {
        if err.status() == Some(401) {
            self.show_setup(Some(err.to_string()));
            true
        } else {
            false
        }
    }

    pub fn pending_changes(&self) -> usize {
        self.cache.as_ref().map(Cache::pending_count).unwrap_or(0)
    }

    fn open_theme_picker(&mut self) {
        let current = crate::ui::theme::current_info();
        let themes = crate::ui::theme::all();
        let selected = themes
            .iter()
            .position(|(id, _)| *id == current.id)
            .unwrap_or(0);
        self.modal = Some(Modal::ThemePicker(ThemePicker {
            selected,
            original: current.id,
        }));
    }

    // ------------------------------------------------------------------
    // Toasts
    // ------------------------------------------------------------------

    pub fn toast_info(&mut self, message: impl Into<String>) {
        self.set_toast(ToastKind::Info, message.into(), 50);
    }

    pub fn toast_success(&mut self, message: impl Into<String>) {
        self.set_toast(ToastKind::Success, message.into(), 40);
    }

    pub fn toast_error(&mut self, message: impl Into<String>) {
        self.set_toast(ToastKind::Error, message.into(), 120);
    }

    fn set_toast(&mut self, kind: ToastKind, text: String, ttl: u64) {
        self.toast = Some(Toast {
            kind,
            text,
            expires: self.tick.wrapping_add(ttl),
        });
    }
}

/// Cursor position of the previous or next comma-separated label.
///
/// Forward moves to the start of the next label, or to the end of the line
/// when there is none. Backward moves to the start of the current label, or
/// to the previous one when the cursor is already there.
const ANNOTATION_COLORS: [&str; 5] =
    ["yellow", "red", "green", "blue", "purple"];

fn next_annotation_color(color: &str) -> &'static str {
    let index = ANNOTATION_COLORS
        .iter()
        .position(|value| value.eq_ignore_ascii_case(color))
        .unwrap_or(0);
    ANNOTATION_COLORS[(index + 1) % ANNOTATION_COLORS.len()]
}

/// Keep note separators outside the highlight.
fn trim_trailing_space(
    article: &crate::article::Article,
    block: usize,
    start: usize,
    mut end: usize,
) -> usize {
    let Some(text) = article.blocks.get(block).map(|block| &block.text) else {
        return end;
    };
    let chars: Vec<char> = text.chars().collect();
    while end > start && end <= chars.len() && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    end
}

fn label_jump(value: &str, cursor: usize, forward: bool) -> usize {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut starts = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == ',' || chars[index].is_whitespace() {
            index += 1;
            continue;
        }
        starts.push(index);
        while index < chars.len() && chars[index] != ',' {
            index += 1;
        }
    }

    if forward {
        starts
            .into_iter()
            .find(|start| *start > cursor)
            .unwrap_or(chars.len())
    } else {
        starts
            .into_iter()
            .rfind(|start| *start < cursor)
            .unwrap_or(0)
    }
}

/// Locate the comma-separated label token around the cursor.
///
/// Returns character indices for the trimmed token and the text before the
/// cursor, which is used as the completion prefix.
fn label_token(value: &str, cursor: usize) -> (usize, usize, String) {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());

    let mut start = 0;
    for index in (0..cursor).rev() {
        if chars[index] == ',' {
            start = index + 1;
            break;
        }
    }

    let mut end = cursor;
    while end < chars.len() && chars[end] != ',' {
        end += 1;
    }

    let mut content_start = start;
    while content_start < cursor && chars[content_start].is_whitespace() {
        content_start += 1;
    }
    let mut content_end = end;
    while content_end > content_start && chars[content_end - 1].is_whitespace()
    {
        content_end -= 1;
    }

    let prefix_end = cursor.min(content_end).max(content_start);
    let prefix = chars[content_start..prefix_end]
        .iter()
        .collect::<String>()
        .to_lowercase();
    (content_start, content_end, prefix)
}

fn label_suggestions(
    labels: &[Label],
    value: &str,
    cursor: usize,
) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    let (content_start, content_end, _) = label_token(value, cursor);
    let prefix_end = cursor.min(content_end).max(content_start);
    let prefix: String = chars[content_start..prefix_end]
        .iter()
        .collect::<String>()
        .to_lowercase();

    let mut used = HashSet::new();
    let mut token_start = 0;
    for index in 0..=chars.len() {
        if index == chars.len() || chars[index] == ',' {
            if !(token_start <= cursor && cursor <= index) {
                let token: String = chars[token_start..index].iter().collect();
                let token = token.trim();
                if !token.is_empty() {
                    used.insert(token.to_lowercase());
                }
            }
            token_start = index + 1;
        }
    }

    let mut suggestions: Vec<String> = labels
        .iter()
        .map(|label| label.name.trim())
        .filter(|name| !name.is_empty())
        .filter(|name| !used.contains(&name.to_lowercase()))
        .filter(|name| name.to_lowercase().starts_with(&prefix))
        .map(str::to_string)
        .collect();
    suggestions.sort_by_key(|name| name.to_lowercase());
    suggestions.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    suggestions
}

fn parse_labels(value: &str) -> Vec<String> {
    let mut labels = Vec::new();
    for part in value.split(',') {
        let label = part.trim();
        if !label.is_empty()
            && !labels.iter().any(|existing: &String| existing == label)
        {
            labels.push(label.to_string());
        }
    }
    labels
}

pub fn normalize_url(input: &str) -> Result<String, String> {
    let mut value = input.trim().to_string();
    if value.is_empty() {
        return Err("A URL is required.".into());
    }
    if !value.contains("://") {
        value = format!("https://{value}");
    }
    let parsed =
        url::Url::parse(&value).map_err(|_| format!("Invalid URL: {input}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Only http and https URLs can be saved.".into());
    }
    if parsed.host_str().is_none() {
        return Err("The URL has no host.".into());
    }
    Ok(value)
}

/// Position of an annotation selector in the article: `p[2]` or
/// `section[1]/ul[1]/li[3]`. `None` for an empty or malformed selector.
fn selector_position(selector: &str) -> Option<Vec<(String, usize)>> {
    let selector = selector.trim();
    if selector.is_empty() {
        return None;
    }
    let mut position = Vec::new();
    for part in selector.split('/') {
        let open = part.rfind('[')?;
        let close = part.rfind(']')?;
        if close != part.len() - 1 || open == 0 || open >= close {
            return None;
        }
        let index = part[open + 1..close].parse().ok()?;
        position.push((part[..open].to_string(), index));
    }
    Some(position)
}

fn fallback_article(bookmark: &Bookmark) -> Article {
    let mut blocks = Vec::new();
    if let Some(description) = bookmark
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        blocks.push(("Description", description.to_string()));
    }
    if let Some(note) = bookmark
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        blocks.push(("Your note", note.to_string()));
    }
    if blocks.is_empty() {
        blocks.push((
            "",
            "Readeck has no article text for this bookmark yet.\n\n\
             The extraction may still be running, or this entry may only be a link. \
             Press o to open the original page in your browser, or r to try fetching the article again."
                .to_string(),
        ));
    }
    Article::from_text_blocks(blocks)
}

async fn run_oauth_login(
    client: ReadeckClient,
    seq: u64,
    cancel: Arc<AtomicBool>,
    tx: UnboundedSender<Event>,
) {
    let client_id = match client.oauth_register().await {
        Ok(client_id) => client_id,
        Err(err) => {
            let _ = tx.send(Event::api(ApiEvent::OAuthDevice {
                seq,
                result: Err(err),
            }));
            return;
        }
    };

    let device = match client.oauth_device(&client_id).await {
        Ok(device) => device,
        Err(err) => {
            let _ = tx.send(Event::api(ApiEvent::OAuthDevice {
                seq,
                result: Err(err),
            }));
            return;
        }
    };

    let poll_interval = device.interval.max(1);
    let expires_in = device.expires_in.max(60);
    let started = Instant::now();
    if tx
        .send(Event::api(ApiEvent::OAuthDevice {
            seq,
            result: Ok(device.clone()),
        }))
        .is_err()
    {
        return;
    }

    let mut wait = poll_interval;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if started.elapsed().as_secs() >= expires_in {
            let _ = tx.send(Event::api(ApiEvent::OAuthToken {
                seq,
                result: Err(ApiError::OAuth {
                    code: "expired_token".into(),
                    message: "The device authorization code expired.".into(),
                }),
            }));
            return;
        }

        tokio::time::sleep(Duration::from_secs(wait)).await;
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        let result =
            match client.oauth_token(&client_id, &device.device_code).await {
                Ok(OAuthPoll::Pending) => continue,
                Ok(OAuthPoll::SlowDown) => {
                    wait = (wait + 5).min(60);
                    continue;
                }
                Ok(OAuthPoll::Token(token)) => Ok(token),
                Ok(OAuthPoll::Denied(message)) => Err(ApiError::OAuth {
                    code: "access_denied".into(),
                    message,
                }),
                Ok(OAuthPoll::Expired(message)) => Err(ApiError::OAuth {
                    code: "expired_token".into(),
                    message,
                }),
                Err(err) => Err(err),
            };
        let _ = tx.send(Event::api(ApiEvent::OAuthToken { seq, result }));
        return;
    }
}

fn article_error(err: &ApiError) -> String {
    if err.is_no_article() {
        "No article is available for this bookmark yet. Press r to retry."
            .into()
    } else {
        err.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::article::BlockKind;

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url("example.com/article").unwrap(),
            "https://example.com/article"
        );
        assert_eq!(
            normalize_url("http://example.com").unwrap(),
            "http://example.com"
        );
        assert!(normalize_url("   ").is_err());
        assert!(normalize_url("ftp://example.com").is_err());
        assert!(normalize_url("https://").is_err());
    }

    #[test]
    fn parses_label_lists() {
        assert_eq!(
            parse_labels(" rust, tui ,, rust, read-later "),
            vec!["rust", "tui", "read-later"]
        );
        assert!(parse_labels("  , ").is_empty());
    }

    #[test]
    fn filter_membership() {
        let mut bookmark = Bookmark {
            id: "1".into(),
            read_progress: 0,
            ..Default::default()
        };
        assert!(Filter::Unread.matches(&bookmark));
        assert!(!Filter::Reading.matches(&bookmark));

        bookmark.read_progress = 50;
        assert!(Filter::Reading.matches(&bookmark));
        assert!(Filter::Unread.matches(&bookmark));

        bookmark.read_progress = 100;
        assert!(Filter::Unread.matches(&bookmark));

        bookmark.labels = vec!["rust".into()];
        assert!(Filter::Label("rust".into()).matches(&bookmark));
        assert!(!Filter::Label("tui".into()).matches(&bookmark));

        bookmark.is_archived = true;
        assert!(Filter::Label("rust".into()).matches(&bookmark));
        assert!(!Filter::Unread.matches(&bookmark));
        assert!(Filter::Archived.matches(&bookmark));
    }

    #[test]
    fn fallback_article_uses_description() {
        let bookmark = Bookmark {
            description: Some("A description".into()),
            note: Some("A note".into()),
            ..Default::default()
        };
        let article = fallback_article(&bookmark);
        assert!(
            article
                .blocks
                .iter()
                .any(|b| b.text.contains("A description"))
        );
        assert!(article.blocks.iter().any(|b| b.text.contains("A note")));
    }

    #[test]
    fn reader_progress_tracks_position() {
        let bookmark = Bookmark {
            has_article: true,
            ..Default::default()
        };
        let mut reader = Reader::new(bookmark, true, 0, true);
        reader.viewport = 10;
        let blocks = (0..40)
            .map(|i| {
                (
                    "",
                    format!("Paragraph number {i} with enough text to wrap a little."),
                )
            })
            .collect();
        reader.set_article(Article::from_text_blocks(blocks));

        assert!(reader.lines.len() > reader.viewport);
        assert_eq!(reader.half_page_amount(), 5);
        assert_eq!(reader.progress(), 0);
        assert!(!reader.scrolled);

        let max = reader.max_scroll();
        reader.jump_to(max);
        assert_eq!(reader.progress(), 100);
        assert!(reader.scrolled);

        reader.scroll_to(0);
        assert_eq!(reader.progress(), 0);
    }

    #[test]
    fn escape_clears_article_search_before_closing_reader() {
        let mut app = test_app_with_bookmarks(1);
        app.bookmarks[0].has_article = false;
        app.open_reader();
        if let Some(reader) = &mut app.reader {
            reader.viewport = 1;
            reader.set_article(Article::from_text_blocks(vec![
                ("", "alpha first".into()),
                ("", "alpha second".into()),
            ]));
            assert_eq!(reader.search("alpha"), 2);
        }

        app.handle_reader_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.reader.is_some());
        assert!(app.reader.as_ref().unwrap().search_query.is_none());

        app.handle_reader_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.reader.is_none());

        app.open_reader();
        if let Some(reader) = &mut app.reader {
            assert_eq!(reader.search("no-such-text"), 0);
        }
        app.handle_reader_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.reader.is_none());
    }

    #[test]
    fn article_search_finds_and_navigates_matches() {
        let bookmark = Bookmark {
            has_article: true,
            ..Default::default()
        };
        let mut reader = Reader::new(bookmark, true, 0, true);
        reader.viewport = 1;
        reader.set_article(Article::from_text_blocks(vec![
            ("", "alpha first".into()),
            ("", "beta".into()),
            ("", "Alpha second".into()),
        ]));

        assert_eq!(reader.search("alpha"), 2);
        assert_eq!(reader.scroll, 0);

        reader.next_search_match(true);
        assert_eq!(reader.scroll, 4);

        reader.next_search_match(true);
        assert_eq!(reader.scroll, 4);

        reader.next_search_match(false);
        assert_eq!(reader.scroll, 0);

        reader.next_search_match(true);
        assert_eq!(reader.scroll, 4);
        reader.next_search_match(false);
        assert_eq!(reader.scroll, 0);

        assert_eq!(reader.search(""), 0);
        assert!(reader.search_query.is_none());
    }

    #[test]
    fn space_scrolls_to_the_next_paragraph() {
        let bookmark = Bookmark {
            has_article: true,
            ..Default::default()
        };
        let mut reader = Reader::new(bookmark, true, 0, true);
        reader.viewport = 2;
        reader.set_article(Article::from_text_blocks(vec![
            ("", "First paragraph".into()),
            ("", "Second paragraph".into()),
            ("", "Third paragraph".into()),
        ]));

        reader.scroll_to_next_paragraph();
        assert_eq!(reader.scroll, 2);
        assert!(reader.lines[2].kind.is_some());
        assert!(reader.lines[1].kind.is_none());
        assert!(reader.scrolled);
    }

    #[test]
    fn reader_restores_saved_progress() {
        let bookmark = Bookmark {
            has_article: true,
            read_progress: 50,
            ..Default::default()
        };
        let mut reader = Reader::new(bookmark, true, 0, true);
        reader.viewport = 10;
        let blocks = (0..40)
            .map(|i| {
                (
                    "",
                    format!("Paragraph number {i} with enough text to wrap a little."),
                )
            })
            .collect();
        reader.set_article(Article::from_text_blocks(blocks));

        let max = reader.max_scroll();
        assert!(max > 0);
        let expected = (max as f32 * 0.5).round() as usize;
        assert_eq!(reader.scroll, expected);
        assert!((49..=51).contains(&reader.progress()));
        assert!(!reader.scrolled);

        reader.scroll_by(1);
        assert!(reader.scrolled);
    }

    fn test_app_with_bookmarks(count: usize) -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), PathBuf::from("/tmp/eckly-test"), tx);
        app.screen = Screen::Main;
        app.filter = Filter::All;
        app.bookmarks = (0..count)
            .map(|i| Bookmark {
                id: format!("bookmark-{i}"),
                title: Some(format!("Bookmark {i}")),
                ..Default::default()
            })
            .collect();
        app.total = count;
        app.list_state.select(Some(0));
        app
    }

    #[test]
    fn reader_respects_maximum_text_width() {
        let bookmark = Bookmark {
            has_article: true,
            ..Default::default()
        };
        let mut reader = Reader::new(bookmark, true, 40, true);
        reader.viewport = 10;
        reader.set_article(Article::from_text_blocks(vec![(
            "",
            "A long paragraph that would normally use the full terminal width. ".repeat(10),
        )]));
        reader.reflow(100);

        assert_eq!(reader.width, 40);
        assert!(
            reader
                .lines
                .iter()
                .all(|line| line.text.chars().count() <= 40)
        );
    }

    #[test]
    fn reading_view_can_delete_the_bookmark() {
        let mut app = test_app_with_bookmarks(1);
        app.bookmarks[0].has_article = false;
        app.open_reader();

        app.handle_reader_key(KeyEvent::new(
            KeyCode::Char('D'),
            KeyModifiers::NONE,
        ));

        match app.modal.as_ref().unwrap() {
            Modal::Confirm(confirm) => match &confirm.action {
                ConfirmAction::DeleteBookmarks { ids } => {
                    assert_eq!(ids, &vec!["bookmark-0"]);
                }
            },
            _ => panic!("expected a confirmation modal"),
        }
    }

    fn archive_test_app(
        archive_closes_reader: bool,
        archived: bool,
    ) -> (App, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let config = Config {
            archive_closes_reader,
            ..Default::default()
        };
        let mut app = App::new(config, PathBuf::from("/tmp/eckly-test"), tx);
        app.screen = Screen::Main;
        app.filter = Filter::Unread;
        let bookmark = Bookmark {
            id: "article".into(),
            has_article: true,
            is_archived: archived,
            ..Default::default()
        };
        app.bookmarks = vec![bookmark.clone()];
        app.total = 1;
        app.list_state.select(Some(0));

        let mut reader = Reader::new(bookmark, true, 0, true);
        reader.viewport = 10;
        let blocks = (0..40)
            .map(|i| ("", format!("Paragraph {i} of the test article.")))
            .collect();
        reader.set_article(Article::from_text_blocks(blocks));
        app.reader = Some(reader);
        (app, rx)
    }

    fn press_a(app: &mut App) {
        app.handle_reader_key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        ));
    }

    fn offline_cache(name: &str) -> Cache {
        let dir = std::env::temp_dir()
            .join(format!("eckly-{name}-test-{}", std::process::id()));
        Cache::new(dir)
    }

    #[test]
    fn archiving_keeps_the_reader_open_by_default() {
        let (mut app, _rx) = archive_test_app(false, false);
        app.cache = Some(offline_cache("archive-default"));
        app.offline_mode = true;

        press_a(&mut app);

        let reader = app.reader.as_ref().unwrap();
        assert!(reader.bookmark.is_archived);
    }

    #[test]
    fn archiving_can_close_the_reader() {
        let (mut app, _rx) = archive_test_app(true, false);
        app.cache = Some(offline_cache("archive-close"));
        app.offline_mode = true;

        press_a(&mut app);

        assert!(app.reader.is_none());
        assert!(app.bookmarks.is_empty());
        let pending = app.cache.as_ref().unwrap().pending_updates();
        assert_eq!(pending["article"].is_archived, Some(true));
    }

    #[tokio::test]
    async fn archiving_and_closing_sends_one_update_with_progress() {
        let (mut app, mut rx) = archive_test_app(true, false);
        app.client =
            Some(ReadeckClient::new("http://127.0.0.1:1", "token").unwrap());
        app.reader_scroll(5);
        let progress = app.reader.as_ref().unwrap().progress();
        assert!(progress > 0);

        press_a(&mut app);
        assert!(app.reader.is_none());

        let mut updates = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            rx.recv(),
        )
        .await
        {
            if let Event::Api(api) = event
                && let ApiEvent::Updated { patch, .. } = *api
            {
                updates.push(patch);
            }
        }
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].is_archived, Some(true));
        assert_eq!(updates[0].read_progress, Some(progress));
    }

    #[test]
    fn restoring_from_the_archive_keeps_the_reader_open() {
        let (mut app, _rx) = archive_test_app(true, true);
        app.filter = Filter::Archived;
        app.cache = Some(offline_cache("archive-restore"));
        app.offline_mode = true;

        press_a(&mut app);

        let reader = app.reader.as_ref().unwrap();
        assert!(!reader.bookmark.is_archived);
    }

    #[test]
    fn text_alignment_can_be_toggled_in_the_reader() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), PathBuf::from("/tmp/eckly-test"), tx);
        app.reader = Some(Reader::new(Bookmark::default(), true, 80, true));

        app.toggle_reader_alignment();
        assert!(!app.reader.as_ref().unwrap().center_text);
        assert_eq!(app.config.text_align, "left");

        app.toggle_reader_alignment();
        assert!(app.reader.as_ref().unwrap().center_text);
        assert_eq!(app.config.text_align, "center");
    }

    fn graphics_reader(html: &str) -> Reader {
        let mut reader = Reader::new(Bookmark::default(), true, 0, true);
        reader.graphics = Some(Picker::halfblocks());
        reader.set_article(parse_article(html));
        reader
    }

    #[test]
    fn graphics_encode_once_per_column_width() {
        let mut reader =
            graphics_reader(r#"<p>Text</p><img src="/a.png" alt="A">"#);
        assert_eq!(reader.width, 60);
        let image = DynamicImage::new_rgba8(100, 40);
        reader.set_image("/a.png".into(), image.clone());

        let pending = reader.pending_encodes();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "/a.png");
        assert!(reader.pending_encodes().is_empty());
        assert!(reader.lines.iter().any(|line| line.text == "[image: A]"));

        let picker = Picker::halfblocks();
        reader.set_graphic(
            "/a.png".into(),
            graphics::encode(&picker, image.clone(), 60).unwrap(),
        );
        assert!(reader.pending_encodes().is_empty());
        let rows = reader
            .lines
            .iter()
            .filter(|line| line.kind == Some(BlockKind::Image))
            .count();
        assert_eq!(rows, 2);

        reader.reflow(30);
        assert!(reader.lines.iter().any(|line| line.text == "[image: A]"));
        assert_eq!(reader.pending_encodes().len(), 1);

        reader.set_graphic(
            "/a.png".into(),
            graphics::encode(&picker, image, 60).unwrap(),
        );
        assert!(reader.lines.iter().any(|line| line.text == "[image: A]"));
    }

    #[test]
    fn graphics_off_keeps_half_block_rows() {
        let mut reader = Reader::new(Bookmark::default(), true, 0, true);
        reader.set_article(parse_article(r#"<img src="/a.png" alt="A">"#));
        reader.set_image("/a.png".into(), DynamicImage::new_rgba8(10, 10));

        assert!(reader.pending_encodes().is_empty());
        assert!(reader.lines.iter().any(|line| line.image.is_some()));
    }

    #[test]
    fn visible_graphics_follow_the_scroll() {
        let text: String =
            (0..30).map(|n| format!("<p>Line {n}</p>")).collect();
        let mut reader = graphics_reader(&format!(
            r#"<p>Text</p><img src="/a.png" alt="A">{text}"#
        ));
        reader.viewport = 5;
        let image = DynamicImage::new_rgba8(100, 200);
        reader.set_image("/a.png".into(), image.clone());
        reader.set_graphic(
            "/a.png".into(),
            graphics::encode(&Picker::halfblocks(), image, 60).unwrap(),
        );
        let positions = |reader: &Reader| -> Vec<(i16, i16)> {
            reader
                .visible_graphics()
                .iter()
                .map(|(position, _)| (position.x, position.y))
                .collect()
        };

        reader.scroll_to(0);
        assert_eq!(positions(&reader), vec![(25, 2)]);
        reader.scroll_to(4);
        assert_eq!(positions(&reader), vec![(25, -2)]);
        reader.scroll_to(12);
        assert!(positions(&reader).is_empty());
    }

    #[test]
    fn graphics_are_drawn_in_the_reader() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Color;

        let mut app = test_app_with_bookmarks(1);
        let mut reader =
            graphics_reader(r#"<p>Text</p><img src="/a.png" alt="A">"#);
        let red = image::Rgba([255, 0, 0, 255]);
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            100, 40, red,
        ));
        reader.set_image("/a.png".into(), image);
        app.reader = Some(reader);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut has_red = |app: &mut App| {
            terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
            terminal.backend().buffer().content().iter().any(|cell| {
                cell.fg == Color::Rgb(255, 0, 0)
                    || cell.bg == Color::Rgb(255, 0, 0)
            })
        };
        assert!(!has_red(&mut app));

        let reader = app.reader.as_mut().unwrap();
        let (url, image) = reader.pending_encodes().remove(0);
        let graphic =
            graphics::encode(&Picker::halfblocks(), image, reader.width)
                .unwrap();
        reader.set_graphic(url, graphic);
        assert!(has_red(&mut app));
    }

    #[test]
    fn images_can_be_toggled_in_the_reader() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let config = Config {
            images: false,
            ..Default::default()
        };
        let mut app = App::new(config, PathBuf::from("/tmp/eckly-test"), tx);
        assert!(!app.images_enabled);

        let mut reader = Reader::new(Bookmark::default(), false, 0, true);
        reader.set_article(parse_article(
            r#"<p>Text</p><img src="/image.png" alt="Image">"#,
        ));
        app.reader = Some(reader);

        app.toggle_reader_images();
        assert!(app.images_enabled);
        assert!(app.reader.as_ref().unwrap().show_images);

        app.toggle_reader_images();
        assert!(!app.images_enabled);
        assert!(!app.reader.as_ref().unwrap().show_images);
    }

    #[test]
    fn opening_reader_keeps_selection() {
        let mut app = test_app_with_bookmarks(2);
        app.bookmarks[0].has_article = false;
        app.selected_ids.insert("bookmark-0".into());
        app.selected_ids.insert("bookmark-1".into());

        app.open_reader();

        assert!(app.reader.is_some());
        assert_eq!(app.selected_ids.len(), 2);
    }

    #[test]
    fn library_search_cursor_follows_typed_text() {
        use ratatui::Terminal;
        use ratatui::backend::{Backend, TestBackend};

        let mut app = test_app_with_bookmarks(1);
        app.focus = Focus::Sidebar;
        app.sidebar_search = TextInput::new("read");
        app.sidebar_search_active = true;
        app.rebuild_sidebar();

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();

        // Left border, the " / " prompt, then the four typed characters.
        let position = terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!((position.x, position.y), (8, 1));
    }

    fn press(app: &mut App, code: KeyCode) {
        let modifiers = match code {
            KeyCode::Char(c) if c.is_ascii_uppercase() => KeyModifiers::SHIFT,
            _ => KeyModifiers::NONE,
        };
        app.handle_key(KeyEvent::new(code, modifiers));
    }

    #[test]
    fn shift_l_toggles_the_library_pane() {
        let mut app = test_app_with_bookmarks(3);
        assert!(!app.sidebar_hidden);

        press(&mut app, KeyCode::Char('L'));
        assert!(app.sidebar_hidden);

        press(&mut app, KeyCode::Char('L'));
        assert!(!app.sidebar_hidden);
    }

    /// Draws the app on a 60x12 test terminal and returns its rows as text.
    fn render_rows(app: &mut App) -> Vec<String> {
        render_rows_at(app, 60, 12)
    }

    fn render_rows_at(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn hidden_library_pane_gives_the_list_the_full_width() {
        let mut app = test_app_with_bookmarks(3);
        let shown = render_rows(&mut app);
        assert!(shown[0].contains("Library"), "{shown:#?}");

        press(&mut app, KeyCode::Char('L'));
        let hidden = render_rows(&mut app);

        assert!(!hidden.iter().any(|row| row.contains("Library")));
        // One box spans the row: the list's corners sit at both edges.
        let top = &hidden[0];
        assert!(top.starts_with('┌') && top.ends_with('┐'), "{hidden:#?}");
        assert_eq!(top.matches('┌').count(), 1, "{hidden:#?}");
    }

    #[test]
    fn hiding_the_focused_library_pane_focuses_the_list() {
        let mut app = test_app_with_bookmarks(3);
        app.focus = Focus::Sidebar;

        press(&mut app, KeyCode::Char('L'));

        assert!(app.sidebar_hidden);
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn keys_that_focus_the_library_pane_do_nothing_while_it_is_hidden() {
        for filter in [Filter::All, Filter::Highlights] {
            for code in [
                KeyCode::Tab,
                KeyCode::Char('h'),
                KeyCode::Left,
                KeyCode::Esc,
            ] {
                let mut app = test_app_with_bookmarks(3);
                app.filter = filter.clone();
                press(&mut app, KeyCode::Char('L'));

                press(&mut app, code);

                assert!(app.sidebar_hidden, "{filter:?} {code:?}");
                assert_eq!(app.focus, Focus::List, "{filter:?} {code:?}");
            }
        }
    }

    #[test]
    fn hidden_library_pane_stays_hidden_after_reading() {
        let mut app = test_app_with_bookmarks(3);
        press(&mut app, KeyCode::Char('L'));

        press(&mut app, KeyCode::Enter);
        assert!(app.reader.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.reader.is_none());

        assert!(app.sidebar_hidden);
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn shift_l_is_text_in_the_library_filter() {
        let mut app = test_app_with_bookmarks(3);
        app.focus = Focus::Sidebar;
        press(&mut app, KeyCode::Char('/'));

        press(&mut app, KeyCode::Char('L'));

        assert_eq!(app.sidebar_search.value, "L");
        assert!(!app.sidebar_hidden);
        assert_eq!(app.focus, Focus::Sidebar);
    }

    #[test]
    fn hidden_library_pane_puts_the_filter_in_the_list_title() {
        let mut app = test_app_with_bookmarks(3);
        app.filter = Filter::Archived;
        let shown = render_rows(&mut app);
        assert!(shown[0].contains("Bookmarks 3"), "{shown:#?}");

        press(&mut app, KeyCode::Char('L'));
        let hidden = render_rows(&mut app);

        assert!(hidden[0].contains("Archived 3"), "{hidden:#?}");
        assert!(!hidden[0].contains("Bookmarks"), "{hidden:#?}");
    }

    #[test]
    fn showing_the_library_pane_focuses_it() {
        let mut app = test_app_with_bookmarks(3);
        app.rebuild_sidebar();
        app.sidebar_state.select(None);
        press(&mut app, KeyCode::Char('L'));

        press(&mut app, KeyCode::Char('L'));

        assert!(!app.sidebar_hidden);
        assert_eq!(app.focus, Focus::Sidebar);
        assert_eq!(app.sidebar_state.selected(), Some(0));
    }

    #[test]
    fn footer_hints_name_l_while_the_library_pane_is_hidden() {
        for filter in [Filter::All, Filter::Highlights] {
            let mut app = test_app_with_bookmarks(3);
            app.filter = filter.clone();
            let shown = render_rows_at(&mut app, 160, 12);
            let footer = shown.last().unwrap();
            assert!(footer.contains("Tab filters"), "{filter:?} {footer}");
            assert!(!footer.contains("L library"), "{filter:?} {footer}");

            press(&mut app, KeyCode::Char('L'));
            let hidden = render_rows_at(&mut app, 160, 12);
            let footer = hidden.last().unwrap();

            assert!(footer.contains("L library"), "{filter:?} {footer}");
            assert!(!footer.contains("Tab filters"), "{filter:?} {footer}");
        }
    }

    #[test]
    fn selection_maps_to_article_offsets() {
        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                ..Default::default()
            },
            false,
            0,
            true,
        );
        reader.set_article(parse_article("<p>alpha beta</p><p>second</p>"));
        reader.start_selection();
        let first = reader.selection.unwrap().cursor;
        assert_eq!(first, TextPos { line: 0, offset: 0 });

        for _ in 0..5 {
            reader.selection_right();
        }
        reader.selection.as_mut().unwrap().anchor = Some(first);
        assert_eq!(
            reader.annotation_selection(),
            Some(("p[1]".into(), 0, "p[1]".into(), 5))
        );

        // Extending into the next block uses both selectors.
        reader.selection.as_mut().unwrap().cursor =
            TextPos { line: 2, offset: 3 };
        assert_eq!(
            reader.annotation_selection(),
            Some(("p[1]".into(), 0, "p[2]".into(), 3))
        );

        // Left at the start of a line moves to the previous line's end.
        reader.selection.as_mut().unwrap().cursor =
            TextPos { line: 2, offset: 0 };
        reader.selection_left();
        assert_eq!(reader.selection.unwrap().cursor.line, 0);
        assert_eq!(reader.selection.unwrap().cursor.offset, 10);
    }

    #[test]
    fn selection_supports_vim_motions() {
        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                ..Default::default()
            },
            false,
            0,
            true,
        );
        reader.set_article(parse_article(
            "<p>one two three</p><p>foo.bar baz</p>",
        ));
        reader.viewport = 5;
        reader.start_selection();
        let pos = |line, offset| TextPos { line, offset };

        reader.selection_word(true, false);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 4));
        reader.selection_word(true, false);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 8));
        reader.selection_word(false, false);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 4));

        // End of word with `e`, and end of WORD with `E`.
        reader.selection.as_mut().unwrap().cursor = pos(0, 0);
        reader.selection_word_end(false);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 2));
        reader.selection_word_end(false);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 6));
        reader.selection_word_end(false);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 12));
        reader.selection.as_mut().unwrap().cursor = pos(2, 0);
        reader.selection_word_end(false);
        assert_eq!(reader.selection.unwrap().cursor, pos(2, 2));
        reader.selection_word_end(false);
        assert_eq!(reader.selection.unwrap().cursor, pos(2, 3));
        reader.selection.as_mut().unwrap().cursor = pos(2, 0);
        reader.selection_word_end(true);
        assert_eq!(reader.selection.unwrap().cursor, pos(2, 6));

        // Punctuation separates words, not WORDS.
        reader.selection.as_mut().unwrap().cursor = pos(2, 0);
        reader.selection_word(true, false);
        assert_eq!(reader.selection.unwrap().cursor, pos(2, 3));
        reader.selection.as_mut().unwrap().cursor = pos(2, 0);
        reader.selection_word(true, true);
        assert_eq!(reader.selection.unwrap().cursor, pos(2, 8));
        reader.selection.as_mut().unwrap().cursor = pos(2, 8);
        reader.selection_word(false, true);
        assert_eq!(reader.selection.unwrap().cursor, pos(2, 0));

        // Line edges.
        reader.selection.as_mut().unwrap().cursor = pos(0, 2);
        reader.selection_line_edge(false);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 0));
        reader.selection_line_edge(true);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 13));

        // Paragraph moves.
        reader.selection.as_mut().unwrap().cursor = pos(0, 5);
        reader.selection_block(1);
        assert_eq!(reader.selection.unwrap().cursor, pos(2, 0));
        reader.selection_block(-1);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 0));

        // Words continue across wrapped lines.
        reader.reflow(10);
        reader.selection = None;
        reader.start_selection();
        reader.selection_word(true, false);
        assert_eq!(reader.selection.unwrap().cursor, pos(0, 4));
        reader.selection_word(true, false);
        assert_eq!(reader.selection.unwrap().cursor, pos(1, 0));
    }

    #[test]
    fn selected_text_is_highlighted() {
        use ratatui::Terminal;
        use ratatui::backend::{Backend, TestBackend};

        let mut app = test_app_with_bookmarks(1);
        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                title: Some("Article".into()),
                has_article: true,
                ..Default::default()
            },
            false,
            0,
            true,
        );
        reader.set_article(parse_article("<p>alpha beta</p>"));
        app.reader = Some(reader);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();

        {
            let reader = app.reader.as_mut().unwrap();
            reader.start_selection();
            let anchor = reader.selection.unwrap().cursor;
            for _ in 0..5 {
                reader.selection_right();
            }
            reader.selection.as_mut().unwrap().anchor = Some(anchor);
        }
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let selected = crate::ui::theme::bg_selected();
        let mut checked = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width.saturating_sub(5) {
                let word: String =
                    (x..x + 5).map(|x| buffer[(x, y)].symbol()).collect();
                if word == "alpha" {
                    for position in x..x + 5 {
                        assert_eq!(buffer[(position, y)].bg, selected);
                    }
                    checked = true;
                }
            }
        }
        assert!(checked, "article text was not rendered");

        let position = terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!((position.x, position.y), (7, 4));
    }

    #[test]
    fn offline_reader_uses_cached_annotations() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cache = Cache::new(
            std::env::temp_dir()
                .join(format!("eckly-annotations-test-{}", std::process::id())),
        );
        cache.set_annotations(
            "b1",
            vec![Annotation {
                id: "a1".into(),
                start_selector: "p[1]".into(),
                ..Default::default()
            }],
        );
        let mut app = App::with_cache(
            Config::default(),
            PathBuf::from("/tmp/eckly-test"),
            tx,
            Some(cache),
            true,
        );
        app.screen = Screen::Main;
        app.bookmarks = vec![Bookmark {
            id: "b1".into(),
            description: Some("An article".into()),
            ..Default::default()
        }];
        app.list_state.select(Some(0));

        app.open_reader();
        assert_eq!(app.reader.as_ref().unwrap().annotations.len(), 1);
    }

    #[test]
    fn annotations_map_to_article_blocks() {
        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                ..Default::default()
            },
            false,
            0,
            true,
        );
        reader.set_article(parse_article("<p>first</p><p>second</p>"));
        reader.viewport = 1;
        reader.annotations = vec![Annotation {
            id: "a1".into(),
            start_selector: "p[2]".into(),
            ..Default::default()
        }];

        let block = reader.annotation_block(&reader.annotations[0]).unwrap();
        assert_eq!(reader.lines[0].block, Some(0));
        assert!(reader.jump_to_block(block));
        assert_eq!(reader.lines[reader.scroll].block, Some(1));
        assert_eq!(reader.annotation_block(&reader.annotations[0]), Some(1));
    }

    #[test]
    fn annotated_paragraphs_use_the_annotation_color() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = test_app_with_bookmarks(1);
        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                title: Some("Article".into()),
                has_article: true,
                ..Default::default()
            },
            false,
            0,
            true,
        );
        reader.set_article(parse_article("<p>alpha beta gamma</p>"));
        reader.annotations = vec![Annotation {
            id: "a1".into(),
            start_selector: "p[1]".into(),
            start_offset: 0,
            end_selector: "p[1]".into(),
            end_offset: 5,
            color: "red".into(),
            ..Default::default()
        }];
        app.reader = Some(reader);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let expected = crate::ui::annotation_color("red");
        let mut checked = false;
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if let Some(index) = row.find("alpha") {
                assert_eq!(buffer[(index as u16, y)].fg, expected);
                checked = true;
            }
        }
        assert!(checked, "article text was not rendered");
    }

    #[test]
    fn highlights_follow_all_bookmarks_in_the_sidebar() {
        let mut app = test_app_with_bookmarks(1);
        app.rebuild_sidebar();
        let texts: Vec<&str> = app
            .sidebar_items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        let all = texts
            .iter()
            .position(|text| *text == "≡  All Bookmarks")
            .unwrap();
        assert_eq!(texts[all + 1], "✎  Highlights");
    }

    #[test]
    fn highlights_view_renders_annotations() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = test_app_with_bookmarks(1);
        app.filter = Filter::Highlights;
        let text = format!("start {} endmarker", "filler ".repeat(30));
        app.annotation_summaries = Some(HashMap::from([(
            "b1".to_string(),
            vec![Annotation {
                id: "a1".into(),
                bookmark_id: "b1".into(),
                bookmark_title: "The article".into(),
                text,
                note: "a note".into(),
                color: "yellow".into(),
                ..Default::default()
            }],
        )]));
        app.rebuild_highlights();
        app.list_state.select(Some(0));

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert!(rows.iter().any(|row| row.contains("Highlights 1")));
        let title_row = rows
            .iter()
            .position(|row| row.contains("The article"))
            .unwrap();
        let first_row = rows
            .iter()
            .position(|row| row.contains("▍") && row.contains("start"))
            .unwrap();
        assert!(title_row < first_row);
        let last_row = rows
            .iter()
            .position(|row| row.contains("endmarker"))
            .unwrap();
        assert!(last_row > first_row);
        assert!(rows.iter().filter(|row| row.contains("filler")).count() > 1,);
        let note_row =
            rows.iter().position(|row| row.contains("a note")).unwrap();
        assert!(note_row > last_row);
    }

    #[test]
    fn highlights_group_by_bookmark_and_follow_article_order() {
        let created = |day: u32| {
            Some(
                chrono::DateTime::parse_from_rfc3339(&format!(
                    "2026-11-{day:02}T00:00:00Z"
                ))
                .unwrap()
                .with_timezone(&chrono::Utc),
            )
        };
        let annotation =
            |id: &str, bookmark: &str, selector: &str, day: u32| Annotation {
                id: id.into(),
                bookmark_id: bookmark.into(),
                bookmark_title: bookmark.into(),
                start_selector: selector.into(),
                created: created(day),
                text: id.into(),
                ..Default::default()
            };
        let mut app = test_app_with_bookmarks(1);
        app.annotation_summaries = Some(HashMap::from([
            (
                "b1".to_string(),
                vec![
                    annotation("a2", "b1", "p[5]", 2),
                    annotation("a1", "b1", "p[2]", 1),
                    annotation("a4", "b1", "section[1]/ul[1]/li[3]", 3),
                ],
            ),
            ("b2".to_string(), vec![annotation("a3", "b2", "p[1]", 4)]),
        ]));

        app.rebuild_highlights();

        let ids: Vec<&str> =
            app.highlights.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["a3", "a1", "a2", "a4"]);
    }

    #[test]
    fn highlights_show_the_bookmark_title_once() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = test_app_with_bookmarks(1);
        app.filter = Filter::Highlights;
        let annotation = |id: &str, selector: &str, text: &str| Annotation {
            id: id.into(),
            bookmark_id: "b1".into(),
            bookmark_title: "The article".into(),
            start_selector: selector.into(),
            text: text.into(),
            ..Default::default()
        };
        app.annotation_summaries = Some(HashMap::from([(
            "b1".to_string(),
            vec![
                annotation("a1", "p[1]", "first highlight"),
                annotation("a2", "p[2]", "second highlight"),
            ],
        )]));
        app.rebuild_highlights();
        app.list_state.select(Some(0));

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("The article"))
                .count(),
            1
        );
        let first = rows
            .iter()
            .position(|row| row.contains("first highlight"))
            .unwrap();
        let second = rows
            .iter()
            .position(|row| row.contains("second highlight"))
            .unwrap();
        assert!(first < second);
    }

    #[test]
    fn reader_annotations_keep_bookmark_metadata_in_highlights() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = std::env::temp_dir()
            .join(format!("eckly-summaries-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cache = Cache::new(dir.clone());
        let bookmark = Bookmark {
            id: "b1".into(),
            title: Some("Article".into()),
            url: "https://example.com/1".into(),
            site_name: Some("Example".into()),
            has_article: true,
            ..Default::default()
        };
        cache.upsert_bookmarks(std::slice::from_ref(&bookmark));
        cache
            .write_article("b1", "<p>first</p><p>second paragraph</p>")
            .unwrap();
        // A bookmark's own annotation list omits the bookmark fields.
        let annotation = Annotation {
            id: "a1".into(),
            start_selector: "p[2]".into(),
            start_offset: 0,
            end_selector: "p[2]".into(),
            end_offset: 6,
            text: "second".into(),
            color: "yellow".into(),
            ..Default::default()
        };
        cache.set_annotations("b1", vec![annotation.clone()]);

        let mut app = App::with_cache(
            Config::default(),
            PathBuf::from("/tmp/eckly-test"),
            tx,
            Some(cache),
            true,
        );
        app.screen = Screen::Main;

        let mut reader = Reader::new(bookmark, false, 0, false);
        reader.annotations = vec![annotation];
        app.reader = Some(reader);
        app.annotation_summaries = Some(HashMap::new());
        app.store_annotation_summaries();

        let stored = &app.annotation_summaries.as_ref().unwrap()["b1"];
        assert_eq!(stored[0].bookmark_id, "b1");
        assert_eq!(stored[0].bookmark_title, "Article");
        assert_eq!(stored[0].bookmark_url, "https://example.com/1");
        assert_eq!(stored[0].bookmark_site_name, "Example");

        app.reader = None;
        app.filter = Filter::Highlights;
        app.rebuild_highlights();
        assert_eq!(app.highlights.len(), 1);
        assert_eq!(app.highlights[0].bookmark_title, "Article");
        app.list_state.select(Some(0));

        app.open_selected_highlight();
        let reader = app.reader.as_ref().unwrap();
        assert_eq!(reader.bookmark.id, "b1");
        assert!(reader.error.is_none());
        assert!(!reader.lines.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaving_speed_reading_flashes_the_last_word() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::with_cache(
            Config::default(),
            PathBuf::from("/tmp/eckly-test"),
            tx,
            None,
            true,
        );
        app.screen = Screen::Main;

        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                title: Some("Article".into()),
                has_article: true,
                ..Default::default()
            },
            false,
            0,
            false,
        );
        reader.set_article(parse_article("<p>alpha beta gamma delta</p>"));
        reader.speed = SpeedReader::new(&reader.lines, 0, 300);
        reader
            .speed
            .as_mut()
            .unwrap()
            .advance(Instant::now() + Duration::from_millis(450));
        assert_eq!(reader.speed.as_ref().unwrap().current(), "gamma");

        reader.stop_speed_reading();
        let flash = reader.flash.unwrap();
        assert_eq!((flash.line, flash.start, flash.end), (0, 11, 16));
        assert!(Instant::now() < flash.until);
        assert!(reader.speed.is_none());
        app.reader = Some(reader);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let locate = |buffer: &ratatui::buffer::Buffer| {
            (0..buffer.area.height).find_map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                row.find("gamma")
                    .map(|byte| (row[..byte].chars().count() as u16, y))
            })
        };

        let buffer = terminal.backend().buffer();
        let (x, y) = locate(buffer).expect("the flashed word is drawn");
        for offset in 0..5 {
            let cell = &buffer[(x + offset, y)];
            assert_eq!(cell.bg, crate::ui::theme::accent());
            assert_eq!(cell.fg, crate::ui::theme::bg_base());
        }
        assert_ne!(buffer[(x + 6, y)].bg, crate::ui::theme::accent());

        // Halfway through, the word sits between the accent and the article
        // colors.
        app.reader.as_mut().unwrap().flash.as_mut().unwrap().until =
            Instant::now() + crate::app::FLASH_DURATION / 2;
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (x, y) = locate(buffer).unwrap();
        let middle = buffer[(x, y)].bg;
        assert_ne!(middle, crate::ui::theme::accent());
        assert_ne!(middle, crate::ui::theme::bg_base());

        app.reader.as_mut().unwrap().flash.as_mut().unwrap().until =
            Instant::now() - Duration::from_secs(1);
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (x, y) = locate(buffer).unwrap();
        for offset in 0..5 {
            assert_ne!(buffer[(x + offset, y)].bg, crate::ui::theme::accent());
        }
    }

    #[test]
    fn leaving_speed_reading_scrolls_to_the_word() {
        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                title: Some("Article".into()),
                has_article: true,
                ..Default::default()
            },
            false,
            0,
            false,
        );
        let mut html = String::new();
        for index in 1..=40 {
            html.push_str(&format!("<p>paragraph {index}</p>"));
        }
        reader.set_article(parse_article(&html));
        reader.viewport = 10;
        reader.jump_to(30);
        reader.speed = SpeedReader::new(&reader.lines, 20, 300);

        reader.stop_speed_reading();

        let flash = reader.flash.unwrap();
        assert!(flash.line >= 20);
        assert!(flash.line >= reader.scroll);
        assert!(flash.line < reader.scroll + reader.viewport);
    }

    #[test]
    fn deleting_a_highlight_updates_the_view() {
        let mut app = test_app_with_bookmarks(1);
        app.filter = Filter::Highlights;
        app.annotation_summaries = Some(HashMap::from([(
            "b1".to_string(),
            vec![
                Annotation {
                    id: "a1".into(),
                    bookmark_id: "b1".into(),
                    bookmark_title: "The article".into(),
                    text: "one".into(),
                    ..Default::default()
                },
                Annotation {
                    id: "a2".into(),
                    bookmark_id: "b1".into(),
                    bookmark_title: "The article".into(),
                    text: "two".into(),
                    ..Default::default()
                },
            ],
        )]));
        app.rebuild_highlights();
        assert_eq!(app.highlights.len(), 2);
        app.list_state.select(Some(0));

        app.handle_event(Event::api(ApiEvent::AnnotationDeleted {
            bookmark_id: "b1".into(),
            id: "a1".into(),
            result: Ok(()),
        }));

        assert_eq!(app.highlights.len(), 1);
        assert_eq!(app.highlights[0].id, "a2");
        assert_eq!(app.list_state.selected(), Some(0));
        let summaries = app.annotation_summaries.as_ref().unwrap();
        assert_eq!(summaries["b1"].len(), 1);
        assert_eq!(summaries["b1"][0].id, "a2");
    }

    #[test]
    fn opening_a_highlight_scrolls_the_reader() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = std::env::temp_dir()
            .join(format!("eckly-highlights-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cache = Cache::new(dir.clone());
        cache.upsert_bookmarks(&[Bookmark {
            id: "b1".into(),
            title: Some("Article".into()),
            has_article: true,
            ..Default::default()
        }]);
        let mut html = String::new();
        for index in 1..=30 {
            html.push_str(&format!("<p>paragraph {index}</p>"));
        }
        cache.write_article("b1", &html).unwrap();
        cache.set_annotations(
            "b1",
            vec![Annotation {
                id: "a1".into(),
                start_selector: "p[30]".into(),
                start_offset: 0,
                end_selector: "p[30]".into(),
                end_offset: 11,
                text: "paragraph 30".into(),
                color: "yellow".into(),
                ..Default::default()
            }],
        );

        let mut app = App::with_cache(
            Config::default(),
            PathBuf::from("/tmp/eckly-test"),
            tx,
            Some(cache),
            true,
        );
        app.screen = Screen::Main;
        app.filter = Filter::Highlights;
        app.rebuild_highlights();
        assert_eq!(app.highlights.len(), 1);
        app.list_state.select(Some(0));

        app.open_selected_highlight();
        let reader = app.reader.as_ref().unwrap();
        let block_line = reader
            .lines
            .iter()
            .position(|line| line.block == Some(29))
            .unwrap();
        assert!(reader.scroll <= block_line);
        assert!(block_line < reader.scroll + reader.viewport);
        assert!(app.pending_highlight.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bookmark_list_shows_annotations() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = test_app_with_bookmarks(1);
        app.bookmarks[0].title = Some("Article".into());
        app.annotation_summaries = Some(HashMap::from([(
            "bookmark-0".to_string(),
            vec![
                Annotation {
                    id: "a1".into(),
                    text: "first highlight".into(),
                    color: "red".into(),
                    ..Default::default()
                },
                Annotation {
                    id: "a2".into(),
                    text: "second highlight".into(),
                    color: "green".into(),
                    ..Default::default()
                },
            ],
        )]));

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert!(rows.iter().any(|row| row.contains("first highlight")));
        assert!(rows.iter().any(|row| row.contains("second highlight")));
        assert!(rows.iter().any(|row| row.contains("2 annotations")));

        // Without annotations there is no count.
        app.annotation_summaries = Some(HashMap::new());
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert!(!rows.iter().any(|row| row.contains("annotation")));
    }

    #[test]
    fn annotation_notes_are_shown_inline_unless_selecting() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = test_app_with_bookmarks(1);
        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                title: Some("Article".into()),
                has_article: true,
                ..Default::default()
            },
            false,
            0,
            true,
        );
        reader.set_article(parse_article("<p>alpha beta</p>"));
        reader.annotations = vec![Annotation {
            id: "a1".into(),
            start_selector: "p[1]".into(),
            start_offset: 0,
            end_selector: "p[1]".into(),
            // Includes the trailing space: the note separator must not be
            // highlighted.
            end_offset: 6,
            color: "red".into(),
            note: "my note".into(),
            ..Default::default()
        }];
        app.reader = Some(reader);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let expected = crate::ui::theme::dim();
        let background = crate::ui::theme::bg_base();
        let annotated = crate::ui::annotation_color("red");
        let mut checked = false;
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if let Some(index) = row.find("alpha [my note] beta") {
                // `index` is a byte offset; the border is not ASCII.
                let start = row[..index].chars().count();
                // The separator space is note text, not highlight.
                let space = start + "alpha".chars().count();
                assert_eq!(buffer[(space as u16, y)].fg, expected);
                assert_eq!(buffer[(space as u16, y)].bg, background);
                // The highlighted word keeps its color.
                assert_eq!(buffer[(start as u16, y)].fg, annotated);
                let note = start + "alpha [my note".chars().count();
                assert_eq!(buffer[(note as u16, y)].fg, expected);
                assert_eq!(buffer[(note as u16, y)].bg, background);
                checked = true;
            }
        }
        assert!(checked, "inline note was not rendered");

        // Selection mode hides inline notes.
        {
            let reader = app.reader.as_mut().unwrap();
            reader.start_selection();
            let anchor = reader.selection.unwrap().cursor;
            for _ in 0..3 {
                reader.selection_right();
            }
            reader.selection.as_mut().unwrap().anchor = Some(anchor);
        }
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            assert!(!row.contains("[my note]"));
        }
    }

    #[test]
    fn annotations_modal_opens() {
        let mut app = test_app_with_bookmarks(1);
        let mut reader = Reader::new(
            Bookmark {
                id: "b1".into(),
                ..Default::default()
            },
            false,
            0,
            true,
        );
        reader.set_article(parse_article("<p>first</p>"));
        app.reader = Some(reader);

        app.open_annotations();
        match app.modal.as_ref().unwrap() {
            Modal::Annotations(modal) => {
                assert_eq!(modal.bookmark_id, "b1");
            }
            _ => panic!("expected the annotations modal"),
        }
    }

    #[test]
    fn renaming_a_label_updates_local_state() {
        let mut app = test_app_with_bookmarks(1);
        app.labels = vec![
            Label {
                name: "old".into(),
                count: 2,
            },
            Label {
                name: "keep".into(),
                count: 1,
            },
        ];
        app.bookmarks[0].labels = vec!["old".into(), "keep".into()];
        app.filter = Filter::Label("old".into());
        app.rebuild_sidebar();

        app.apply_label_rename("old", "new");

        assert!(app.labels.iter().any(|l| l.name == "new" && l.count == 2));
        assert!(!app.labels.iter().any(|l| l.name == "old"));
        assert_eq!(
            app.bookmarks[0]
                .labels
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["new", "keep"]
        );
        assert_eq!(app.filter, Filter::Label("new".into()));
        let selected = app.sidebar_state.selected().unwrap();
        assert_eq!(app.sidebar_items[selected].text, "# new");

        // Renaming onto an existing label merges both lists.
        app.apply_label_rename("new", "keep");
        assert_eq!(app.labels.len(), 1);
        assert_eq!(app.labels[0].name, "keep");
        assert_eq!(app.labels[0].count, 3);
        assert_eq!(
            app.bookmarks[0]
                .labels
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["keep"]
        );
    }

    #[test]
    fn rename_key_opens_the_label_prompt() {
        let mut app = test_app_with_bookmarks(1);
        app.labels = vec![Label {
            name: "rust".into(),
            count: 1,
        }];
        app.focus = Focus::Sidebar;
        app.rebuild_sidebar();
        let index = app
            .sidebar_items
            .iter()
            .position(|item| item.filter == Some(Filter::Label("rust".into())))
            .unwrap();
        app.sidebar_state.select(Some(index));

        app.handle_sidebar_key(KeyEvent::new(
            KeyCode::Char('R'),
            KeyModifiers::SHIFT,
        ));

        match app.modal.as_ref().unwrap() {
            Modal::Input(input) => {
                assert_eq!(input.input.value, "rust");
                assert!(matches!(input.kind, InputKind::RenameLabel { .. }));
            }
            _ => panic!("expected an input modal"),
        }
    }

    #[test]
    fn filtering_the_library_resets_the_scroll_offset() {
        let mut app = test_app_with_bookmarks(1);
        app.labels = (0..10)
            .map(|i| Label {
                name: format!("label-{i}"),
                count: 1,
            })
            .collect();
        app.sidebar_viewport = 8;
        app.rebuild_sidebar();
        *app.sidebar_state.offset_mut() = 12;

        app.sidebar_search = TextInput::new("label-3");
        app.sidebar_search_active = true;
        app.rebuild_sidebar();

        assert_eq!(app.sidebar_state.offset(), 0);
        let texts: Vec<&str> = app
            .sidebar_items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(texts.iter().filter(|text| **text == "LABELS").count(), 1);
        assert!(texts.contains(&"# label-3"));
    }

    #[test]
    fn collections_are_listed_before_labels() {
        let mut app = test_app_with_bookmarks(1);
        app.labels = vec![Label {
            name: "rust".into(),
            count: 1,
        }];
        app.collections = vec![Collection {
            id: "c1".into(),
            name: "Read later".into(),
            ..Default::default()
        }];
        app.rebuild_sidebar();

        let texts: Vec<&str> = app
            .sidebar_items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        let collections = texts
            .iter()
            .position(|text| *text == "COLLECTIONS")
            .unwrap();
        let labels = texts.iter().position(|text| *text == "LABELS").unwrap();
        assert_eq!(
            &texts[collections..=labels],
            &["COLLECTIONS", "▤  Read later", "LABELS"]
        );
    }

    #[test]
    fn library_search_filters_labels_and_applies() {
        let mut app = test_app_with_bookmarks(1);
        app.focus = Focus::Sidebar;
        app.labels = vec![
            Label {
                name: "rust".into(),
                count: 1,
            },
            Label {
                name: "rust-lang".into(),
                count: 1,
            },
            Label {
                name: "tui".into(),
                count: 1,
            },
        ];

        app.handle_sidebar_key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        ));
        assert!(app.sidebar_search_active);
        for ch in "rust".chars() {
            app.handle_sidebar_key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            ));
        }

        let texts: Vec<&str> = app
            .sidebar_items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert!(texts.contains(&"# rust"));
        assert!(texts.contains(&"# rust-lang"));
        assert!(!texts.contains(&"# tui"));

        app.handle_sidebar_key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.filter, Filter::Label("rust".into()));
        assert!(!app.sidebar_search_active);
        assert!(app.sidebar_search.value.is_empty());
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn sidebar_page_keys_move_selection() {
        let mut app = test_app_with_bookmarks(1);
        app.focus = Focus::Sidebar;
        app.labels = (0..8)
            .map(|i| Label {
                name: format!("label-{i}"),
                count: 1,
            })
            .collect();
        app.sidebar_viewport = 5;
        app.rebuild_sidebar();
        app.sidebar_state.select(Some(0));

        app.handle_sidebar_key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::NONE,
        ));
        let selected = app.sidebar_state.selected().unwrap();
        assert!(selected > 0);
        assert!(!app.sidebar_items[selected].header);

        app.handle_sidebar_key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        ));
        let after = app.sidebar_state.selected().unwrap();
        assert!(after != selected);
        assert!(!app.sidebar_items[after].header);
    }

    #[test]
    fn list_escape_clears_search_then_selection_then_focuses_library() {
        let mut app = test_app_with_bookmarks(2);
        app.search = "rust".into();
        app.selected_ids.insert("bookmark-0".into());

        app.handle_list_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.search.is_empty());
        assert!(app.selected_ids.is_empty());
        assert_eq!(app.focus, Focus::List);

        app.selected_ids.insert("bookmark-0".into());
        app.handle_list_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.selected_ids.is_empty());
        assert_eq!(app.focus, Focus::List);

        app.handle_list_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Sidebar);
    }

    #[test]
    fn offline_mode_uses_only_the_cache() {
        let dir = std::env::temp_dir()
            .join(format!("eckly-offline-test-{}", std::process::id()));
        let mut cache = Cache::new(dir);
        cache.upsert_bookmarks(&[Bookmark {
            id: "offline1".into(),
            title: Some("Cached bookmark".into()),
            has_article: true,
            ..Default::default()
        }]);
        cache
            .write_article("offline1", "<p>cached body</p>")
            .unwrap();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::with_cache(
            Config::default(),
            PathBuf::from("/tmp/eckly-test"),
            tx,
            Some(cache),
            true,
        );
        app.bootstrap();

        assert_eq!(app.screen, Screen::Main);
        assert!(app.offline);
        assert!(app.offline_mode);
        assert!(app.client.is_none());
        assert_eq!(app.bookmarks.len(), 1);

        app.open_reader();
        let reader = app.reader.as_ref().unwrap();
        assert!(!reader.loading);
        assert!(reader.article.is_some());
    }

    #[test]
    fn unread_is_the_default_and_first_library_section() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), PathBuf::from("/tmp/eckly-test"), tx);
        assert_eq!(app.filter, Filter::Unread);

        app.rebuild_sidebar();
        let filters: Vec<Filter> = app
            .sidebar_items
            .iter()
            .filter(|item| !item.header)
            .take(5)
            .filter_map(|item| item.filter.clone())
            .collect();
        assert_eq!(
            filters,
            vec![
                Filter::Unread,
                Filter::Reading,
                Filter::Marked,
                Filter::Archived,
                Filter::All,
            ]
        );
    }

    #[test]
    fn ctrl_a_selects_all_loaded_bookmarks() {
        let mut app = test_app_with_bookmarks(3);

        app.handle_list_key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        ));

        assert_eq!(app.selected_ids.len(), 3);
        assert!(app.selected_ids.contains("bookmark-0"));
        assert!(app.selected_ids.contains("bookmark-1"));
        assert!(app.selected_ids.contains("bookmark-2"));
    }

    #[test]
    fn space_toggles_bookmark_selection() {
        let mut app = test_app_with_bookmarks(2);

        app.handle_list_key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        ));
        assert!(app.selected_ids.contains("bookmark-0"));

        app.handle_list_key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        ));
        assert!(app.selected_ids.is_empty());
    }

    #[test]
    fn actions_target_selection_or_highlight() {
        let mut app = test_app_with_bookmarks(3);
        app.list_state.select(Some(1));
        app.selected_ids.insert("bookmark-0".into());
        app.selected_ids.insert("bookmark-2".into());

        let ids: Vec<String> = app
            .action_targets()
            .into_iter()
            .map(|bookmark| bookmark.id)
            .collect();
        assert_eq!(ids, vec!["bookmark-0", "bookmark-2"]);

        app.selected_ids.clear();
        let ids: Vec<String> = app
            .action_targets()
            .into_iter()
            .map(|bookmark| bookmark.id)
            .collect();
        assert_eq!(ids, vec!["bookmark-1"]);
    }

    #[test]
    fn bulk_update_applies_patch_and_keeps_selection() {
        let mut app = test_app_with_bookmarks(3);
        app.selected_ids.insert("bookmark-0".into());
        app.selected_ids.insert("bookmark-1".into());
        let ids = vec!["bookmark-0".to_string(), "bookmark-1".to_string()];

        app.handle_api(ApiEvent::BulkUpdated {
            ids,
            patch: BookmarkPatch {
                read_progress: Some(100),
                ..Default::default()
            },
            message: "Marked 2 bookmarks as read".into(),
            quiet: true,
            result: Ok(2),
        });

        assert_eq!(app.bookmarks[0].read_progress, 100);
        assert_eq!(app.bookmarks[1].read_progress, 100);
        assert_eq!(app.bookmarks[2].read_progress, 0);
        assert!(app.selected_ids.contains("bookmark-0"));
        assert!(app.selected_ids.contains("bookmark-1"));
    }

    #[test]
    fn delete_confirmation_targets_the_selection() {
        let mut app = test_app_with_bookmarks(3);
        app.list_state.select(Some(1));
        app.selected_ids.insert("bookmark-0".into());
        app.selected_ids.insert("bookmark-2".into());

        app.confirm_delete();
        match app.modal.as_ref().unwrap() {
            Modal::Confirm(confirm) => match &confirm.action {
                ConfirmAction::DeleteBookmarks { ids } => {
                    assert_eq!(ids, &vec!["bookmark-0", "bookmark-2"]);
                }
            },
            _ => panic!("expected a confirmation modal"),
        }
    }

    #[test]
    fn bulk_delete_removes_all_selected_bookmarks() {
        let mut app = test_app_with_bookmarks(3);
        app.selected_ids.insert("bookmark-0".into());
        app.selected_ids.insert("bookmark-2".into());
        let ids = vec!["bookmark-0".to_string(), "bookmark-2".to_string()];

        app.handle_api(ApiEvent::BulkDeleted { ids, result: Ok(2) });

        assert_eq!(app.bookmarks.len(), 1);
        assert_eq!(app.bookmarks[0].id, "bookmark-1");
        assert_eq!(app.total, 1);
        assert!(app.selected_ids.is_empty());
    }

    #[test]
    fn bulk_label_editor_uses_shared_labels() {
        let mut app = test_app_with_bookmarks(2);
        for bookmark in &mut app.bookmarks {
            bookmark.labels = vec!["rust".into(), "tui".into()];
        }
        app.selected_ids.insert("bookmark-0".into());
        app.selected_ids.insert("bookmark-1".into());

        app.edit_labels();
        match app.modal.as_ref().unwrap() {
            Modal::Input(input) => {
                assert_eq!(input.input.value, "rust, tui");
                assert!(matches!(input.kind, InputKind::EditLabels { .. }));
            }
            _ => panic!("expected an input modal"),
        }

        app.bookmarks[1].labels = vec!["different".into()];
        app.modal = None;
        app.edit_labels();
        match app.modal.as_ref().unwrap() {
            Modal::Input(input) => assert!(input.input.value.is_empty()),
            _ => panic!("expected an input modal"),
        }
    }

    #[test]
    fn tab_cycles_label_completions() {
        let mut app = test_app_with_bookmarks(1);
        app.labels = vec![
            Label {
                name: "read-later".into(),
                count: 1,
            },
            Label {
                name: "rust".into(),
                count: 1,
            },
            Label {
                name: "rust-lang".into(),
                count: 1,
            },
            Label {
                name: "tui".into(),
                count: 1,
            },
        ];
        app.modal = Some(Modal::Input(InputModal::new(
            "Edit labels",
            "",
            TextInput::new("ru"),
            InputKind::EditLabels {
                ids: vec!["bookmark-0".into()],
            },
        )));

        app.handle_modal_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(label_modal_value(&app), "rust");
        app.handle_modal_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(label_modal_value(&app), "rust-lang");
        app.handle_modal_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(label_modal_value(&app), "rust");

        assert_eq!(
            label_suggestions(&app.labels, "tui, re", 8),
            vec!["read-later"]
        );
    }

    #[test]
    fn label_jump_finds_label_boundaries() {
        let value = "rust, tui ,cli";
        // Label starts are at 0, 6 and 11.
        assert_eq!(label_jump(value, 14, false), 11);
        assert_eq!(label_jump(value, 11, false), 6);
        assert_eq!(label_jump(value, 6, false), 0);
        assert_eq!(label_jump(value, 0, false), 0);
        assert_eq!(label_jump(value, 0, true), 6);
        assert_eq!(label_jump(value, 6, true), 11);
        assert_eq!(label_jump(value, 11, true), 14);
        assert_eq!(label_jump(value, 14, true), 14);

        assert_eq!(label_jump("  ,  ", 3, false), 0);
        assert_eq!(label_jump("  ,  ", 3, true), 5);
    }

    #[test]
    fn alt_arrows_jump_between_labels_in_the_editor() {
        let mut app = test_app_with_bookmarks(1);
        app.modal = Some(Modal::Input(InputModal::new(
            "Edit labels",
            "",
            TextInput::new("rust, tui, cli"),
            InputKind::EditLabels {
                ids: vec!["bookmark-0".into()],
            },
        )));

        let alt_left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        let alt_right = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);

        app.handle_modal_key(alt_left);
        assert_eq!(label_modal_cursor(&app), 11);
        app.handle_modal_key(alt_left);
        assert_eq!(label_modal_cursor(&app), 6);
        app.handle_modal_key(alt_right);
        assert_eq!(label_modal_cursor(&app), 11);
        app.handle_modal_key(alt_right);
        assert_eq!(label_modal_cursor(&app), 14);
        assert_eq!(label_modal_value(&app), "rust, tui, cli");
    }

    fn label_modal_cursor(app: &App) -> usize {
        match app.modal.as_ref().unwrap() {
            Modal::Input(input) => input.input.cursor,
            _ => panic!("expected an input modal"),
        }
    }

    fn label_modal_value(app: &App) -> String {
        match app.modal.as_ref().unwrap() {
            Modal::Input(input) => input.input.value.clone(),
            _ => panic!("expected an input modal"),
        }
    }

    #[test]
    fn ctrl_w_deletes_a_word_in_search_and_label_dialogs() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), PathBuf::from("/tmp/eckly-test"), tx);

        for kind in [
            InputKind::Search,
            InputKind::EditLabels {
                ids: vec!["bookmark-0".into()],
            },
        ] {
            app.modal = Some(Modal::Input(InputModal::new(
                "Edit",
                "",
                TextInput::new("rust tui"),
                kind,
            )));

            app.handle_modal_key(KeyEvent::new(
                KeyCode::Char('w'),
                KeyModifiers::CONTROL,
            ));

            match app.modal.as_ref().unwrap() {
                Modal::Input(input) => assert_eq!(input.input.value, "rust "),
                _ => panic!("expected an input modal"),
            }
        }
    }

    #[test]
    fn ctrl_d_and_ctrl_u_scroll_list_half_a_page() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), PathBuf::from("/tmp/eckly-test"), tx);
        app.bookmarks = (0..20)
            .map(|i| Bookmark {
                id: format!("bookmark-{i}"),
                ..Default::default()
            })
            .collect();
        app.total = app.bookmarks.len();
        app.list_viewport = 10;
        app.list_state.select(Some(0));

        app.handle_list_key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.list_state.selected(), Some(5));

        app.handle_list_key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.list_state.selected(), Some(0));

        app.handle_list_key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.list_state.selected(), Some(10));
    }

    #[test]
    fn ctrl_d_and_ctrl_u_scroll_half_a_page() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), PathBuf::from("/tmp/eckly-test"), tx);

        let bookmark = Bookmark {
            has_article: true,
            ..Default::default()
        };
        let mut reader = Reader::new(bookmark, true, 0, true);
        reader.viewport = 10;
        let blocks = (0..40)
            .map(|i| {
                ("", format!("Paragraph number {i}, long enough to wrap."))
            })
            .collect();
        reader.set_article(Article::from_text_blocks(blocks));
        app.reader = Some(reader);

        app.handle_reader_key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.reader.as_ref().unwrap().scroll, 5);

        app.handle_reader_key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.reader.as_ref().unwrap().scroll, 0);

        app.handle_reader_key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.reader.as_ref().unwrap().scroll, 0);
    }
}
