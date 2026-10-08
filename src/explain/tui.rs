//! The full-screen browser of `runway explain`: keys on the left, what the
//! selected one does on the right, `/` to search, `f` for the keys your
//! file sets.

use super::catalog::{Entry, catalog, find, is_placeholder};
use super::file::FileInfo;
use crate::error::{Error, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::{Frame, Terminal};
use std::collections::BTreeSet;

/// The brand colors of runway.echaouchna.dev.
#[derive(Clone, Copy)]
struct Theme {
    cyan: Color,
    violet: Color,
    pink: Color,
    green: Color,
    amber: Color,
    red: Color,
    muted: Color,
    dim: Color,
    line: Color,
    select: Color,
    color: bool,
}

impl Theme {
    fn new(color: bool) -> Self {
        Self {
            cyan: Color::Rgb(0x22, 0xd3, 0xee),
            violet: Color::Rgb(0xa7, 0x8b, 0xfa),
            pink: Color::Rgb(0xf4, 0x72, 0xb6),
            green: Color::Rgb(0x34, 0xd3, 0x99),
            amber: Color::Rgb(0xfb, 0xbf, 0x24),
            red: Color::Rgb(0xf8, 0x71, 0x71),
            muted: Color::Rgb(0x9a, 0xa6, 0xd1),
            dim: Color::Rgb(0x6b, 0x77, 0xa6),
            line: Color::Rgb(0x26, 0x33, 0x5f),
            select: Color::Rgb(0x16, 0x21, 0x4a),
            color,
        }
    }
    fn fg(&self, c: Color) -> Style {
        match self.color {
            true => Style::new().fg(c),
            false => Style::new(),
        }
    }
    fn heading(&self) -> Style {
        self.fg(self.violet).add_modifier(Modifier::BOLD)
    }
}

/// A row of the tree: a key, a guide topic, or a group of service keys.
#[derive(Debug, Clone)]
struct Node {
    label: String,
    entry: Option<&'static Entry>,
    depth: usize,
    parent: Option<usize>,
    children: Vec<usize>,
}

pub struct App {
    nodes: Vec<Node>,
    roots: Vec<usize>,
    expanded: BTreeSet<usize>,
    visible: Vec<usize>,
    list: ListState,
    scroll: u16,
    /// The furthest the explanation scrolls (from the last drawing).
    max_scroll: u16,
    /// Rows of the explanation on screen (a page).
    page: u16,
    /// ↑↓ scroll the explanation instead of moving in the keys.
    reading: bool,
    query: String,
    typing: bool,
    only_file: bool,
    help: bool,
    quit: bool,
    file: Option<FileInfo>,
    file_entry: Entry,
    theme: Theme,
}

impl App {
    pub fn new(file: Option<FileInfo>, color: bool) -> Self {
        let mut app = App {
            nodes: Vec::new(),
            roots: Vec::new(),
            expanded: BTreeSet::new(),
            visible: Vec::new(),
            list: ListState::default(),
            scroll: 0,
            max_scroll: 0,
            page: 10,
            reading: false,
            query: String::new(),
            typing: false,
            only_file: false,
            help: false,
            quit: false,
            file,
            file_entry: Entry {
                path: ":file".into(),
                title: Some("Your runway.yaml".into()),
                ..Default::default()
            },
            theme: Theme::new(color),
        };
        app.build();
        app.refresh();
        app.list.select(Some(0));
        app
    }

    fn add(
        &mut self,
        label: String,
        entry: Option<&'static Entry>,
        parent: Option<usize>,
    ) -> usize {
        let depth = parent.map_or(0, |p| self.nodes[p].depth + 1);
        self.nodes.push(Node {
            label,
            entry,
            depth,
            parent,
            children: Vec::new(),
        });
        let id = self.nodes.len() - 1;
        match parent {
            Some(p) => self.nodes[p].children.push(id),
            None => self.roots.push(id),
        }
        id
    }

    fn build(&mut self) {
        let mut topics = catalog().iter().filter(|e| e.is_topic());
        if let Some(start) = topics.next() {
            self.add(start.name().to_string(), Some(start), None);
        }
        if self.file.is_some() {
            // Rendered from the file, not the catalog.
            self.add("Your runway.yaml".into(), None, None);
        }
        for t in topics {
            self.add(t.name().to_string(), Some(t), None);
        }
        let mut by_path: std::collections::HashMap<&str, usize> = Default::default();
        let mut groups: std::collections::HashMap<(usize, &str), usize> = Default::default();
        for e in catalog().iter().filter(|e| !e.is_topic()) {
            let mut parent = e.parent().and_then(|p| by_path.get(p).copied());
            if let (Some(p), Some(g)) = (parent, e.group.as_deref()) {
                let id = match groups.get(&(p, g)) {
                    Some(id) => *id,
                    None => {
                        let id = self.add(g.to_string(), None, Some(p));
                        groups.insert((p, g), id);
                        id
                    }
                };
                parent = Some(id);
            }
            let id = self.add(e.name().to_string(), Some(e), parent);
            by_path.insert(&e.path, id);
        }
    }

    fn is_file_node(&self, id: usize) -> bool {
        self.nodes[id].entry.is_none() && self.nodes[id].parent.is_none()
    }

    fn matches(&self, id: usize) -> bool {
        let n = &self.nodes[id];
        let words: Vec<String> = self
            .query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let hay = match n.entry {
            Some(e) => format!(
                "{} {} {} {}",
                e.path,
                e.name(),
                e.summary(),
                e.kind.as_deref().unwrap_or("")
            )
            .to_lowercase(),
            None => n.label.to_lowercase(),
        };
        (words.is_empty() || words.iter().all(|w| hay.contains(w.as_str())))
            && (!self.only_file || self.in_file(id))
    }

    /// The node (or a key below it) is set in the file.
    fn in_file(&self, id: usize) -> bool {
        let n = &self.nodes[id];
        match (&self.file, n.entry) {
            (Some(f), Some(e)) => f.sets_below(e) || n.children.iter().any(|c| self.in_file(*c)),
            (Some(_), None) => self.is_file_node(id) || n.children.iter().any(|c| self.in_file(*c)),
            (None, _) => false,
        }
    }

    fn filtering(&self) -> bool {
        !self.query.trim().is_empty() || self.only_file
    }

    /// Recomputes the rows, keeping the selected node when it stays visible.
    fn refresh(&mut self) {
        let keep = self.selected_node();
        let mut visible = Vec::new();
        let roots = self.roots.clone();
        for r in roots {
            self.collect(r, &mut visible);
        }
        self.visible = visible;
        let i = keep
            .and_then(|k| self.visible.iter().position(|v| *v == k))
            .unwrap_or(0);
        self.list.select((!self.visible.is_empty()).then_some(i));
    }

    /// Adds `id` and what is shown below it; while filtering, a node shows
    /// when it or a node below it matches (with every match unfolded).
    fn collect(&self, id: usize, out: &mut Vec<usize>) -> bool {
        if !self.filtering() {
            out.push(id);
            if self.expanded.contains(&id) {
                for c in &self.nodes[id].children {
                    self.collect(*c, out);
                }
            }
            return true;
        }
        let at = out.len();
        out.push(id);
        let mut any = false;
        for c in &self.nodes[id].children {
            any |= self.collect(*c, out);
        }
        if any || self.matches(id) {
            true
        } else {
            out.truncate(at);
            false
        }
    }

    fn selected_node(&self) -> Option<usize> {
        self.list
            .selected()
            .and_then(|i| self.visible.get(i).copied())
    }

    fn select(&mut self, i: usize) {
        if self.visible.is_empty() {
            return;
        }
        let i = i.min(self.visible.len() - 1);
        if self.list.selected() != Some(i) {
            self.scroll = 0;
        }
        self.list.select(Some(i));
    }

    /// Selects the node of `path`, unfolding its parents.
    pub fn go_to(&mut self, path: &str) -> bool {
        let Some(id) = self
            .nodes
            .iter()
            .position(|n| n.entry.is_some_and(|e| e.path == path))
        else {
            return false;
        };
        let mut p = self.nodes[id].parent;
        while let Some(x) = p {
            self.expanded.insert(x);
            p = self.nodes[x].parent;
        }
        self.query.clear();
        self.only_file = false;
        self.reading = false;
        self.refresh();
        if let Some(i) = self.visible.iter().position(|v| *v == id) {
            self.select(i);
        }
        true
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.help {
            self.help = false;
            return;
        }
        let i = self.list.selected().unwrap_or(0);
        if self.typing {
            match k.code {
                KeyCode::Char(c) if !ctrl => {
                    self.query.push(c);
                    self.refresh();
                    self.select(self.first_match());
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.refresh();
                }
                KeyCode::Enter => self.typing = false,
                KeyCode::Esc => {
                    self.typing = false;
                    self.query.clear();
                    self.refresh();
                }
                KeyCode::Down => self.select(i + 1),
                KeyCode::Up => self.select(i.saturating_sub(1)),
                _ => {}
            }
            return;
        }
        // Either panel.
        match k.code {
            KeyCode::Char('q') => return self.quit = true,
            KeyCode::Char('c') if ctrl => return self.quit = true,
            KeyCode::Char('?') => return self.help = true,
            KeyCode::Char('/') => {
                self.reading = false;
                return self.typing = true;
            }
            KeyCode::Char('f') if self.file.is_some() => {
                self.only_file = !self.only_file;
                return self.refresh();
            }
            KeyCode::Char('n') => return self.next_problem(),
            KeyCode::PageDown => return self.scroll_by(self.page as i32),
            KeyCode::PageUp => return self.scroll_by(-(self.page as i32)),
            KeyCode::Char('d') if ctrl => return self.scroll_by(self.page as i32 / 2),
            KeyCode::Char('u') if ctrl => return self.scroll_by(-(self.page as i32 / 2)),
            _ => {}
        }
        if self.reading {
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
                KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
                KeyCode::Char(' ') => self.scroll_by(self.page as i32),
                KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
                KeyCode::End | KeyCode::Char('G') => self.scroll = self.max_scroll,
                KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc | KeyCode::Tab => {
                    self.reading = false
                }
                KeyCode::Enter => self.follow(),
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Esc if self.filtering() => {
                self.query.clear();
                self.only_file = false;
                self.refresh();
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.select(i + 1),
            KeyCode::Up | KeyCode::Char('k') => self.select(i.saturating_sub(1)),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
            KeyCode::Right | KeyCode::Char('l') => self.unfold_or_read(),
            KeyCode::Enter | KeyCode::Tab => self.reading = self.selected_node().is_some(),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Left | KeyCode::Char('h') => self.fold_or_leave(),
            _ => {}
        }
    }

    fn scroll_by(&mut self, n: i32) {
        self.scroll = (self.scroll as i32 + n).clamp(0, self.max_scroll as i32) as u16;
    }

    /// The first key whose name has the query, else the first match.
    fn first_match(&self) -> usize {
        let q = self.query.trim().to_lowercase();
        let named = |id: &usize| {
            self.nodes[*id]
                .entry
                .is_some_and(|e| !q.is_empty() && e.name().to_lowercase().contains(&q))
        };
        self.visible
            .iter()
            .position(|id| named(id) && self.matches(*id))
            .or_else(|| {
                self.visible
                    .iter()
                    .position(|id| self.matches(*id) && self.nodes[*id].entry.is_some())
            })
            .unwrap_or(0)
    }

    /// Unfolds a folded node; otherwise moves to the explanation.
    fn unfold_or_read(&mut self) {
        let Some(id) = self.selected_node() else {
            return;
        };
        if !self.nodes[id].children.is_empty() && !self.filtering() && self.expanded.insert(id) {
            self.refresh();
        } else {
            self.reading = true;
        }
    }

    fn fold_or_leave(&mut self) {
        let Some(id) = self.selected_node() else {
            return;
        };
        if !self.filtering() && self.expanded.remove(&id) {
            self.refresh();
            return;
        }
        if let Some(p) = self.nodes[id].parent
            && let Some(i) = self.visible.iter().position(|v| *v == p)
        {
            self.select(i);
        }
    }

    fn toggle(&mut self) {
        let Some(id) = self.selected_node() else {
            return;
        };
        if !self.nodes[id].children.is_empty() && !self.filtering() {
            if !self.expanded.remove(&id) {
                self.expanded.insert(id);
            }
            self.refresh();
        }
    }

    /// Goes to the first "see also" of the selected key (`defaults` leads
    /// to `service`), back in the keys.
    fn follow(&mut self) {
        let see = self
            .selected_node()
            .and_then(|id| self.nodes[id].entry)
            .and_then(|e| e.see.first())
            .cloned();
        if let Some(see) = see {
            self.go_to(&see);
        }
    }

    /// Selects the next key with a validation error.
    fn next_problem(&mut self) {
        let Some(f) = &self.file else { return };
        let ids: Vec<usize> = (0..self.nodes.len())
            .filter(|id| {
                self.nodes[*id]
                    .entry
                    .is_some_and(|e| f.issues(e).iter().any(|(_, s, _)| *s == "error"))
            })
            .collect();
        let current = self.selected_node().unwrap_or(0);
        let next = ids
            .iter()
            .find(|id| **id > current)
            .or(ids.first())
            .copied();
        if let Some(id) = next
            && let Some(e) = self.nodes[id].entry
        {
            let path = e.path.clone();
            self.go_to(&path);
        }
    }

    // ------------------------------------------------------------ drawing

    pub fn draw(&mut self, f: &mut Frame) {
        let t = self.theme;
        let [top, main, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(f.area());
        f.render_widget(Paragraph::new(self.title_line()), top);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)])
                .areas(main);

        let width = left.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|id| ListItem::new(self.row(*id, width)))
            .collect();
        let keys_title = match (self.filtering(), self.only_file) {
            (_, true) => " Keys in your file ".to_string(),
            (true, _) => format!(" Keys matching \"{}\" ", self.query.trim()),
            _ => " Keys ".into(),
        };
        // The focused panel has a bright border.
        let border = |focused: bool| match focused {
            true => t.fg(t.cyan),
            false => t.fg(t.line),
        };
        let list = List::new(items)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(border(!self.reading))
                    .title(Span::styled(
                        keys_title,
                        match self.reading {
                            true => t.fg(t.muted),
                            false => t.fg(t.cyan).add_modifier(Modifier::BOLD),
                        },
                    )),
            )
            .highlight_style(match (t.color, self.reading) {
                (true, false) => Style::new().bg(t.select).add_modifier(Modifier::BOLD),
                (true, true) => Style::new().bg(t.select),
                (false, _) => Style::new().add_modifier(Modifier::REVERSED),
            })
            .highlight_symbol(if self.reading { "│" } else { "▌" });
        f.render_stateful_widget(list, left, &mut self.list);

        let (title, body) = self.details();
        // Rows once wrapped as rendered (word wrapping, in the width left by
        // the borders and the padding): how far it scrolls.
        let inner_w = right.width.saturating_sub(4).max(1);
        let rows = Paragraph::new(body.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner_w);
        self.page = right.height.saturating_sub(2).max(1);
        self.max_scroll = (rows as u16).saturating_sub(self.page);
        self.scroll = self.scroll.min(self.max_scroll);
        let more = match (self.scroll > 0, self.scroll < self.max_scroll) {
            (true, true) => " ↕ ",
            (false, true) => " ↓ ",
            (true, false) => " ↑ ",
            (false, false) => "",
        };
        f.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(border(self.reading))
                        .padding(Padding::horizontal(1))
                        .title(Span::styled(
                            format!(" {title} "),
                            t.fg(t.cyan).add_modifier(Modifier::BOLD),
                        ))
                        .title_bottom(Line::styled(more, t.fg(t.dim)).right_aligned()),
                ),
            right,
        );
        if self.max_scroll > 0 {
            let mut state = ScrollbarState::new(self.max_scroll as usize)
                .position(self.scroll as usize)
                .viewport_content_length(self.page as usize);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .track_symbol(Some("│"))
                    .track_style(border(self.reading))
                    .thumb_style(t.fg(if self.reading { t.cyan } else { t.dim })),
                right.inner(ratatui::layout::Margin::new(0, 1)),
                &mut state,
            );
        }
        f.render_widget(Paragraph::new(self.status_line()), bottom);
        if self.help {
            self.draw_help(f);
        }
    }

    fn title_line(&self) -> Line<'static> {
        let t = self.theme;
        let badge = match t.color {
            true => Style::new()
                .fg(Color::Black)
                .bg(t.cyan)
                .add_modifier(Modifier::BOLD),
            false => Style::new().add_modifier(Modifier::REVERSED),
        };
        let mut spans = vec![Span::styled(" runway explain ", badge), Span::raw("  ")];
        match &self.file {
            Some(file) => {
                spans.push(Span::styled(
                    file.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    t.fg(t.green),
                ));
                if let Some(app) = &file.app {
                    spans.push(Span::styled(format!(" · app {app}"), t.fg(t.muted)));
                }
                let names: Vec<&str> = file.stages.iter().map(|s| s.name.as_str()).collect();
                if !names.is_empty() {
                    spans.push(Span::styled(
                        format!(" · stages {}", names.join(", ")),
                        t.fg(t.muted),
                    ));
                }
                let errors = file.error_count();
                if errors > 0 {
                    spans.push(Span::styled(
                        format!(" · {errors} problem(s)"),
                        t.fg(t.red).add_modifier(Modifier::BOLD),
                    ));
                }
            }
            None => spans.push(Span::styled(
                "no runway.yaml here: explaining every key",
                t.fg(t.muted),
            )),
        }
        Line::from(spans)
    }

    fn status_line(&self) -> Line<'static> {
        let t = self.theme;
        if self.typing {
            let n = self.visible.iter().filter(|id| self.matches(**id)).count();
            return Line::from(vec![
                Span::styled(" / ", t.fg(t.cyan).add_modifier(Modifier::BOLD)),
                Span::raw(self.query.clone()),
                Span::styled("█", t.fg(t.cyan)),
                Span::styled(
                    format!("   {n} match(es) · Enter keep · Esc clear"),
                    t.fg(t.dim),
                ),
            ]);
        }
        let mut hints = match self.reading {
            true => {
                let mut h = vec![
                    ("↑↓", "scroll"),
                    ("PgDn PgUp", "page"),
                    ("← Esc", "back to keys"),
                ];
                let see = self
                    .selected_node()
                    .and_then(|id| self.nodes[id].entry)
                    .is_some_and(|e| !e.see.is_empty());
                if see {
                    h.push(("Enter", "see also"));
                }
                h
            }
            false => vec![
                ("↑↓", "move"),
                ("→ Enter", "read"),
                ("← Space", "fold"),
                ("/", "search"),
            ],
        };
        if self.file.is_some() && !self.reading {
            hints.push(("f", "your keys"));
            hints.push(("n", "next problem"));
        }
        hints.extend([("?", "help"), ("q", "quit")]);
        let mut spans = vec![Span::raw(" ")];
        for (k, what) in hints {
            spans.push(Span::styled(k, t.fg(t.cyan).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(format!(" {what}   "), t.fg(t.dim)));
        }
        Line::from(spans)
    }

    fn row(&self, id: usize, width: usize) -> Line<'static> {
        let t = self.theme;
        let n = &self.nodes[id];
        // While filtering, a node is open when a node below it shows.
        let open = match self.filtering() {
            true => n.children.iter().any(|c| self.visible.contains(c)),
            false => self.expanded.contains(&id),
        };
        let leaf = n.children.is_empty() || (self.filtering() && !open);
        let arrow = match (leaf, open) {
            (true, _) => "  ",
            (false, true) => "▾ ",
            (false, false) => "▸ ",
        };
        let indent = "  ".repeat(n.depth);
        let label_style = match n.entry {
            None if self.is_file_node(id) => t.fg(t.green).add_modifier(Modifier::BOLD),
            None => t.fg(t.muted).add_modifier(Modifier::ITALIC),
            Some(e) if e.is_topic() => t.fg(t.violet).add_modifier(Modifier::BOLD),
            Some(_) if is_placeholder(&n.label) => t.fg(t.amber),
            Some(_) if n.depth == 0 => t.fg(t.cyan).add_modifier(Modifier::BOLD),
            Some(_) => Style::new(),
        };
        let (mark, mark_style) = self.marker(id);
        let used = indent.chars().count() + 2 + n.label.chars().count();
        let pad = width.saturating_sub(used + 2);
        Line::from(vec![
            Span::raw(indent),
            Span::styled(arrow, t.fg(t.dim)),
            Span::styled(n.label.clone(), label_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(mark, mark_style),
        ])
    }

    /// ✗ a problem, ● set in the file, ◦ something below it is set.
    fn marker(&self, id: usize) -> (&'static str, Style) {
        let t = self.theme;
        let Some(f) = &self.file else {
            return ("", Style::new());
        };
        let n = &self.nodes[id];
        if self.is_file_node(id) {
            let bad = f.error.is_some() || f.stages.iter().any(|s| !s.errors.is_empty());
            return match bad {
                true => (" ✗", t.fg(t.red)),
                false => (" ✓", t.fg(t.green)),
            };
        }
        let Some(e) = n.entry else {
            return match self.in_file(id) {
                true => (" ◦", t.fg(t.green)),
                false => ("", Style::new()),
            };
        };
        if !e.is_topic() && f.has_error_below(&e.path) {
            return (" ✗", t.fg(t.red));
        }
        if !f.places(e).is_empty() {
            (" ●", t.fg(t.green))
        } else if self.in_file(id) {
            (" ◦", t.fg(t.green))
        } else {
            ("", Style::new())
        }
    }

    /// The title and text of the selected node.
    fn details(&self) -> (String, Text<'static>) {
        let Some(id) = self.selected_node() else {
            return (
                "Nothing matches".into(),
                Text::from("Esc clears the search."),
            );
        };
        let n = &self.nodes[id];
        match n.entry {
            Some(e) => (
                if e.is_topic() {
                    e.name().to_string()
                } else {
                    e.path.clone()
                },
                self.entry_text(e, &n.children),
            ),
            None if self.is_file_node(id) => (self.file_entry.name().to_string(), self.file_text()),
            None => (
                format!("{} · {}", self.nodes[n.parent.unwrap_or(id)].label, n.label),
                self.children_text(&n.children),
            ),
        }
    }

    fn entry_text(&self, e: &Entry, children: &[usize]) -> Text<'static> {
        let t = self.theme;
        let mut lines: Vec<Line> = Vec::new();
        if !e.is_topic() {
            let mut head = vec![Span::styled(
                e.name().to_string(),
                t.fg(t.cyan).add_modifier(Modifier::BOLD),
            )];
            if let Some(k) = &e.kind {
                head.push(Span::styled(format!("  {k}"), t.fg(t.amber)));
            }
            if let Some(d) = &e.default {
                head.push(Span::styled("  default ", t.fg(t.dim)));
                head.push(Span::styled(d.clone(), t.fg(t.muted)));
            }
            lines.push(Line::from(head));
            lines.push(Line::raw(""));
        }
        for (i, p) in e.text.iter().enumerate() {
            let base = match i {
                0 => Style::new().add_modifier(Modifier::BOLD),
                _ => Style::new(),
            };
            lines.push(inline(p, base, t));
            lines.push(Line::raw(""));
        }
        if !e.example.is_empty() {
            lines.push(Line::styled("Example", t.heading()));
            for l in &e.example {
                lines.push(yaml_line(l, t));
            }
            lines.push(Line::raw(""));
        }
        if !e.rules.is_empty() {
            lines.push(Line::styled("Rules", t.heading()));
            for r in &e.rules {
                let mut l = inline(r, Style::new(), t);
                l.spans.insert(0, Span::styled("  • ", t.fg(t.pink)));
                lines.push(l);
            }
            lines.push(Line::raw(""));
        }
        if !children.is_empty() {
            lines.extend(self.children_text(children).lines);
        }
        if let Some(f) = &self.file
            && !e.is_topic()
        {
            lines.extend(self.file_lines(f, e));
        }
        if !e.see.is_empty() {
            let mut spans = vec![Span::styled("See also  ", t.heading())];
            for (i, s) in e.see.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" · ", t.fg(t.dim)));
                }
                let label = find(s).map_or(s.clone(), |x| match x.is_topic() {
                    true => x.name().to_string(),
                    false => x.path.clone(),
                });
                spans.push(Span::styled(label, t.fg(t.cyan)));
            }
            lines.push(Line::from(spans));
            lines.push(Line::styled(
                "  (Enter while reading goes to the first)",
                t.fg(t.dim),
            ));
            lines.push(Line::raw(""));
        }
        if let Some(url) = e.docs_url() {
            lines.push(Line::from(vec![
                Span::styled("Docs  ", t.heading()),
                Span::styled(url, t.fg(t.cyan).add_modifier(Modifier::UNDERLINED)),
            ]));
        }
        Text::from(lines)
    }

    /// "In your file", "Per stage" and "Problems" for a key.
    fn file_lines(&self, f: &FileInfo, e: &Entry) -> Vec<Line<'static>> {
        let t = self.theme;
        let mut lines = vec![Line::styled("In your file", t.heading())];
        let places = f.places(e);
        if places.is_empty() {
            lines.push(Line::styled(
                match &e.default {
                    Some(d) => format!("  not set: {d}"),
                    None => "  not set".to_string(),
                },
                t.fg(t.dim),
            ));
        }
        for p in places {
            lines.push(Line::from(vec![
                Span::styled(format!("  {}", p.path), t.fg(t.muted)),
                Span::styled(": ", t.fg(t.dim)),
                Span::styled(p.value, t.fg(t.green)),
            ]));
        }
        let values = f.values(e);
        if !values.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("Per stage (resolved)", t.heading()));
            let stage_w = values.iter().map(|v| v.stage.len()).max().unwrap_or(0);
            let wl_w = values
                .iter()
                .map(|v| v.workload.as_deref().map_or(0, str::len))
                .max()
                .unwrap_or(0);
            for v in values {
                let mut spans = vec![Span::styled(
                    format!("  {:stage_w$}  ", v.stage),
                    t.fg(t.violet),
                )];
                if let Some(w) = v.workload {
                    spans.push(Span::styled(format!("{w:wl_w$}  "), t.fg(t.muted)));
                }
                spans.push(Span::styled(v.value, t.fg(t.green)));
                lines.push(Line::from(spans));
            }
        }
        let issues = f.issues(e);
        if !issues.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Problems",
                t.fg(t.red).add_modifier(Modifier::BOLD),
            ));
            for (stage, sev, i) in issues {
                let color = if sev == "error" { t.red } else { t.amber };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {} ", if sev == "error" { "✗" } else { "!" }),
                        t.fg(color),
                    ),
                    Span::styled(format!("{stage} · {}: ", i.path), t.fg(t.muted)),
                    Span::raw(i.message),
                ]));
            }
        }
        lines.push(Line::raw(""));
        lines
    }

    fn children_text(&self, children: &[usize]) -> Text<'static> {
        let t = self.theme;
        let mut lines = vec![Line::styled("Keys", t.heading())];
        let w = children
            .iter()
            .map(|c| self.nodes[*c].label.chars().count())
            .max()
            .unwrap_or(0);
        for c in children {
            let n = &self.nodes[*c];
            let summary = match n.entry {
                Some(e) => first_sentence(e.summary()),
                None => format!("{} key(s)", n.children.len()),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {:w$}  ", n.label), t.fg(t.cyan)),
                Span::styled(summary, t.fg(t.muted)),
            ]));
        }
        lines.push(Line::raw(""));
        Text::from(lines)
    }

    fn file_text(&self) -> Text<'static> {
        let t = self.theme;
        let Some(f) = &self.file else {
            return Text::default();
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled("File  ", t.heading()),
                Span::styled(f.path.display().to_string(), t.fg(t.green)),
            ]),
            Line::raw(""),
        ];
        if let Some(err) = &f.error {
            lines.push(Line::styled(
                "runway cannot read it:",
                t.fg(t.red).add_modifier(Modifier::BOLD),
            ));
            for l in err.lines() {
                lines.push(Line::raw(format!("  {l}")));
            }
            return Text::from(lines);
        }
        lines.push(inline(
            "Keys marked ● are set in this file (◦: something below them is). Their explanation ends with where they are set and their value in each stage. `f` shows only them, `n` jumps to the next problem.",
            Style::new(),
            t,
        ));
        lines.push(Line::raw(""));
        for s in &f.stages {
            let ok = s.errors.is_empty();
            lines.push(Line::from(vec![
                Span::styled(
                    if ok { "✓ " } else { "✗ " },
                    t.fg(if ok { t.green } else { t.red }),
                ),
                Span::styled(format!("stage {}", s.name), t.heading()),
            ]));
            for w in &s.workloads {
                lines.push(Line::from(vec![
                    Span::styled(format!("    {:8}", w.kind), t.fg(t.dim)),
                    Span::styled(w.id.clone(), t.fg(t.cyan)),
                ]));
            }
            for (sev, list, color) in [("✗", &s.errors, t.red), ("!", &s.warnings, t.amber)] {
                for i in list {
                    lines.push(Line::from(vec![
                        Span::styled(format!("    {sev} "), t.fg(color)),
                        Span::styled(format!("{}: ", i.path), t.fg(t.muted)),
                        Span::raw(i.message.clone()),
                    ]));
                }
            }
            lines.push(Line::raw(""));
        }
        Text::from(lines)
    }

    fn draw_help(&self, f: &mut Frame) {
        let t = self.theme;
        // (keys, what they do); an empty key starts a section.
        let keys = [
            ("", "In the keys"),
            ("↑ ↓  j k", "move"),
            (
                "→ l",
                "unfold; on an open or plain key, read its explanation",
            ),
            ("Enter Tab", "read the explanation"),
            ("← h", "fold, then go to the parent"),
            ("Space", "fold or unfold"),
            ("Esc", "clear the search, then quit"),
            ("", "In the explanation"),
            ("↑ ↓  j k", "scroll"),
            ("g G", "top, bottom"),
            ("← h  Esc Tab", "back to the keys"),
            ("Enter", "go to the first \"see also\""),
            ("", "Anywhere"),
            (
                "PgDn PgUp",
                "scroll the explanation a page (Ctrl-d, Ctrl-u: half)",
            ),
            (
                "/",
                "search keys and explanations (Enter keeps, Esc clears)",
            ),
            ("f", "only the keys your runway.yaml sets"),
            ("n", "next key with a problem"),
            ("q", "quit"),
        ];
        let mut lines = vec![Line::raw("")];
        for (k, what) in keys {
            if k.is_empty() {
                lines.push(Line::styled(format!(" {what}"), t.heading()));
                continue;
            }
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {k:18}"),
                    t.fg(t.cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(what),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  runway explain KEY prints one key; -o json for scripts.",
            t.fg(t.dim),
        ));
        lines.push(Line::styled("  Any key closes this.", t.fg(t.dim)));
        let area = centered(f.area(), 84, lines.len() as u16 + 2);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(t.fg(t.violet))
                    .title(Span::styled(" Keys ", t.heading())),
            ),
            area,
        );
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn first_sentence(s: &str) -> String {
    let end = s.find(". ").map_or(s.len(), |i| i + 1);
    s[..end].replace('`', "")
}

/// Text with `code` spans highlighted.
fn inline(s: &str, base: Style, t: Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, part) in s.split('`').enumerate() {
        if part.is_empty() {
            continue;
        }
        spans.push(match i % 2 {
            1 => Span::styled(part.to_string(), base.patch(t.fg(t.green))),
            _ => Span::styled(part.to_string(), base),
        });
    }
    Line::from(spans)
}

/// An example line: keys, values and comments in their colors.
fn yaml_line(l: &str, t: Theme) -> Line<'static> {
    let (code, comment) = match l
        .find(" #")
        .or_else(|| l.trim_start().starts_with('#').then_some(0))
    {
        Some(i) => (&l[..i], &l[i..]),
        None => (l, ""),
    };
    let mut spans = vec![Span::raw("  ")];
    let trimmed = code.trim_start();
    let indent = &code[..code.len() - trimmed.len()];
    let (dash, rest) = match trimmed.strip_prefix("- ") {
        Some(r) => ("- ", r),
        None => ("", trimmed),
    };
    spans.push(Span::raw(indent.to_string()));
    spans.push(Span::styled(dash, t.fg(t.pink)));
    match rest
        .split_once(':')
        .filter(|(k, _)| !k.contains(['{', '[', '"', ' ']))
    {
        Some((k, v)) => {
            spans.push(Span::styled(k.to_string(), t.fg(t.cyan)));
            spans.push(Span::styled(":", t.fg(t.dim)));
            spans.push(Span::styled(v.to_string(), t.fg(t.amber)));
        }
        None => spans.push(Span::styled(rest.to_string(), t.fg(t.amber))),
    }
    if !comment.is_empty() {
        spans.push(Span::styled(comment.to_string(), t.fg(t.dim)));
    }
    Line::from(spans)
}

/// Runs the browser until `q`.
pub fn run(file: Option<FileInfo>, color: bool) -> Result<()> {
    let mut app = App::new(file, color);
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    res
}

fn event_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B::Error: std::fmt::Display,
{
    while !app.quit {
        terminal
            .draw(|f| app.draw(f))
            .map_err(|e| Error::internal(format!("drawing the terminal: {e}")))?;
        match event::read().map_err(|e| Error::internal(format!("reading the terminal: {e}")))? {
            Event::Key(k) => app.on_key(k),
            Event::Resize(..) => {}
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn screen(app: &mut App) -> String {
        screen_at(app, 120, 30)
    }

    fn screen_at(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn it_opens_on_start_here_with_the_top_level_keys() {
        let mut app = App::new(None, false);
        let s = screen(&mut app);
        assert!(s.contains("runway explain"), "{s}");
        assert!(s.contains("Start here"));
        assert!(s.contains("provider") && s.contains("service") && s.contains("stages"));
        assert!(s.contains("version: 1"), "the example is shown");
    }

    #[test]
    fn search_finds_a_key_and_shows_it() {
        let mut app = App::new(None, false);
        app.on_key(key(KeyCode::Char('/')));
        for c in "egress".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        let s = screen(&mut app);
        assert!(s.contains("service.vpc.egress"), "{s}");
        assert!(s.contains("private-ranges-only"));
        app.on_key(key(KeyCode::Esc));
        assert!(!app.filtering());
        assert!(!app.quit, "Esc first clears the search");
    }

    #[test]
    fn folding_and_following_see_also() {
        let mut app = App::new(None, false);
        assert!(app.go_to("defaults"));
        app.on_key(key(KeyCode::Enter));
        assert!(app.reading, "Enter reads the explanation");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.selected_node()
                .and_then(|id| app.nodes[id].entry)
                .map(|e| e.path.as_str()),
            Some("service"),
            "defaults leads to service"
        );
        assert!(!app.reading, "back in the keys");
        app.on_key(key(KeyCode::Right));
        let before = app.visible.len();
        assert!(!app.reading, "→ first unfolds");
        app.on_key(key(KeyCode::Left));
        assert!(app.visible.len() < before, "folded");
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.visible.len(), before, "Space unfolds");
        app.on_key(key(KeyCode::Char('?')));
        assert!(screen(&mut app).contains("In the explanation"));
        app.on_key(key(KeyCode::Char('x')));
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn reading_scrolls_the_explanation_and_stops_at_its_end() {
        let mut app = App::new(None, false);
        screen(&mut app);
        let selected = app.list.selected();
        app.on_key(key(KeyCode::Right));
        assert!(app.reading, "→ on a plain key reads it");
        assert!(app.max_scroll > 0, "Start here is longer than the screen");
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.scroll, 2);
        assert_eq!(app.list.selected(), selected, "the selection stays");
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.scroll, 1);
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(app.scroll, app.max_scroll);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.scroll, app.max_scroll, "no scrolling past the end");
        let s = screen(&mut app);
        assert!(s.contains("Docs"), "the end is on screen: {s}");
        app.on_key(key(KeyCode::Esc));
        assert!(!app.reading && !app.quit, "Esc goes back to the keys");
        app.on_key(key(KeyCode::Down));
        assert_ne!(app.list.selected(), selected, "↓ moves again");
        assert_eq!(app.scroll, 0, "a new key starts at the top");
    }

    #[test]
    fn the_end_of_a_long_explanation_is_reachable_on_a_small_terminal() {
        for path in [":layers", ":variables", "service.vpc"] {
            let mut app = App::new(None, false);
            assert!(app.go_to(path), "{path}");
            screen_at(&mut app, 80, 24);
            app.on_key(key(KeyCode::Enter));
            app.on_key(key(KeyCode::End));
            let s = screen_at(&mut app, 80, 24);
            // The last line of every explanation is its docs link (wrapped
            // at this width): the right panel's text, rows joined, has it.
            let url = find(path).and_then(|e| e.docs_url()).unwrap();
            let right: String = s
                .lines()
                .flat_map(|row| row.chars().skip(29))
                .filter(|c| !c.is_whitespace() && !"│█╭╮╰╯─↑↓↕".contains(*c))
                .collect();
            assert!(right.contains(&url), "{path}: end not reached:\n{s}");
            let at_end = app.scroll;
            app.on_key(key(KeyCode::Down));
            assert_eq!(app.scroll, at_end, "{path}: nothing below the end");
        }
    }

    #[test]
    fn service_keys_are_grouped() {
        let mut app = App::new(None, false);
        assert!(app.go_to("service.memory"));
        let id = app.selected_node().unwrap();
        let group = app.nodes[id].parent.unwrap();
        assert_eq!(app.nodes[group].label, "Resources and scaling");
        assert_eq!(app.nodes[app.nodes[group].parent.unwrap()].label, "service");
    }

    #[test]
    fn the_file_is_explained_with_its_values() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(
            &p,
            "version: 1\napp: shop\nprovider: {project: my-gcp-project, region: europe-west1}\nservice:\n  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop:1\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\n  memory: 1Gi\nstages:\n  dev: {}\n  prod:\n    service: {memory: 99Ti}\n",
        )
        .unwrap();
        let mut app = App::new(FileInfo::load(&p), false);
        let s = screen(&mut app);
        assert!(s.contains("app shop") && s.contains("problem"), "{s}");
        app.on_key(key(KeyCode::Char('n')));
        let s = screen(&mut app);
        assert!(s.contains("service.memory"), "{s}");
        assert!(s.contains("stages.prod.service.memory: 99Ti"), "{s}");
        assert!(
            s.contains("shop-dev") && s.contains("1Gi"),
            "resolved dev value: {s}"
        );
        app.on_key(key(KeyCode::Char('f')));
        assert!(app.visible.iter().all(|id| app.in_file(*id)));
    }
}
