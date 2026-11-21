//! Drawing: the conversation on the left, the task panel (current task, recent activity, pending
//! approval) on the right, the message editor and a status line at the bottom. Narrow terminals
//! drop the panel and show the task in the status line instead.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthChar;

use super::app::{App, Entry, Level, Speaker, one_line};
use nypi_core::worker::ToolState;

/// Below this width the task panel is hidden.
const PANEL_MIN_WIDTH: u16 = 90;
const INPUT_MAX_ROWS: usize = 8;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let input_width = area.width.saturating_sub(2) as usize;
    let (input_rows, cursor) = app.input.layout(input_width.saturating_sub(2));
    let input_height = input_rows.len().clamp(1, INPUT_MAX_ROWS) as u16 + 2;
    let [main, editor, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(input_height), Constraint::Length(1)]).areas(area);

    if main.width >= PANEL_MIN_WIDTH {
        let [conversation, panel] =
            Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)]).areas(main);
        draw_conversation(frame, app, conversation);
        draw_panel(frame, app, panel);
    } else {
        draw_conversation(frame, app, main);
    }
    draw_editor(frame, app, editor, &input_rows, cursor);
    draw_status(frame, app, status, main.width < PANEL_MIN_WIDTH);
}

fn draw_conversation(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let height = area.height.saturating_sub(2) as usize;
    let lines = conversation_lines(app, width);
    let max_scroll = lines.len().saturating_sub(height);
    let scroll = app.scroll.min(max_scroll);
    let start = lines.len().saturating_sub(height + scroll);
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    let title =
        if scroll > 0 { format!(" Conversation · ↑{scroll} (PgDn to follow) ") } else { " Conversation ".into() };
    frame.render_widget(Paragraph::new(visible).block(Block::bordered().title(title)), area);
}

/// Every entry as wrapped, styled lines.
pub fn conversation_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let dim = Style::new().fg(Color::DarkGray);
    for entry in &app.entries {
        match entry {
            Entry::User(text) => {
                lines.push(Line::from(Span::styled("you", Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))));
                push_wrapped(&mut lines, text, width, Style::new());
                lines.push(Line::default());
            }
            Entry::Reply { speaker, text, thinking } => {
                if text.trim().is_empty() && thinking.trim().is_empty() {
                    continue;
                }
                let (label, color) = match speaker {
                    Speaker::Lucia => (app.agent_name.clone(), Color::Magenta),
                    Speaker::Task => (format!("{} · task", app.agent_name), Color::Cyan),
                };
                lines.push(Line::from(Span::styled(label, Style::new().fg(color).add_modifier(Modifier::BOLD))));
                if !thinking.trim().is_empty() {
                    push_wrapped(&mut lines, thinking.trim(), width, dim.add_modifier(Modifier::ITALIC));
                }
                push_wrapped(&mut lines, text.trim(), width, Style::new());
                lines.push(Line::default());
            }
            Entry::Tool { line, state, .. } => {
                let (mark, style) = match state {
                    ToolState::Running => ("◌", dim),
                    ToolState::Ok => ("·", dim),
                    ToolState::Failed => ("✗", Style::new().fg(Color::Red)),
                };
                let text = format!("  {mark} {}", one_line(line, width.saturating_sub(6).max(10)));
                lines.push(Line::from(Span::styled(text, style)));
            }
            Entry::Notice { text, level } => {
                let style = match level {
                    Level::Info => dim,
                    Level::Success => Style::new().fg(Color::Green),
                    Level::Warn => Style::new().fg(Color::Yellow),
                    Level::Error => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                };
                push_wrapped(&mut lines, &format!("── {text}"), width, style);
                lines.push(Line::default());
            }
        }
    }
    lines
}

fn push_wrapped(lines: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for row in wrap(text, width) {
        lines.push(Line::from(Span::styled(row, style)));
    }
}

/// Greedy word wrap by display width; words wider than a line are split.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        let mut row_width = 0;
        for word in paragraph.split_inclusive(' ') {
            // A trailing space may hang past the edge; only the word itself has to fit.
            let word_width: usize = word.trim_end().chars().map(|c| c.width().unwrap_or(0)).sum();
            if row_width + word_width > width && !row.is_empty() && word_width <= width {
                rows.push(std::mem::take(&mut row).trim_end().to_string());
                row_width = 0;
            }
            for c in word.chars() {
                let w = c.width().unwrap_or(0);
                if row_width + w > width && !row.is_empty() {
                    rows.push(std::mem::take(&mut row).trim_end().to_string());
                    row_width = 0;
                    if c == ' ' {
                        continue;
                    }
                }
                row.push(c);
                row_width += w;
            }
        }
        rows.push(row.trim_end().to_string());
    }
    rows
}

fn draw_panel(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let approval_height = if app.approval.is_some() { 7 } else { 0 };
    let task_lines = task_lines(app, width);
    let task_height = (task_lines.len() as u16 + 2).min(area.height / 2);
    let [task, activity, approval] =
        Layout::vertical([Constraint::Length(task_height), Constraint::Min(3), Constraint::Length(approval_height)])
            .areas(area);
    frame.render_widget(Paragraph::new(task_lines).block(Block::bordered().title(" Task ")), task);

    let height = activity.height.saturating_sub(2) as usize;
    let rows: Vec<Line> = app
        .activity
        .iter()
        .rev()
        .take(height)
        .rev()
        .map(|line| Line::from(Span::styled(one_line(line, width), Style::new().fg(Color::DarkGray))))
        .collect();
    frame.render_widget(Paragraph::new(rows).block(Block::bordered().title(" Activity ")), activity);

    if let Some(view) = &app.approval {
        let yellow = Style::new().fg(Color::Yellow);
        let more = if view.waiting > 1 { format!(" · {} waiting", view.waiting) } else { String::new() };
        let lines = vec![
            Line::from(Span::styled(format!("{}{more}", view.tool), yellow.add_modifier(Modifier::BOLD))),
            Line::from(one_line(&view.summary, width * 2)),
            Line::from(Span::styled(
                one_line(&format!("Why: {}", view.reason), width * 2),
                Style::new().fg(Color::DarkGray),
            )),
            Line::from(Span::styled("y = approve · Enter = deny · or type why not", yellow)),
        ];
        let block = Block::bordered().title(" Approval needed ").border_style(yellow);
        frame.render_widget(Paragraph::new(lines).block(block), approval);
    }
}

fn task_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let Some(task) = &app.task else {
        let idle = if app.replying { "Idle · chat is replying" } else { "Idle" };
        return vec![Line::from(Span::styled(idle, dim))];
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("Running · {} s", task.started.elapsed().as_secs()),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))];
    for row in wrap(&one_line(&task.task, width * 3), width).into_iter().take(3) {
        lines.push(Line::from(row));
    }
    for tool in task.tools.iter().rev().take(4).rev() {
        let (mark, style) = match tool.state {
            ToolState::Running => ("◌", Style::new().fg(Color::Cyan)),
            ToolState::Ok => ("·", dim),
            ToolState::Failed => ("✗", Style::new().fg(Color::Red)),
        };
        let text = format!("{mark} {} {}", tool.tool, tool.summary);
        lines.push(Line::from(Span::styled(one_line(&text, width.saturating_sub(1)), style)));
    }
    lines
}

fn draw_editor(frame: &mut Frame, app: &App, area: Rect, rows: &[String], cursor: (usize, usize)) {
    let visible = area.height.saturating_sub(2) as usize;
    let first = (cursor.1 + 1).saturating_sub(visible);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(index, row)| Line::from(format!("{}{row}", if index == 0 { "› " } else { "  " })))
        .collect();
    let (title, style) = if app.approval.is_some() {
        (" Approve? y = yes · Enter = no · or type why not ", Style::new().fg(Color::Yellow))
    } else if app.busy() {
        (" Message · Enter sends (steers the work) · Alt+Enter newline ", Style::new())
    } else {
        (" Message · Enter sends · Alt+Enter newline ", Style::new())
    };
    let mut block = Block::bordered().title(title).border_style(style);
    if let Some(hint) = &app.hint {
        block = block.title_bottom(Line::from(format!(" {hint} ")).style(Style::new().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
    let x = area.x + 1 + 2 + cursor.0 as u16;
    let y = area.y + 1 + (cursor.1 - first) as u16;
    frame.set_cursor_position((x.min(area.right().saturating_sub(2)), y));
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect, narrow: bool) {
    let models = match &app.chat_model {
        Some(chat) => format!("chat {chat} · worker {}", app.worker_model),
        None => app.worker_model.clone(),
    };
    let task = match (&app.task, narrow) {
        (Some(task), true) => format!(" · task {} s", task.started.elapsed().as_secs()),
        _ => String::new(),
    };
    let keys = if app.busy() { "Ctrl-C abort" } else { "Ctrl-C quit" };
    let at = app.focus.as_ref().map(|focus| format!(" · at {focus}")).unwrap_or_default();
    let text = format!(" NyPi {}{at} · {models}{task} · PgUp/PgDn scroll · {keys} · /help", app.agent_name);
    let style = Style::new().fg(Color::Black).bg(Color::Gray);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(one_line(&text, area.width as usize), style))), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn wraps_words_and_wide_characters() {
        assert_eq!(wrap("the quick brown fox", 9), ["the quick", "brown fox"]);
        assert_eq!(wrap("一二三四五", 4), ["一二", "三四", "五"]);
        assert_eq!(wrap("a\n\nb", 5), ["a", "", "b"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    fn screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn draws_conversation_panel_and_approval() {
        let mut app = App::new("Lucia", Some("fast".into()), "strong", false);
        app.user("What changed today?");
        app.notice(Level::Success, "Done · 12 s");
        app.activity.push_back("16:39:15 runtime read ok 28 ms".into());
        app.approval = Some(super::super::app::ApprovalView {
            id: "approval-1".into(),
            tool: "bash".into(),
            summary: "git push origin main".into(),
            reason: "pushing to a remote".into(),
            waiting: 1,
        });
        let wide = screen(&app, 120, 30);
        for expected in [
            "Conversation",
            "What changed today?",
            "Done · 12 s",
            "Activity",
            "read ok",
            "Approval needed",
            "git push origin main",
            "Approve? y = yes",
        ] {
            assert!(wide.contains(expected), "missing {expected}:\n{wide}");
        }
        let narrow = screen(&app, 60, 20);
        assert!(!narrow.contains("Activity") && narrow.contains("What changed today?"), "{narrow}");
    }
}
