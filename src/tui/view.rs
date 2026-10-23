//! Rendering only: reads `App`, never performs I/O.

use super::actions::{log_lines, pretty};
use super::app::{App, Focus, Level, Modal, Screen, item_key};
use super::form::{FieldKind, Form, Picker};
use super::kinds::{self, Kind, s};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use std::time::Duration;

const MIN_W: u16 = 60;
const MIN_H: u16 = 15;

fn sel() -> Style {
    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        f.render_widget(
            Paragraph::new(format!("Terminal too small ({}x{}); need at least {MIN_W}x{MIN_H}. q quits.", area.width, area.height))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let [header, body, footer] = Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(2)]).areas(area);
    draw_header(f, app, header);
    let [nav, main] = Layout::horizontal([Constraint::Length(if area.width >= 100 { 22 } else { 0 }), Constraint::Min(20)]).areas(body);
    if nav.width > 0 {
        draw_nav(f, app, nav);
    }
    match &app.screen {
        Screen::Dashboard => draw_text_screen(f, "Dashboard  [r: refresh]", &app.dashboard, main),
        Screen::Help => draw_text_screen(f, "Help", &help_lines(), main),
        Screen::List(_) => draw_list(f, app, main),
    }
    draw_footer(f, app, footer);
    if let Some(m) = &app.modal {
        draw_modal(f, m, app, area);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let profile = app.profile.as_ref().and_then(|p| app.cfg.profiles.get(p).map(|x| (p, x)));
    let prod = profile.is_some_and(|(_, p)| p.production);
    let mut spans = vec![Span::styled(" pulp-tui ", Style::default().add_modifier(Modifier::BOLD))];
    match profile {
        Some((name, p)) => {
            spans.push(Span::raw(format!("| server: {name} ({}) ", p.url)));
            let user = app.conn.as_ref().and_then(|c| c.caps.user.as_ref()).map(|u| u.username.clone());
            spans.push(Span::raw(match (&app.conn, user) {
                (Some(_), Some(u)) => format!("| user: {u} "),
                (Some(_), None) => "| anonymous ".into(),
                (None, _) if app.connecting => "| connecting... ".into(),
                (None, _) => "| NOT CONNECTED ".into(),
            }));
            if p.read_only {
                spans.push(Span::styled(" READ-ONLY ", Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)));
            }
            if prod {
                spans.push(Span::styled(" PRODUCTION ", Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD)));
            }
        }
        None => spans.push(Span::raw("| no server selected ")),
    }
    let ops = app.jobs.iter().filter(|j| !j.is_load).count();
    spans.push(Span::raw(format!(" | jobs {ops} | tasks {} active ", app.tasks.active())));
    let style = if prod { Style::default().bg(Color::Red).fg(Color::White) } else { Style::default().bg(Color::Blue).fg(Color::White) };
    f.render_widget(Paragraph::new(Line::from(spans)).style(style), area);
}

fn draw_nav(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![];
    let labels = std::iter::once("Dashboard").chain(kinds::NAV.iter().map(|(l, _)| *l)).chain(std::iter::once("Help"));
    for (i, l) in labels.enumerate() {
        let marker = if i == app.nav { "▶ " } else { "  " };
        let style = if i == app.nav && app.focus == Focus::Nav {
            sel()
        } else if i == app.nav {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::styled(format!("{marker}{l}"), style));
    }
    let border = if app.focus == Focus::Nav { Style::default().fg(Color::Cyan) } else { Style::default() };
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Screens ").border_style(border)), area);
}

fn draw_text_screen(f: &mut Frame, title: &str, lines: &[String], area: Rect) {
    let text: Vec<Line> = lines.iter().map(|l| Line::raw(l.as_str())).collect();
    f.render_widget(Paragraph::new(text).block(Block::bordered().title(format!(" {title} "))).wrap(Wrap { trim: false }), area);
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let l = &app.list;
    let mut area = area;
    if l.kind == Kind::Tasks && !app.jobs.is_empty() {
        let h = (app.jobs.len() as u16 + 2).min(8);
        let [jobs, rest] = Layout::vertical([Constraint::Length(h), Constraint::Min(5)]).areas(area);
        let lines: Vec<Line> =
            app.jobs.iter().map(|j| Line::raw(format!("{:>5}s  {}  {}", j.started.elapsed().as_secs(), j.label, j.progress))).collect();
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(" Client operations (Esc on a loading list cancels the request; server tasks continue) ")),
            jobs,
        );
        area = rest;
    }
    let wide = area.width >= 110;
    let [table_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(if wide { 58 } else { 100 }), Constraint::Percentage(if wide { 42 } else { 0 })])
            .areas(area);

    let cols = l.kind.columns();
    let mut info = vec![];
    if l.count > l.items.len() as u64 {
        info.push(format!("{}-{} of {}", l.offset + 1, l.offset + l.items.len() as u64, l.count));
    } else {
        info.push(format!("{} items", l.items.len()));
    }
    if !l.filter.is_empty() {
        info.push(format!("filter: {}", l.filter));
    }
    if let Some((c, d)) = l.sort {
        info.push(format!("sort: {}{}", cols.get(c).map(|c| c.title).unwrap_or(""), if d { " desc" } else { "" }));
    }
    if !l.marked.is_empty() {
        info.push(format!("{} marked", l.marked.len()));
    }
    let title = format!(" {}  [{}] ", l.kind.title(), info.join(", "));
    let border = if app.focus == Focus::List { Style::default().fg(Color::Cyan) } else { Style::default() };
    let block = Block::bordered().title(title).border_style(border);

    if l.loading && l.items.is_empty() || l.error.is_some() && l.items.is_empty() {
        let msg = if let Some(e) = &l.error { format!("Error: {e}\n\nr: retry") } else { "Loading... (Esc cancels the request)".into() };
        let style = if l.error.is_some() { Style::default().fg(Color::Red) } else { Style::default() };
        f.render_widget(Paragraph::new(msg).style(style).block(block).wrap(Wrap { trim: true }), table_area);
    } else if l.items.is_empty() {
        f.render_widget(Paragraph::new("No items. Enter opens actions (e.g. create).").block(block), table_area);
    } else {
        let header = Row::new(std::iter::once(Cell::from(" ")).chain(cols.iter().map(|c| Cell::from(c.title))))
            .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
        let rows: Vec<Row> = l
            .items
            .iter()
            .map(|v| {
                let mark = if l.marked.contains(&item_key(v)) { "●" } else { " " };
                Row::new(std::iter::once(Cell::from(mark)).chain(cols.iter().map(|c| Cell::from((c.get)(v)))))
            })
            .collect();
        let widths: Vec<Constraint> =
            std::iter::once(Constraint::Length(1)).chain(cols.iter().map(|c| Constraint::Min(c.width.min(40)))).collect();
        let table = Table::new(rows, widths).header(header).block(block).row_highlight_style(sel()).highlight_symbol("▶");
        let mut st = TableState::default().with_selected(Some(l.selected));
        f.render_stateful_widget(table, table_area, &mut st);
    }
    if detail_area.width > 0 {
        let lines = l.current().map(|v| detail_lines(&l.kind, v)).unwrap_or_default();
        let text: Vec<Line> = lines.into_iter().map(Line::raw).collect();
        f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Details ")).wrap(Wrap { trim: false }), detail_area);
    }
}

fn detail_lines(kind: &Kind, v: &serde_json::Value) -> Vec<String> {
    let mut out = vec![];
    match kind {
        Kind::Distributions(_) => {
            out.push(format!("URL: {}", s(v, "base_url")));
            out.push(match (v["publication"].as_str(), v["repository_version"].as_str(), v["repository"].as_str()) {
                (Some(p), _, _) => format!("PINNED to publication {} (immutable)", kinds::short(p)),
                (_, Some(rv), _) => format!("PINNED to repository version {}", kinds::short(rv)),
                (_, _, Some(r)) => format!("FOLLOWS LATEST of repository {}", kinds::short(r)),
                _ => "UNBOUND: serves nothing".into(),
            });
            out.push(String::new());
        }
        Kind::Repos(_) => {
            out.push(match v["retain_repo_versions"].as_u64() {
                Some(n) => format!("Retention: keeps the latest {n} versions (older are deleted by Pulp)"),
                None => "Retention: all versions are kept".into(),
            });
            if v["pulp_labels"].get(crate::cv::LABEL_CV).is_some() {
                out.push("Owned by a Content View (snapshot repository)".into());
            }
            out.push(String::new());
        }
        Kind::Tasks => {
            if !s(v, "error").is_empty() {
                out.push(format!("ERROR: {}", s(v, "error")));
            }
            out.push(format!("href: {}", s(v, "pulp_href")));
            for r in v["task"]["progress_reports"].as_array().into_iter().flatten() {
                out.push(format!("  [{}] {} {}/{}", s(r, "state"), s(r, "message"), s(r, "done"), s(r, "total")));
            }
            return out;
        }
        Kind::Logs { .. } => return log_lines(v),
        _ => {}
    }
    out.extend(pretty(v));
    out
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints = match (&app.modal, &app.screen, app.focus) {
        (Some(_), _, _) => "Esc: close".to_string(),
        (_, _, Focus::Nav) => "↑↓: choose screen  Enter: open  Tab: list  :: palette  ?: help  q: quit".into(),
        (_, Screen::List(_), _) => "↑↓/PgUp/PgDn  Enter: actions  /: search  s/S: sort  n/p: page  Space: mark  t: type  r: refresh  Esc: back  Tab: screens  :: palette  q: quit".into(),
        _ => "Tab: screens  r: refresh  :: palette  ?: help  N: notifications  q: quit".into(),
    };
    let [h, n] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    f.render_widget(Paragraph::new(hints).style(Style::default().fg(Color::DarkGray)), h);
    let job = app.jobs.iter().rev().find(|j| !j.is_load);
    let notice = app.notices.back().filter(|x| x.at.elapsed() < Duration::from_secs(10));
    let line = match (notice, job) {
        (Some(x), _) => {
            let color = match x.level {
                Level::Info => Color::Cyan,
                Level::Ok => Color::Green,
                Level::Warn => Color::Yellow,
                Level::Error => Color::Red,
            };
            Line::styled(format!("[{}] {}", x.level.tag(), x.text), Style::default().fg(color))
        }
        (None, Some(j)) => Line::raw(format!("… {} {} ({}s)", j.label, j.progress, j.started.elapsed().as_secs())),
        _ => Line::raw(""),
    };
    f.render_widget(Paragraph::new(line), n);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_modal(f: &mut Frame, m: &Modal, app: &App, area: Rect) {
    match m {
        Modal::Menu { title, items, selected, .. } => {
            let r = centered(area, 56, items.len() as u16 + 2);
            let lines: Vec<Line> = items
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let mutates = a.mutates(&app.list.kind);
                    let t = format!(
                        "{} [{}] {}{}",
                        if i == *selected { "▶" } else { " " },
                        a.key(),
                        a.label(),
                        if mutates { " *" } else { "" }
                    );
                    if i == *selected { Line::styled(t, sel()) } else { Line::raw(t) }
                })
                .collect();
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(Block::bordered().title(format!(" {title} (* changes the server) "))), r);
        }
        Modal::Form { form, .. } => draw_form(f, form, area),
        Modal::Confirm { title, lines, typed, input, danger, scroll, .. } => {
            let w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(40).clamp(50, 120) as u16 + 4;
            // Wrapped height, so the prompt row below always stays visible.
            let inner_w = w.saturating_sub(2).max(1) as usize;
            let wrapped: usize = lines.iter().map(|l| l.chars().count().max(1).div_ceil(inner_w)).sum();
            let r = centered(area, w, wrapped as u16 + 5);
            let text: Vec<Line> = lines
                .iter()
                .skip(*scroll)
                .map(|l| {
                    if l.contains("!!!") || l.contains("WARNING") || l.contains("BLOCKED") {
                        Line::styled(l.clone(), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                    } else if l.starts_with("+ ") {
                        Line::styled(l.clone(), Style::default().fg(Color::Green))
                    } else if l.starts_with("- ") {
                        Line::styled(l.clone(), Style::default().fg(Color::Red))
                    } else {
                        Line::raw(l.clone())
                    }
                })
                .collect();
            let prompt = match typed {
                Some(t) => format!("Type \"{t}\" and press Enter to confirm (Esc cancels): {input}▏"),
                None => "y / Enter: confirm    n / Esc: cancel".into(),
            };
            let border = if *danger { Style::default().fg(Color::Red) } else { Style::default().fg(Color::Yellow) };
            let block = Block::bordered().border_style(border).title(format!(" {title} ")).title_bottom(format!(
                " {}/{} ↑↓ PgUp/PgDn scroll ",
                scroll + 1,
                lines.len().max(1)
            ));
            let inner = block.inner(r);
            let [content, gap, p] = Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
            let _ = gap;
            f.render_widget(Clear, r);
            f.render_widget(block, r);
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), content);
            f.render_widget(Paragraph::new(prompt).style(Style::default().add_modifier(Modifier::BOLD)), p);
        }
        Modal::Text { title, lines, scroll } => {
            let r = centered(area, area.width.saturating_sub(6), area.height.saturating_sub(4));
            let text: Vec<Line> = lines
                .iter()
                .skip(*scroll)
                .map(|l| {
                    let style = if l.starts_with('+') {
                        Style::default().fg(Color::Green)
                    } else if l.starts_with("- ") || l.contains("MISMATCH") || l.contains("DRIFT") || l.contains("failed") {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default()
                    };
                    Line::styled(l.clone(), style)
                })
                .collect();
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(text).block(Block::bordered().title(format!(
                    " {title}  [{}/{}]  j/k PgUp/PgDn  Esc: close ",
                    scroll + 1,
                    lines.len().max(1)
                ))),
                r,
            );
        }
        Modal::Input { title, input, .. } => {
            let r = centered(area, 70, 3);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(format!("{input}▏")).block(Block::bordered().title(format!(" {title} "))), r);
        }
        Modal::Palette { input, selected } => {
            let entries = app.palette_entries(input);
            let r = centered(area, 60, (entries.len() as u16 + 3).min(20));
            let mut text = vec![Line::styled(format!("> {input}▏"), Style::default().add_modifier(Modifier::BOLD))];
            for (i, (l, _)) in entries.iter().enumerate().take(r.height.saturating_sub(3) as usize) {
                text.push(if i == *selected { Line::styled(format!("▶ {l}"), sel()) } else { Line::raw(format!("  {l}")) });
            }
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Command palette ")), r);
        }
    }
}

fn draw_form(f: &mut Frame, form: &Form, area: Rect) {
    let label_w = form.fields.iter().map(|x| x.label.chars().count()).max().unwrap_or(10) + 2;
    let h = form.fields.len() + form.notes.len() + 6;
    let r = centered(area, 100, h as u16);
    let mut lines: Vec<Line> = form.notes.iter().map(|n| Line::styled(n.clone(), Style::default().fg(Color::Yellow))).collect();
    if !form.notes.is_empty() {
        lines.push(Line::raw(""));
    }
    for (i, fld) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let req = if fld.required { "*" } else { " " };
        let cursor =
            if focused && !matches!(fld.kind, FieldKind::Bool | FieldKind::Choice(_) | FieldKind::Pick { .. }) { "▏" } else { "" };
        let label = format!("{}{req}{:<w$}", if focused { "▶" } else { " " }, fld.label, w = label_w);
        let value = format!("{}{cursor}", fld.display());
        lines.push(Line::from(vec![Span::raw(label), if focused { Span::styled(value, sel()) } else { Span::raw(value) }]));
    }
    lines.push(Line::raw(""));
    if let Some(h) = form.fields.get(form.focus).map(|x| x.help.clone()).filter(|h| !h.is_empty()) {
        lines.push(Line::styled(h, Style::default().fg(Color::DarkGray)));
    }
    if let Some(e) = &form.error {
        lines.push(Line::styled(format!("Error: {e}"), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)));
    }
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .borders(Borders::ALL)
                .title(format!(" {} ", form.title))
                .title_bottom(" Tab/↑↓ move  Space toggle/open  ←→ choose  Enter next/submit/pick  Ctrl-S submit  Esc cancel "),
        ),
        r,
    );
    if let Some(p) = &form.picker {
        draw_picker(f, form, p, r, area);
    }
}

/// The option list of an open picker, over the form. Rows scroll so the highlighted one stays visible.
fn draw_picker(f: &mut Frame, form: &Form, p: &Picker, form_area: Rect, area: Rect) {
    let fld = &form.fields[p.field];
    let multi = matches!(fld.kind, FieldKind::Pick { multi: true, .. });
    // Taller than the form when the form is small, so a few rows are always visible.
    let r = centered(area, form_area.width.saturating_sub(4), form_area.height.saturating_sub(2).max(12));
    let rows = r.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if let Some(e) = &p.error {
        vec![Line::styled(format!("Could not load choices: {e}"), Style::default().fg(Color::Red)), Line::raw("Type the value instead.")]
    } else if fld.options.is_none() {
        vec![Line::raw("loading…")]
    } else {
        let vis = form.visible();
        if vis.is_empty() {
            vec![Line::styled("no match", Style::default().fg(Color::DarkGray))]
        } else {
            let pad = if vis.iter().any(|i| !i.note.is_empty()) { 5 } else { 0 };
            let top = p.selected.saturating_sub(rows.saturating_sub(1));
            vis.iter()
                .enumerate()
                .skip(top)
                .take(rows)
                .map(|(i, it)| {
                    let mark = match (multi, p.marked.contains(&it.value)) {
                        (false, _) => "",
                        (true, true) => "[x] ",
                        (true, false) => "[ ] ",
                    };
                    let text = format!("{mark}{:<pad$}{}", it.note, it.label);
                    if i == p.selected { Line::styled(text, sel()) } else { Line::raw(text) }
                })
                .collect()
        }
    };
    let hint = match (p.error.is_some(), multi) {
        (true, _) => " Enter: use typed text  Esc: cancel ",
        (false, true) => " Space: mark  Enter: done  Esc: cancel ",
        (false, false) => " Enter: select  Esc: cancel ",
    };
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(format!(" Pick {} ── filter: {} ", fld.label, p.filter)).title_bottom(hint)),
        r,
    );
}

pub fn help_lines() -> Vec<String> {
    [
        "pulp-tui - keyboard reference",
        "",
        "Global",
        "  Tab            switch focus between screen list and main area",
        "  :  or Ctrl-P   command palette (screens, connect to server, commands)",
        "  ?              this help        N  notifications log        q  quit (Ctrl-C: immediate)",
        "",
        "Lists",
        "  ↑↓ j k PgUp PgDn g G   move        Enter  action menu (shortcut letters shown)",
        "  /   server-side search  s/S sort column / reverse   n/p next/previous page",
        "  Space  mark item (multi-select: compare, remove/add content)",
        "  t   toggle RPM/File, content type, tracked/server tasks, or task polling in Logs",
        "  r   refresh   Esc  cancel loading request / clear marks / go back",
        "",
        "Cancelling: Esc cancels only the client-side request. Pulp tasks keep running on the",
        "server; cancel them explicitly in Tasks (X). Quitting never cancels server work.",
        "",
        "Typical workflows",
        "  Repository: RPM/File Repositories -> c create -> u upload or f sync -> P publish -> D distribute",
        "  Rollback:   Versions (v) -> b on an older version -> choose distribution (history is kept)",
        "  Compare:    Versions -> Space on one version -> m on another",
        "  Content View: Content Views -> c create -> a add repos -> P publish (preview first)",
        "                -> M promote through Lifecycle Envs -> b roll back -> D verify/drift",
        "  Replication: Replication -> c add Upstream Pulp -> R replicate; F = fallback sync from a profile",
        "",
        "Safety",
        "  Read-only profiles block all server changes. Production profiles show a red banner",
        "  and destructive confirmations require typing the object name.",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::form::{Field, FormEvent, PickItem, Source};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn screen(form: &Form) -> String {
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| draw_form(f, form, f.area())).unwrap();
        t.backend().to_string()
    }

    #[test]
    fn picker_popup_shows_loading_scrolls_and_shows_errors() {
        let mut form = Form::new("t", vec![Field::pick("repo", "Repository", Source::Roles).multi()]);
        let ev = form.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(ev, FormEvent::Load(0)));
        assert!(screen(&form).contains("loading…"));
        let items = (0..40).map(|i| PickItem { label: format!("item{i:02}"), value: format!("v{i}"), note: "rpm".into() }).collect();
        form.set_options(0, Ok(items));
        form.picker.as_mut().unwrap().selected = 30;
        let s = screen(&form);
        assert!(s.contains("[ ] rpm  item30") && !s.contains("item00"), "{s}");
        assert!(s.contains("Space: mark"));
        form.picker.as_mut().unwrap().error = Some("boom".into());
        let s = screen(&form);
        assert!(s.contains("Could not load choices: boom") && s.contains("Enter: use typed text"), "{s}");
    }
}
