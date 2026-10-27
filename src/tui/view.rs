//! Rendering only: reads `App`, never performs I/O.

use super::actions::{log_lines, pretty};
use super::app::{App, Focus, Level, Modal, Screen, item_key};
use super::form::{Field, FieldKind, Form, Picker};
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
            if app.read_only() {
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
    if !app.queued.is_empty() {
        spans.push(Span::raw(format!("| {} dialog(s) waiting ", app.queued.len())));
    }
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
    let paged = l.count > l.items.len() as u64;
    if paged {
        info.push(format!("{}-{} of {}", l.offset + 1, l.offset + l.items.len() as u64, l.count));
    } else {
        info.push(format!("{} items", l.items.len()));
    }
    if !l.filter.is_empty() {
        info.push(format!("filter: {}", l.filter));
    }
    if let Some((c, d)) = l.sort {
        // `apply_sort` orders the loaded rows only.
        let page = if paged { " (this page)" } else { "" };
        info.push(format!("sort: {}{}{page}", cols.get(c).map(|c| c.title).unwrap_or(""), if d { " desc" } else { "" }));
    }
    if !l.marked.is_empty() {
        info.push(format!("{} marked", l.marked.len()));
    }
    // Without rows the body shows these.
    if !l.items.is_empty() {
        if let Some(e) = &l.error {
            info.push(format!("error: {e}"));
        }
        if l.loading {
            info.push("loading".into());
        }
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
        let rows = table_area.height.saturating_sub(3) as usize; // borders and header
        l.top.set(scroll_top(l.top.get(), l.selected, rows, l.items.len()));
        let mut st = TableState::default().with_selected(Some(l.selected)).with_offset(l.top.get());
        f.render_stateful_widget(table, table_area, &mut st);
    }
    if detail_area.width > 0 {
        let lines = l.current().map(|v| detail_lines(&l.kind, v)).unwrap_or_default();
        let text: Vec<Line> = lines.into_iter().map(Line::raw).collect();
        f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Details ")).wrap(Wrap { trim: false }), detail_area);
    }
}

/// Rows kept visible above and below the highlighted one (vim `scrolloff`).
const SCROLLOFF: usize = 5;

/// First visible row of a list with `rows` visible rows; `top` is the one of the last frame.
/// The view moves only when the highlight comes nearer than SCROLLOFF rows to an edge.
fn scroll_top(top: usize, sel: usize, rows: usize, len: usize) -> usize {
    let rows = rows.max(1);
    let so = SCROLLOFF.min(rows.saturating_sub(1) / 2);
    let top = top.min(sel.saturating_sub(so)).max((sel + so + 1).saturating_sub(rows));
    top.min(len.saturating_sub(rows))
}

/// `text` with the edit cursor `▏` at char index `cur`.
fn with_cursor(text: &str, cur: usize) -> String {
    let mut out: String = text.chars().take(cur).collect();
    out.push('▏');
    out.extend(text.chars().skip(cur));
    out
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
        Kind::Repos(..) => {
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
    // Rows stay unredacted for the actions; only the shown copy is masked.
    let mut v = v.clone();
    crate::session_log::redact(&mut v);
    out.extend(pretty(&v));
    out
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints = match (&app.modal, &app.screen, app.focus) {
        (Some(m), _, _) => modal_hint(m),
        (_, _, Focus::Nav) => "j/k g/G: choose screen  l/Enter: open  Tab: list  :: palette  ?: help  q: quit".into(),
        (_, Screen::List(_), _) => "j/k g/G Ctrl-D/U  h: screens  Enter: actions  /: search  s/S: sort  n/p: page  Space: mark  t: type  r: refresh  Esc: back  Tab: screens  :: palette  q: quit".into(),
        _ => "Tab: screens  r: refresh  :: palette  ?: help  N: notifications  q: quit".into(),
    };
    let [h, n] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    f.render_widget(Paragraph::new(hints).style(Style::default().fg(Color::DarkGray)), h);
    let ver = env!("PULP_TUI_VERSION");
    let [n, v] = Layout::horizontal([Constraint::Min(0), Constraint::Length(ver.len() as u16 + 1)]).areas(n);
    f.render_widget(Paragraph::new(ver).right_aligned().style(Style::default().fg(Color::DarkGray)), v);
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

/// Footer keys of the open modal, by kind and mode: they name the mode the operator is in.
fn modal_hint(m: &Modal) -> String {
    match m {
        Modal::Menu { .. } => "j/k g/G: move  Enter: run  letter: shortcut  Esc: close".into(),
        Modal::Form { form, .. } => match &form.picker {
            Some(p) => picker_hint(&form.fields[p.field], p),
            None => form_hint(form),
        },
        Modal::Confirm { typed: Some(_), .. } => "-- EDIT --  Enter: confirm  Ctrl-D/U: scroll  Esc: cancel".into(),
        Modal::Confirm { .. } => "y/Enter: confirm  n/Esc: cancel  j/k g/G Ctrl-D/U: scroll".into(),
        Modal::Text { watch: Some(_), .. } => "live: refreshes until the task ends  j/k g/G Ctrl-D/U: scroll  Esc: close".into(),
        Modal::Text { .. } => "j/k g/G Ctrl-D/U: scroll  Esc: close".into(),
        Modal::Input { .. } => "-- EDIT --  Enter: done  ←→ Ctrl-A/E: move  Ctrl-W/U/K: delete  Esc: cancel".into(),
        Modal::Palette { filtering: true, .. } => "-- FILTER --  Ctrl-J/K: move  Enter: run  Esc: stop filtering".into(),
        Modal::Palette { .. } => "j/k g/G: move  /: filter  Enter: run  Esc: close".into(),
    }
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
                    let mutates = a.mutates(&app.list.kind, app.shared());
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
                " {}/{} ↑↓ Ctrl-D/U scroll ",
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
        Modal::Text { title, lines, scroll, .. } => {
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
                Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().title(format!(
                    " {title}  [{}/{}]  j/k g/G Ctrl-D/U  Esc: close ",
                    scroll + 1,
                    lines.len().max(1)
                ))),
                r,
            );
        }
        Modal::Input { title, input, cursor, .. } => {
            let r = centered(area, 70, 3);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(with_cursor(input, *cursor)).block(Block::bordered().title(format!(" {title} "))), r);
        }
        Modal::Palette { input, selected, filtering } => {
            let entries = app.palette_entries(input);
            let r = centered(area, 60, (entries.len() as u16 + 3).min(20));
            let cursor = if *filtering { "▏" } else { "" };
            let mut text = vec![Line::styled(format!("> {input}{cursor}"), Style::default().add_modifier(Modifier::BOLD))];
            // Rows scroll so the highlighted one stays visible, as in the picker.
            let rows = r.height.saturating_sub(3) as usize;
            let top = selected.saturating_sub(rows.saturating_sub(1));
            for (i, (l, _)) in entries.iter().enumerate().skip(top).take(rows) {
                text.push(if i == *selected { Line::styled(format!("▶ {l}"), sel()) } else { Line::raw(format!("  {l}")) });
            }
            f.render_widget(Clear, r);
            let title = if *filtering { " Command palette -- FILTER -- " } else { " Command palette " };
            f.render_widget(Paragraph::new(text).block(Block::bordered().title(title)), r);
        }
    }
}

fn draw_form(f: &mut Frame, form: &Form, area: Rect) {
    let label_w = form.fields.iter().map(|x| x.label.chars().count()).max().unwrap_or(10) + 2;
    // Wrapped note height (as in the Confirm modal), so the fields and the error line below stay visible.
    let inner_w = 100.min(area.width.saturating_sub(2)).saturating_sub(2).max(1) as usize;
    let notes: usize = form.notes.iter().map(|n| n.chars().count().max(1).div_ceil(inner_w)).sum();
    let h = form.fields.len() + notes + 6;
    let r = centered(area, 100, h as u16);
    let mut lines: Vec<Line> = form.notes.iter().map(|n| Line::styled(n.clone(), Style::default().fg(Color::Yellow))).collect();
    if !form.notes.is_empty() {
        lines.push(Line::raw(""));
    }
    for (i, fld) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let req = if fld.required { "*" } else { " " };
        let label = format!("{}{req}{:<w$}", if focused { "▶" } else { " " }, fld.label, w = label_w);
        let value = if focused && form.editing {
            // A secret shows at most 12 bullets; its cursor stays inside them.
            let cur = if fld.kind == FieldKind::Secret { form.cursor.min(12) } else { form.cursor };
            with_cursor(&fld.display(), cur)
        } else {
            fld.display()
        };
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
            Block::bordered().borders(Borders::ALL).title(format!(" {} ", form.title)).title_bottom(format!(" {} ", form_hint(form))),
        ),
        r,
    );
    if let Some(p) = &form.picker {
        draw_picker(f, form, p, r, area);
    }
}

/// Keys of the form itself (the picker has its own hint). At most 90 characters: the box is 98 wide.
fn form_hint(form: &Form) -> String {
    if form.editing {
        let enter = if form.focus + 1 == form.fields.len() { "submit" } else { "next" };
        return format!("-- EDIT --  Enter: {enter}  Esc: done  Ctrl-S: submit");
    }
    let act = match form.fields.get(form.focus).map(|f| &f.kind) {
        Some(FieldKind::Choice(_)) => "h/l Space: choose",
        Some(FieldKind::Bool) => "Space: toggle",
        Some(FieldKind::Pick { .. }) => "Enter: pick  /: filter",
        _ => "i/Enter: edit",
    };
    format!("{act}  j/k: field  Ctrl-S: submit  Esc: cancel")
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
            p.top.set(scroll_top(p.top.get(), p.selected, rows, vis.len()));
            let top = p.top.get();
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
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(format!(" Pick {} ── filter: {} ", fld.label, p.filter))
                .title_bottom(format!(" {} ", picker_hint(fld, p))),
        ),
        r,
    );
}

/// Keys of the open picker, by mode. At most 90 characters: the popup is narrower than the 98-wide form box.
fn picker_hint(fld: &Field, p: &Picker) -> String {
    let multi = matches!(fld.kind, FieldKind::Pick { multi: true, .. });
    let take = if multi { "Space: mark  Enter: done" } else { "Enter: select" };
    match (p.error.is_some(), p.filtering) {
        (true, true) => "-- FILTER --  Enter: use typed text  Esc: stop filtering".into(),
        (true, false) => "Enter: use typed text  /: filter  Esc: close".into(),
        (false, true) => format!("-- FILTER --  Ctrl-J/K: move  {take}  Esc: stop filtering"),
        (false, false) => format!("j/k: move  /: filter  {take}  Esc: close"),
    }
}

pub fn help_lines() -> Vec<String> {
    [
        "pulp-tui - keyboard reference",
        "",
        "Global",
        "  Tab            switch focus between screen list and main area",
        "  :  or Ctrl-P   command palette (screens, connect to server, commands)",
        "  ?              this help (a text view: j k g G Ctrl-D/U scroll, Esc closes)",
        "  N              notifications log        q  quit (Ctrl-C: immediate)",
        "  j k g G        in the screen list: choose a screen; l or Enter opens it",
        "",
        "Lists",
        "  ↑↓ j k  PgUp PgDn Ctrl-D Ctrl-U  g G  move   h  screen list     Enter  action menu (shortcut letters shown)",
        "  /   server-side search  s/S sort the loaded page by column / reverse   n/p next/previous page",
        "  Space  mark item (multi-select: compare, remove/add content)",
        "  t   toggle RPM/File/Deb, content type, tracked/server tasks, task polling in Logs, or CV snapshots in Repositories",
        "  r   refresh   Esc  cancel loading request / clear marks / go back",
        "",
        "Forms",
        "  j k Tab ↑↓   move between fields      i/Enter  edit a text field (-- EDIT --)",
        "  h l ←→ Space change a choice / toggle  Ctrl-S  submit (both modes)    Esc  cancel (move mode)",
        "  In -- EDIT --: Enter goes to the next field (submits on the last), Esc stops editing",
        "    ←→ Ctrl-B/F  char    Alt-B/F Ctrl-←→  word    Home End Ctrl-A/E  line start/end",
        "    Backspace Delete  delete char    Ctrl-W  delete word    Ctrl-U/K  delete to line start/end",
        "  A form with one text field opens in -- EDIT --.",
        "",
        "Pickers and palette",
        "  j k g G ↑↓ PgUp PgDn Ctrl-D/U  move       /  filter (-- FILTER --)       Ctrl-S  take the selection and submit",
        "  On a closed pick field a letter other than j k opens the list in -- FILTER -- with that letter typed",
        "  In -- FILTER --: type to filter, Ctrl-J/K or Ctrl-N/P move, Esc keeps the filter; the next Esc closes",
        "  In move mode one Esc closes the list.",
        "  The palette (: or Ctrl-P) opens in -- FILTER --; Esc leaves it for j k g G, / filters again, Enter runs",
        "",
        "Text views and confirmations",
        "  j k g G Ctrl-D Ctrl-U ↑↓ PgUp PgDn Home End  scroll",
        "  A confirmation with a typed name takes every letter: only Ctrl-D/U, PgUp PgDn, Home End and ↑↓ scroll",
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
        "  Composite:  Content Views -> c create with Composite -> a add components (x remove) -> P publish",
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
    fn form_shows_edit_mode_and_cursor_only_while_editing() {
        let mut form = Form::new("t", vec![Field::text("name", "Name"), Field::text("other", "Other")]);
        let s = screen(&form);
        assert!(s.contains("i/Enter: edit") && s.contains("Ctrl-S: submit") && !s.contains("-- EDIT --") && !s.contains('▏'), "{s}");
        form.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        let s = screen(&form);
        assert!(s.contains("-- EDIT --") && s.contains('▏') && s.contains("Enter: next"), "{s}");
    }

    #[test]
    fn form_hint_enter_submits_on_the_last_field_and_move_hint_follows_the_field_kind() {
        let choice = FieldKind::Choice(vec!["a".into()]);
        let mut form = Form::new(
            "t",
            vec![
                Field::text("a", "A"),
                Field::new("b", "B", choice),
                Field::new("c", "C", FieldKind::Bool),
                Field::pick("d", "D", Source::Roles),
                Field::text("e", "E"),
            ],
        );
        for (focus, want) in
            [(0, "i/Enter: edit"), (1, "h/l Space: choose"), (2, "Space: toggle"), (3, "Enter: pick  /: filter"), (4, "i/Enter: edit")]
        {
            form.focus = focus;
            let h = form_hint(&form);
            assert!(h.contains(want) && h.contains("j/k: field  Ctrl-S: submit  Esc: cancel") && h.chars().count() <= 90, "{h}");
        }
        form.editing = true;
        form.focus = 3;
        assert!(form_hint(&form).contains("Enter: next"));
        form.focus = 4;
        assert!(form_hint(&form).contains("Enter: submit"));
    }

    #[tokio::test]
    async fn palette_scrolls_so_the_selected_entry_stays_visible() {
        let st = std::sync::Arc::new(crate::store::Store::open_in_memory().await.unwrap());
        let app = App::new(crate::config::Config::default(), "/dev/null".into(), st.clone(), Ok(st));
        let entries = app.palette_entries("");
        assert!(entries.len() > 17, "the box shows 17 rows");
        let draw = |selected| {
            let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let m = Modal::Palette { input: String::new(), selected, filtering: false };
            t.draw(|f| draw_modal(f, &m, &app, f.area())).unwrap();
            t.backend().to_string()
        };
        assert!(draw(0).contains(&format!("▶ {}", entries[0].0)));
        let last = entries.len() - 1;
        let s = draw(last);
        assert!(s.contains(&format!("▶ {}", entries[last].0)) && !s.contains(&entries[0].0), "{s}");
    }

    #[tokio::test]
    async fn read_only_banner_follows_the_connection() {
        use crate::config::{AuthMethod, Config, Profile};
        let st = std::sync::Arc::new(crate::store::Store::open_in_memory().await.unwrap());
        let p = Profile { url: "https://pulp.example.com".into(), auth: AuthMethod::None, ..Default::default() };
        let mut cfg = Config::default();
        cfg.profiles.insert("lab".into(), p.clone());
        let conn = |read_only| {
            let client = crate::pulp::PulpClient::new("lab", &Profile { read_only, ..p.clone() }, &cfg.settings).unwrap();
            crate::tui::app::Conn { client, caps: Default::default(), scope: "lab".into() }
        };
        let (rw, ro) = (conn(false), conn(true));
        let mut app = App::new(cfg, "/dev/null".into(), st.clone(), Ok(st));
        app.profile = Some("lab".into());
        app.conn = Some(rw);
        app.cfg.profiles.get_mut("lab").unwrap().read_only = true; // edited after Connect: applies on the next one
        let shown = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
            t.draw(|f| draw(f, app)).unwrap();
            t.backend().to_string().contains("READ-ONLY")
        };
        assert!(!shown(&app));
        app.conn = Some(ro);
        assert!(shown(&app));
    }

    #[tokio::test]
    async fn list_title_shows_a_refresh_error_over_old_rows() {
        let st = std::sync::Arc::new(crate::store::Store::open_in_memory().await.unwrap());
        let mut app = App::new(crate::config::Config::default(), "/dev/null".into(), st.clone(), Ok(st));
        app.screen = Screen::List(Kind::Profiles);
        app.list.items = vec![serde_json::json!({"name": "old-row"})];
        app.list.error = Some("server error [502]".into());
        let screen = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
            t.draw(|f| draw(f, app)).unwrap();
            t.backend().to_string()
        };
        let s = screen(&app);
        assert!(s.contains("error: server error [502]") && s.contains("old-row"), "{s}");
        app.list.error = None;
        app.list.loading = true;
        let s = screen(&app);
        assert!(s.contains("loading") && s.contains("old-row"), "{s}");
    }

    #[tokio::test]
    async fn list_title_says_a_sort_covers_only_the_loaded_page() {
        let st = std::sync::Arc::new(crate::store::Store::open_in_memory().await.unwrap());
        let mut app = App::new(crate::config::Config::default(), "/dev/null".into(), st.clone(), Ok(st));
        app.screen = Screen::List(Kind::Profiles);
        app.list.kind = Kind::Profiles;
        app.list.items = vec![serde_json::json!({"name": "a"}), serde_json::json!({"name": "b"})];
        app.list.sort = Some((1, true));
        let screen = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
            t.draw(|f| draw(f, app)).unwrap();
            t.backend().to_string()
        };
        app.list.count = 2;
        let s = screen(&app);
        assert!(s.contains("sort: name desc]"), "{s}");
        app.list.count = 250;
        let s = screen(&app);
        assert!(s.contains("1-2 of 250") && s.contains("sort: name desc (this page)]"), "{s}");
    }

    #[tokio::test]
    async fn details_pane_masks_remote_headers_and_tokens() {
        use crate::pulp::RepoType;
        let st = std::sync::Arc::new(crate::store::Store::open_in_memory().await.unwrap());
        let mut app = App::new(crate::config::Config::default(), "/dev/null".into(), st.clone(), Ok(st));
        app.screen = Screen::List(Kind::Remotes(RepoType::Rpm));
        app.list.kind = Kind::Remotes(RepoType::Rpm);
        app.list.items = vec![serde_json::json!({
            "name": "r1", "headers": [{"Authorization": "Bearer abc"}], "sles_auth_token": "tok"
        })];
        // The Details pane needs 110 columns beside the 22 of the screen list.
        let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
        t.draw(|f| draw(f, &app)).unwrap();
        let s = t.backend().to_string();
        assert!(s.contains("Details") && s.contains("•••") && !s.contains("Bearer abc") && !s.contains("\"tok\""), "{s}");
    }

    #[test]
    fn picker_shows_filter_mode() {
        let mut form = Form::new("t", vec![Field::pick("repo", "Repository", Source::Roles)]);
        form.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        form.set_options(0, Ok(vec![PickItem { label: "a".into(), value: "a".into(), note: String::new() }]));
        let s = screen(&form);
        assert!(s.contains("/: filter") && !s.contains("-- FILTER --"), "{s}");
        form.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(screen(&form).contains("-- FILTER --"));
    }

    #[test]
    fn modal_hint_names_mode_and_keys() {
        let pal = |filtering| Modal::Palette { input: String::new(), selected: 0, filtering };
        assert!(modal_hint(&pal(true)).starts_with("-- FILTER --"));
        let h = modal_hint(&pal(false));
        assert!(h.contains("/: filter") && !h.contains("-- FILTER --"), "{h}");
        assert!(
            modal_hint(&Modal::Input { title: String::new(), input: String::new(), cursor: 0, on_done: None }).starts_with("-- EDIT --")
        );
        assert!(modal_hint(&Modal::Text { title: String::new(), lines: vec![], scroll: 0, watch: None }).contains("Ctrl-D/U"));
        let mut form = Form::new("t", vec![Field::text("a", "A")]);
        form.editing = true;
        assert!(modal_hint(&Modal::Form { form, submit: Box::new(|_, _| Ok(())) }).starts_with("-- EDIT --"));
    }

    #[test]
    fn modal_hint_follows_picker_and_confirm_kinds() {
        let mut form = Form::new("t", vec![Field::pick("repo", "Repository", Source::Roles)]);
        form.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let h = modal_hint(&Modal::Form { form, submit: Box::new(|_, _| Ok(())) });
        assert!(h.contains("/: filter") && h.contains("Esc: close"), "{h}");
        let confirm = |typed: Option<&str>| Modal::Confirm {
            title: String::new(),
            lines: vec![],
            typed: typed.map(Into::into),
            input: String::new(),
            on_yes: None,
            danger: false,
            scroll: 0,
        };
        let h = modal_hint(&confirm(Some("x")));
        assert!(h.starts_with("-- EDIT --") && h.contains("Enter: confirm") && h.contains("Ctrl-D/U: scroll"), "{h}");
        let h = modal_hint(&confirm(None));
        assert!(h.contains("y/Enter") && h.contains("n/Esc") && !h.contains("-- EDIT --"), "{h}");
        let menu = Modal::Menu { title: String::new(), items: vec![], selected: 0, item: serde_json::Value::Null };
        assert!(modal_hint(&menu).contains("j/k g/G") && modal_hint(&menu).contains("Esc: close"));
    }

    #[test]
    fn help_lists_vim_keys() {
        let h = help_lines().join("\n");
        for t in ["Ctrl-D", "Ctrl-J", "i/Enter", "-- FILTER --", "-- EDIT --"] {
            assert!(h.contains(t), "help misses {t}");
        }
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

    #[test]
    fn scroll_top_keeps_five_rows_around_the_highlight() {
        // 20 rows visible, 100 items. Down: the view moves when the highlight is 5 rows from the bottom.
        assert_eq!(scroll_top(0, 14, 20, 100), 0);
        assert_eq!(scroll_top(0, 15, 20, 100), 1);
        // Up: the view stays until the highlight is 5 rows from the top.
        assert_eq!(scroll_top(30, 40, 20, 100), 30);
        assert_eq!(scroll_top(30, 35, 20, 100), 30);
        assert_eq!(scroll_top(30, 34, 20, 100), 29);
        // Ends: no empty rows after the last item; a short list never scrolls.
        assert_eq!(scroll_top(90, 99, 20, 100), 80);
        assert_eq!(scroll_top(0, 9, 20, 10), 0);
        // A small view keeps a smaller margin.
        assert_eq!(scroll_top(0, 3, 4, 100), 1);
        assert_eq!(scroll_top(0, 0, 0, 100), 0);
    }

    #[test]
    fn edit_cursor_is_drawn_at_its_position() {
        assert_eq!(with_cursor("abc", 1), "a▏bc");
        assert_eq!(with_cursor("", 0), "▏");
        let mut form = Form::new("t", vec![Field::text("a", "A").value("hello")]);
        form.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert!(screen(&form).contains("▏hello"));
    }
}
