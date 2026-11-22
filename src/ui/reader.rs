use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui_image::sliced::SlicedImage;

use crate::app::App;
use crate::article::{BlockKind, ImageCell};
use crate::models::ReadStatus;
use crate::speed::SpeedReader;

use unicode_width::UnicodeWidthChar;

use super::theme;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let [header_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(area);

    let reader_progress;
    let reader_meta;
    let reader_search;
    let speed_reading;
    let selecting;

    {
        let Some(reader) = app.reader.as_mut() else {
            return;
        };
        speed_reading = reader.speed.is_some();
        selecting = reader.selection.is_some();

        let block = Block::bordered()
            .border_style(Style::new().fg(theme::border()))
            .padding(Padding::horizontal(1));
        let inner = block.inner(body_area);

        let content_width = if reader.max_text_width == 0 {
            inner.width
        } else {
            inner
                .width
                .min(reader.max_text_width.clamp(12, u16::MAX as usize) as u16)
        }
        .max(1);
        let offset = if reader.center_text {
            inner.width.saturating_sub(content_width) / 2
        } else {
            0
        };
        let content_area =
            Rect::new(inner.x + offset, inner.y, content_width, inner.height);

        reader.viewport = inner.height.max(1) as usize;
        reader.reflow(content_width.max(12) as usize);
        let scroll = reader.scroll;
        reader.scroll_to(scroll);
        let live_progress = reader.progress();

        let bookmark = &reader.bookmark;
        let (icon, icon_color) = match bookmark.status() {
            ReadStatus::Unread => ("●", theme::accent()),
            ReadStatus::Reading => ("◐", theme::accent_alt()),
            ReadStatus::Read => ("○", theme::faint()),
        };

        let mut title_spans = vec![
            Span::styled(format!("{icon} "), Style::new().fg(icon_color)),
            Span::styled(
                super::truncate_width(
                    &bookmark.display_title(),
                    area.width.saturating_sub(6) as usize,
                ),
                Style::new()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if bookmark.is_marked {
            title_spans
                .push(Span::styled(" ★", Style::new().fg(theme::accent_alt())));
        }
        if bookmark.is_archived {
            title_spans
                .push(Span::styled(" ■", Style::new().fg(theme::faint())));
        }

        let mut meta_parts = Vec::new();
        let host = bookmark.host();
        if !host.is_empty() {
            meta_parts.push(host);
        }
        let authors = bookmark.author_line();
        if !authors.is_empty() {
            meta_parts.push(authors);
        }
        let date = super::human_date(bookmark.published_or_created());
        if !date.is_empty() {
            meta_parts.push(date);
        }
        if bookmark.reading_time > 0 {
            meta_parts.push(format!("{} min read", bookmark.reading_time));
        }
        if bookmark.word_count > 0 {
            meta_parts.push(format!("{} words", bookmark.word_count));
        }
        if !bookmark.labels.is_empty() {
            meta_parts.push(
                bookmark
                    .labels
                    .iter()
                    .map(|label| format!("#{label}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        if live_progress > 0 {
            meta_parts.push(format!("{live_progress}% read"));
        }
        reader_meta = super::truncate_width(
            &meta_parts.join("  ·  "),
            area.width.saturating_sub(4) as usize,
        );

        let header = Paragraph::new(vec![
            Line::from(title_spans),
            Line::from(Span::styled(
                reader_meta.clone(),
                Style::new().fg(theme::dim()),
            )),
            Line::from(Span::styled(
                super::truncate_width(
                    &bookmark.url,
                    area.width.saturating_sub(4) as usize,
                ),
                Style::new().fg(theme::faint()),
            )),
        ]);
        frame.render_widget(header, header_area);
        frame.render_widget(block, body_area);

        if reader.loading {
            centered(
                frame,
                inner,
                &[Line::from(Span::styled(
                    format!("{}  Loading article…", super::spinner(app.tick)),
                    Style::new().fg(theme::dim()),
                ))],
            );
        } else if let Some(error) = &reader.error {
            centered(
                frame,
                inner,
                &[
                    Line::from(Span::styled(
                        "Could not load the article",
                        Style::new()
                            .fg(theme::red())
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        super::truncate_width(
                            error,
                            inner.width.saturating_sub(4) as usize,
                        ),
                        Style::new().fg(theme::dim()),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Press r to retry · Esc to go back",
                        Style::new().fg(theme::faint()),
                    )),
                ],
            );
        } else if reader.lines.is_empty() {
            centered(
                frame,
                inner,
                &[Line::from(Span::styled(
                    "This bookmark has no article text.",
                    Style::new().fg(theme::dim()),
                ))],
            );
        } else if let Some(speed) = &reader.speed {
            draw_speed(frame, inner, speed, content_width as usize);
        } else {
            let mut annotated: std::collections::HashMap<
                usize,
                Vec<(usize, usize, ratatui::style::Color)>,
            > = std::collections::HashMap::new();
            for annotation in &reader.annotations {
                let color = super::annotation_color(annotation.color());
                match reader.annotation_ranges(annotation) {
                    Some(ranges) => {
                        for (block, from, to) in ranges {
                            annotated
                                .entry(block)
                                .or_default()
                                .push((from, to, color));
                        }
                    }
                    None => {
                        if let Some(block) = reader.annotation_block(annotation)
                        {
                            annotated.entry(block).or_default().push((
                                0,
                                usize::MAX,
                                color,
                            ));
                        }
                    }
                }
            }
            let mut inline_notes: Vec<(usize, usize, String)> = Vec::new();
            if !selecting {
                for annotation in &reader.annotations {
                    let note = annotation.note.trim();
                    if note.is_empty() {
                        continue;
                    }
                    let text = format!(
                        " [{}]",
                        super::truncate_width(&note.replace('\n', " "), 40)
                    );
                    match reader.annotation_ranges(annotation) {
                        Some(ranges) => {
                            if let Some((block, _, end)) = ranges.last() {
                                inline_notes.push((*block, *end, text));
                            }
                        }
                        None => {
                            if let Some(block) =
                                reader.annotation_block(annotation)
                            {
                                inline_notes.push((block, usize::MAX, text));
                            }
                        }
                    }
                }
            }
            let bounds = reader.selected_bounds();
            let selection_style = Style::new()
                .bg(theme::bg_selected())
                .fg(theme::text())
                .add_modifier(Modifier::BOLD);
            let now = Instant::now();
            let flash_style = reader
                .flash
                .filter(|flash| now < flash.until)
                .map(|flash| flash_style(now, flash.until));
            let visible: Vec<Line> = reader
                .lines
                .iter()
                .enumerate()
                .skip(reader.scroll)
                .take(reader.viewport)
                .map(|(index, line)| {
                    if let Some(cells) = &line.image {
                        return image_line(cells, content_width as usize);
                    }
                    let base = block_style(line.kind);
                    let mut styles = Vec::new();
                    if let (Some(range), Some(block)) =
                        (&line.range, line.block)
                    {
                        if let Some(ranges) = annotated.get(&block) {
                            for (from, to, color) in ranges {
                                let start = (*from).max(range.start);
                                let end = (*to).min(range.end);
                                if start < end {
                                    styles.push((
                                        start - range.start,
                                        end - range.start,
                                        Style::new().fg(*color),
                                    ));
                                }
                            }
                        }
                    }
                    if let Some((from, to)) =
                        selection_span(bounds, index, line)
                    {
                        styles.push((from, to, selection_style));
                    }
                    if let (Some(flash), Some(style)) =
                        (reader.flash, flash_style)
                    {
                        if index == flash.line {
                            let indent = indent_chars(&line.text, line.indent);
                            let from = flash.start.saturating_sub(indent);
                            let to = flash.end.saturating_sub(indent);
                            if from < to {
                                styles.push((from, to, style));
                            }
                        }
                    }
                    let mut notes = Vec::new();
                    if let (Some(range), Some(block)) =
                        (&line.range, line.block)
                    {
                        let last_line = reader
                            .lines
                            .get(index + 1)
                            .and_then(|next| next.block)
                            != Some(block);
                        for (note_block, end, text) in &inline_notes {
                            if *note_block != block {
                                continue;
                            }
                            if *end == usize::MAX {
                                if last_line {
                                    notes.push((
                                        range.end - range.start,
                                        text.clone(),
                                    ));
                                }
                            } else if *end > range.start && *end <= range.end {
                                notes.push((*end - range.start, text.clone()));
                            }
                        }
                    }
                    if styles.is_empty() && notes.is_empty() {
                        article_line(
                            &line.text,
                            base,
                            reader.search_query.as_deref(),
                        )
                    } else {
                        styled_line(line, base, &styles, &notes)
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(visible), content_area);
            for (position, protocol) in reader.visible_graphics() {
                frame.render_widget(
                    SlicedImage::new(protocol, position),
                    content_area,
                );
            }

            if let Some(selection) = reader.selection {
                let position = selection.cursor;
                if position.line >= reader.scroll
                    && position.line < reader.scroll + reader.viewport
                {
                    let line = &reader.lines[position.line];
                    let indent = indent_chars(&line.text, line.indent);
                    let length = line
                        .range
                        .as_ref()
                        .map(|range| range.end - range.start)
                        .unwrap_or(0);
                    let used: usize = line
                        .text
                        .chars()
                        .skip(indent)
                        .take(position.offset.min(length))
                        .map(|ch| ch.width().unwrap_or(0))
                        .sum();
                    let column = (line.indent + used)
                        .min(content_area.width.saturating_sub(1) as usize);
                    let x = content_area.x + column as u16;
                    let y =
                        content_area.y + (position.line - reader.scroll) as u16;
                    frame.set_cursor_position((x, y));
                }
            }

            let max = reader.max_scroll().saturating_add(1);
            if max > 1 {
                let mut state = ScrollbarState::new(max)
                    .position(reader.scroll.min(max.saturating_sub(1)));
                frame.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .style(Style::new().fg(theme::border())),
                    body_area,
                    &mut state,
                );
            }
        }

        reader_progress = match &reader.speed {
            Some(speed) => {
                let state = if speed.playing() {
                    ""
                } else if speed.index() + 1 >= speed.total() {
                    " · finished"
                } else {
                    " · paused"
                };
                format!(
                    "word {} of {} · {} wpm{state}",
                    speed.index() + 1,
                    speed.total(),
                    speed.wpm()
                )
            }
            None => format!(
                "{live_progress}% read · line {} of {}",
                (reader.scroll + 1).min(reader.lines.len().max(1)),
                reader.lines.len()
            ),
        };
        reader_search = reader.search_query.as_ref().map(|query| {
            format!(
                "search “{}” · {} matches ",
                super::truncate_width(query, 24),
                reader.search_match_lines().len()
            )
        });
    }

    let [footer_top, footer_bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)])
            .areas(footer_area);

    let hints = if selecting {
        "  h/j/k/l move · w/b/e words · W/B/E WORDS · 0/^/$ line · {/} paragraph · Enter/Space select · Esc cancel"
    } else if speed_reading {
        "  Space pause/resume · h/l word · +/- words per minute · s or Esc back to article"
    } else {
        "  j/k scroll · Space paragraph · Ctrl+d/u half · g/G top/bottom · v highlight · H annotations · s speed read · m read · f favorite · a archive · t labels · i images · o browser · Esc back"
    };
    let top = if let Some(toast) = super::toast_line(app) {
        toast
    } else {
        Line::from(Span::styled(
            super::truncate_width(hints, area.width as usize),
            Style::new().fg(theme::faint()),
        ))
    };
    frame.render_widget(
        Paragraph::new(top).style(Style::new().bg(theme::bg_bar())),
        footer_top,
    );

    let right = reader_search.unwrap_or_else(|| {
        format!(
            "{} ",
            super::truncate_width(
                &reader_meta,
                (area.width as usize / 2).max(10)
            )
        )
    });
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Min(18),
        Constraint::Length(area.width / 2),
    ])
    .areas(footer_bottom);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  {reader_progress}"),
            Style::new().fg(theme::dim()),
        )))
        .style(Style::new().bg(theme::bg_bar())),
        left_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            right,
            Style::new().fg(theme::faint()),
        )))
        .alignment(Alignment::Right)
        .style(Style::new().bg(theme::bg_bar())),
        right_area,
    );
}

/// Style of the word marked after speed reading. It fades from the accent
/// color to the article colors over [`crate::app::FLASH_DURATION`].
fn flash_style(now: Instant, until: Instant) -> Style {
    let total = crate::app::FLASH_DURATION.as_secs_f32().max(0.001);
    let remaining = (until - now).as_secs_f32().clamp(0.0, total);
    let progress = 1.0 - remaining / total;
    let background = smoothstep(progress);
    let foreground = smoothstep((progress / 0.35).min(1.0));
    Style::new()
        .bg(blend(theme::accent(), theme::bg_base(), background))
        .fg(blend(theme::bg_base(), theme::text(), foreground))
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn blend(from: Color, to: Color, t: f32) -> Color {
    let (r1, g1, b1) = rgb(from);
    let (r2, g2, b2) = rgb(to);
    let mix = |a: u8, b: u8| {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
    };
    Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
}

fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

fn block_style(kind: Option<BlockKind>) -> Style {
    match kind {
        None => Style::new(),
        Some(BlockKind::Paragraph | BlockKind::ListItem) => {
            Style::new().fg(theme::text())
        }
        Some(BlockKind::Heading1) => Style::new()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
        Some(BlockKind::Heading2) => Style::new()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
        Some(BlockKind::Heading3) => {
            Style::new().fg(theme::text()).add_modifier(Modifier::BOLD)
        }
        Some(BlockKind::Quote) => {
            Style::new().fg(theme::dim()).add_modifier(Modifier::ITALIC)
        }
        Some(BlockKind::Code) => Style::new().fg(theme::green()),
        Some(BlockKind::Caption) => {
            Style::new().fg(theme::dim()).add_modifier(Modifier::ITALIC)
        }
        Some(BlockKind::Rule) => Style::new().fg(theme::border()),
        Some(BlockKind::Image) => Style::new()
            .fg(theme::faint())
            .add_modifier(Modifier::ITALIC),
    }
}

fn article_line(text: &str, base: Style, query: Option<&str>) -> Line<'static> {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return Line::from(Span::styled(text.to_string(), base));
    };
    let lower = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    if !lower.contains(&needle) {
        return Line::from(Span::styled(text.to_string(), base));
    }

    let highlight = Style::new()
        .fg(theme::bg_base())
        .bg(theme::accent_alt())
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut start = 0;
    while let Some(relative) = lower[start..].find(&needle) {
        let match_start = start + relative;
        let match_end = match_start + needle.len();
        if match_start > start {
            spans
                .push(Span::styled(text[start..match_start].to_string(), base));
        }
        spans.push(Span::styled(
            text[match_start..match_end].to_string(),
            highlight,
        ));
        start = match_end;
        if start >= text.len() {
            break;
        }
    }
    if start < text.len() {
        spans.push(Span::styled(text[start..].to_string(), base));
    }
    Line::from(spans)
}

fn image_line(cells: &[ImageCell], width: usize) -> Line<'static> {
    let (bg_r, bg_g, bg_b) = match theme::bg_base() {
        ratatui::style::Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    };
    let blend = |pixel: [u8; 4]| {
        let alpha = pixel[3] as u16;
        let inverse = 255 - alpha;
        let channel = |value: u8, background: u8| {
            ((value as u16 * alpha + background as u16 * inverse) / 255) as u8
        };
        ratatui::style::Color::Rgb(
            channel(pixel[0], bg_r),
            channel(pixel[1], bg_g),
            channel(pixel[2], bg_b),
        )
    };

    let padding = width.saturating_sub(cells.len()) / 2;
    let mut spans = Vec::with_capacity(cells.len() + 1);
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    for cell in cells {
        spans.push(Span::styled(
            "▀",
            Style::new().fg(blend(cell.top)).bg(blend(cell.bottom)),
        ));
    }
    Line::from(spans)
}

fn indent_chars(text: &str, indent: usize) -> usize {
    if indent == 0 {
        return 0;
    }
    let mut width = 0;
    for (index, ch) in text.chars().enumerate() {
        width += ch.width().unwrap_or(0);
        if width >= indent {
            return index + 1;
        }
    }
    0
}

fn selection_span(
    bounds: Option<(crate::app::TextPos, crate::app::TextPos)>,
    line: usize,
    wrapped: &crate::article::WrappedLine,
) -> Option<(usize, usize)> {
    let (start, end) = bounds?;
    if line < start.line || line > end.line {
        return None;
    }
    let length = wrapped
        .range
        .as_ref()
        .map(|range| range.end - range.start)?;
    let from = if line == start.line { start.offset } else { 0 };
    let to = if line == end.line { end.offset } else { length };
    let from = from.min(length);
    let to = to.min(length);
    if from >= to {
        return None;
    }
    Some((from, to))
}

fn styled_line(
    wrapped: &crate::article::WrappedLine,
    base: Style,
    ranges: &[(usize, usize, Style)],
    notes: &[(usize, String)],
) -> Line<'static> {
    let indent = indent_chars(&wrapped.text, wrapped.indent);
    let chars: Vec<char> = wrapped.text.chars().collect();
    let content = &chars[indent.min(chars.len())..];
    // Notes only dim the text: no background of their own.
    let note_style = Style::new().fg(theme::dim());

    let mut boundaries = vec![0usize, content.len()];
    for (from, to, _) in ranges {
        boundaries.push((*from).min(content.len()));
        boundaries.push((*to).min(content.len()));
    }
    for (position, _) in notes {
        boundaries.push((*position).min(content.len()));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut spans = Vec::new();
    if indent > 0 && indent <= chars.len() {
        spans.push(Span::styled(
            chars[..indent].iter().collect::<String>(),
            base,
        ));
    }

    let mut cursor = 0;
    for boundary in boundaries {
        if boundary > cursor {
            let mut style = base;
            for (from, to, range_style) in ranges {
                if *from <= cursor && *to >= boundary {
                    style = *range_style;
                }
            }
            spans.push(Span::styled(
                content[cursor..boundary].iter().collect::<String>(),
                style,
            ));
        }
        cursor = boundary;
        for (position, text) in notes {
            if *position == boundary {
                spans.push(Span::styled(text.clone(), note_style));
            }
        }
    }
    Line::from(spans)
}

fn draw_speed(
    frame: &mut Frame,
    area: Rect,
    speed: &SpeedReader,
    width: usize,
) {
    let word = super::truncate_width(speed.current(), width.max(1));
    let status = if speed.playing() {
        format!("{} wpm", speed.wpm())
    } else if speed.index() + 1 >= speed.total() {
        format!("{} wpm · finished — Space to restart", speed.wpm())
    } else {
        format!("{} wpm · paused — Space to resume", speed.wpm())
    };
    let lines = vec![
        Line::from(Span::styled(
            word,
            Style::new()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(status, Style::new().fg(theme::faint()))),
    ];
    centered(frame, area, &lines);
}

fn centered(frame: &mut Frame, area: Rect, lines: &[Line<'static>]) {
    let height = lines.len() as u16;
    let y = area.y + area.height.saturating_sub(height) / 2;
    frame.render_widget(
        Paragraph::new(lines.to_vec()).alignment(Alignment::Center),
        Rect::new(area.x, y, area.width, height.min(area.height)),
    );
}
