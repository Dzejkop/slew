//! Rendering: map, robot panel, log, prompt, and footer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;
use crate::world::{Cell, World};

pub(crate) fn ui(frame: &mut Frame, app: &App, world: &World) {
    let [body, input_area, footer] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [top, log_area] =
        Layout::vertical([Constraint::Fill(3), Constraint::Length(10)]).areas(body);
    let [map_area, side_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(32)]).areas(top);

    draw_map(frame, map_area, app, world);
    draw_side(frame, side_area, app, world);
    draw_log(frame, log_area, app);
    draw_input(frame, input_area, app);
    draw_footer(frame, footer, app);
}

fn draw_map(frame: &mut Frame, area: Rect, app: &App, world: &World) {
    let block = Block::bordered().title(" map ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (cx, cy) = app.camera(inner, world);
    let mut lines = Vec::new();
    for row in 0..inner.height {
        let y = cy + row as i32;
        let mut spans = Vec::new();
        for col in 0..inner.width {
            let x = cx + col as i32;
            if x < 0 || y < 0 || x >= world.w || y >= world.h {
                spans.push(Span::raw(" "));
                continue;
            }
            if let Some((ri, r)) = world
                .robots
                .iter()
                .enumerate()
                .find(|(_, r)| r.x == x && r.y == y)
            {
                let style = if ri == app.selected {
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::Green)
                };
                spans.push(Span::styled(r.facing.glyph().to_string(), style));
                continue;
            }
            let cell = world.cells[(y * world.w + x) as usize];
            let span = match cell {
                Cell::Wall => Span::styled("█", Style::new().fg(Color::DarkGray)),
                Cell::Ore(n) if n > 0 => Span::styled("◆", Style::new().fg(Color::Yellow)),
                _ => Span::styled("·", Style::new().fg(Color::Indexed(238))),
            };
            spans.push(span);
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_side(frame: &mut Frame, area: Rect, app: &App, world: &World) {
    let mut lines = Vec::new();
    for (i, r) in world.robots.iter().enumerate() {
        let rob = &app.robots[i];
        let marker = if i == app.selected { "▸" } else { " " };
        let status = rob.status();
        let header = format!("{marker} R{i}  ({},{}) {}", r.x, r.y, r.facing.name());
        let detail = format!(
            "    {status}  ore:{}  line:{}",
            r.carried,
            rob.line.map_or("-".to_string(), |l| l.to_string())
        );
        let style = if rob.error.is_some() {
            Style::new().fg(Color::Red)
        } else if i == app.selected {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new()
        };
        lines.push(Line::from(Span::styled(
            header,
            style.add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(detail, style)));
        if let Some(e) = &rob.error {
            lines.push(Line::from(Span::styled(
                format!("    ! {e}"),
                Style::new().fg(Color::Red),
            )));
        }
    }
    if app.follow {
        lines.push(Line::from(Span::styled(
            " (following)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" robots ")),
        area,
    );
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let world = app.world.borrow();
    let log = world.log.borrow();
    let height = area.height.saturating_sub(2) as usize;
    let skip = log.len().saturating_sub(height);
    let body: Vec<Line> = log[skip..].iter().map(|s| Line::raw(s.clone())).collect();
    frame.render_widget(
        Paragraph::new(body).block(Block::bordered().title(" log ")),
        area,
    );
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let i = app.selected;
    let line = if app.input.is_empty() {
        Line::from(Span::styled(
            "e.g.  robot.forward(); robot.wait()",
            Style::new().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::raw(app.input.clone()),
            Span::styled("▏", Style::new().fg(Color::Yellow)),
        ])
    };
    let prompt_waiting = app
        .robots
        .get(i)
        .and_then(|r| r.prompt.as_ref())
        .is_some_and(|p| p.wait.is_some());
    let title = if prompt_waiting {
        format!(" R{i} prompt · waiting (c cancels) ")
    } else if app.editing {
        format!(" R{i} prompt · typing ")
    } else {
        format!(" R{i} prompt · Tab to type ")
    };
    let border = if app.editing {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(line).block(Block::bordered().title(title).border_style(border)),
        area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let hint = if app.editing {
        "typing · Tab: controls · Enter run · Backspace edit · Esc clear".to_string()
    } else {
        format!(
            "j/k select · f follow:{} · b boot · r reboot · x shutdown · n new · e edit · c cancel · space {} · +/- speed ×{:.2} · q quit",
            if app.follow { "on" } else { "off" },
            if app.paused { "resume" } else { "pause" },
            app.speed,
        )
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        area,
    );
}
