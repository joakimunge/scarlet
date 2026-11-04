//! Drawing. Each component owns its view state and renders itself; this module
//! lays them out.

pub mod connect;
pub mod editor;
pub mod fuzzy;
pub mod grid;
pub mod help;
pub mod input;
pub mod list;
pub mod menu;
pub mod palette;
pub mod row;
pub mod text;
pub mod theme;
pub mod tree;
pub mod value;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType},
};

use crate::{
    app::{App, Focus, Mode, Overlay, RunState, ToastKind, Workspace},
    sql::{self, TokenKind},
};

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 40 || area.height < 8 {
        let message = text::fit("Make the window a little bigger", area.width as usize);
        let rect = text::centered(area, text::measure(&message, 99) as u16, 1);
        frame.buffer_mut().set_string(rect.x, rect.y, message, theme::muted());
        return;
    }
    let [body, footer] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let spinner = app.spinner();

    if let Some(workspace) = &mut app.workspace {
        render_workspace(frame, body, workspace, spinner);
    } else if let Some(connecting) = &app.connecting
        && app.overlay.is_none()
    {
        let target = &connecting.target;
        let message = format!("{spinner} Connecting to {} on {}", target.database(), target.address());
        let message = text::fit(&message, body.width as usize - 4);
        let rect = text::centered(body, text::measure(&message, 999) as u16, 1);
        frame.buffer_mut().set_string(rect.x, rect.y, message, theme::muted());
    }

    if app.overlay.is_some() && app.workspace.is_some() {
        // Quiet the workspace so the dialog stands out.
        frame.buffer_mut().set_style(body, Modifier::DIM);
    }
    let connecting = app.connecting.as_ref().map(|c| &c.target);
    match &mut app.overlay {
        Some(Overlay::Connect(picker)) => {
            let activity = connect::Activity { discovering: app.discovering, connecting, spinner };
            picker.render(frame, body, activity);
        }
        Some(Overlay::Password(prompt)) => prompt.render(frame, body, connecting.is_some(), spinner),
        Some(Overlay::Palette(palette)) => palette.render(frame, body),
        Some(Overlay::Value(view)) => view.render(frame, body),
        Some(Overlay::Row(view)) => view.render(frame, body),
        Some(Overlay::Menu(menu)) => menu.render(frame, body),
        Some(Overlay::Help) => help::render(frame, body, app.vim.as_ref()),
        None => {}
    }

    render_footer(frame, footer, app, spinner);
}

fn render_workspace(frame: &mut Frame, area: Rect, ws: &mut Workspace, spinner: &str) {
    let longest = ws
        .tree
        .schemas
        .iter()
        .flat_map(|s| s.relations.iter().map(|r| text::measure(&r.name, 60)).chain([text::measure(&s.name, 60)]))
        .max()
        .unwrap_or(20);
    let sidebar = (longest as u16 + 12).clamp(24, 40).min(area.width * 2 / 5);
    let [side, main] = Layout::horizontal([Constraint::Length(sidebar), Constraint::Fill(1)]).areas(area);

    let busy = (ws.catalog_pending || !ws.tree.loaded).then_some(spinner);
    let title = ws.target.database().to_owned();
    ws.tree.render(frame, side, ws.focus == Focus::Tree, &title, busy);
    if ws.mode == Mode::Query {
        return render_query(frame, main, ws, spinner);
    }
    match &mut ws.table {
        Some(grid) => grid.render(frame, main, ws.focus == Focus::Grid, spinner),
        None => frame.render_widget(empty_block(" ", false), main),
    }
}

/// The editor above its result.
fn render_query(frame: &mut Frame, area: Rect, ws: &mut Workspace, spinner: &str) {
    let editor_height = (area.height * 2 / 5).clamp(6.min(area.height), area.height.saturating_sub(6).max(6));
    let [top, bottom] = Layout::vertical([Constraint::Length(editor_height), Constraint::Fill(1)]).areas(area);

    // The result first: the editor's completion popup may overlap it.
    let focused = ws.focus == Focus::Grid;
    let message = match &ws.run {
        RunState::Running { .. } => Some((format!("{spinner} Running"), theme::muted())),
        RunState::Failed(error) => Some((error.clone(), theme::error())),
        RunState::Done(message) => Some((message.clone(), Style::new())),
        RunState::NeedsWrite(_) => Some((
            "This statement changes data, and Scarlet is read-only until you say otherwise. \
             Press ctrl+r again to run it once with writes allowed, or esc to leave it."
                .to_owned(),
            Style::new(),
        )),
        RunState::Idle => None,
    };
    match (message, &mut ws.result) {
        (None, Some(grid)) => grid.render(frame, bottom, focused, spinner),
        (message, _) => {
            let block = empty_block("Result", focused);
            let inner = block.inner(bottom);
            frame.render_widget(block, bottom);
            let (message, style) = message.unwrap_or_else(|| ("Results appear here.".into(), theme::muted()));
            for (i, line) in text::wrap(&message, inner.width.saturating_sub(2) as usize).iter().enumerate() {
                if i as u16 >= inner.height {
                    break;
                }
                frame.buffer_mut().set_string(inner.x + 1, inner.y + i as u16, line, style);
            }
        }
    }

    let Some(editor) = &mut ws.editor else { return };
    let text = editor.text();
    let statement = sql::statement_at(&text, editor.cursor());
    let styles: Vec<_> = sql::tokenize(&text)
        .into_iter()
        .filter_map(|token| {
            let style = match token.kind {
                TokenKind::Keyword => theme::keyword(),
                TokenKind::String => theme::string(),
                TokenKind::Number => theme::number(),
                TokenKind::Comment => theme::comment(),
                _ => return None,
            };
            Some((token.start..token.end, style))
        })
        .collect();
    editor.render(frame, top, ws.focus == Focus::Editor, &styles, statement);
}

fn empty_block(title: &str, focused: bool) -> Block<'static> {
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(theme::border(focused));
    if title.trim().is_empty() {
        block
    } else {
        block.title(Line::from(vec![Span::raw(" "), Span::styled(title.to_owned(), theme::strong()), Span::raw(" ")]))
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, spinner: &str) {
    let mut right: Vec<Span> = Vec::new();
    if let Some(toast) = &app.toast {
        let style = match toast.kind {
            ToastKind::Info => theme::accent(),
            ToastKind::Error => theme::error(),
        };
        right.push(Span::styled(toast.text.clone(), style));
    } else if let (Some(connecting), Some(_)) = (&app.connecting, &app.workspace) {
        right.push(Span::styled(format!("{spinner} Connecting to {}", connecting.target.database()), theme::muted()));
    } else if let Some(ws) = &app.workspace {
        right.push(Span::styled(ws.target.address(), theme::muted()));
    }
    let right_width = right.iter().map(|s| text::measure(&s.content, 999)).sum::<usize>().min(area.width as usize / 2);

    let mut left: Vec<Span> = vec![Span::raw(" ")];
    let mut used = 1;
    let room = (area.width as usize).saturating_sub(right_width + 2);
    for (key, label) in app.hints() {
        let width = text::measure(&key, 99) + 1 + text::measure(&label, 99) + 3;
        if used + width > room {
            break;
        }
        left.push(Span::styled(key, theme::accent()));
        left.push(Span::styled(format!(" {label}   "), theme::muted()));
        used += width;
    }
    let buf = frame.buffer_mut();
    buf.set_line(area.x, area.y, &Line::from(left), area.width);
    if !right.is_empty() {
        let line = Line::from(right);
        let x = area.right().saturating_sub(right_width as u16 + 1);
        buf.set_line(x, area.y, &line, right_width as u16);
    }
}
