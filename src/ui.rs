use crate::{
    model::{Filter, Tree, Usage, display_name, human},
    scan::{Progress, ScanOptions, scan},
};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Directories,
    Groups,
    Users,
}

impl View {
    fn next(self) -> Self {
        match self {
            Self::Directories => Self::Groups,
            Self::Groups => Self::Users,
            Self::Users => Self::Directories,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Node(usize),
    Owner(u32),
    SelfUsage,
}

struct Entry {
    item: Item,
    name: String,
    usage: Usage,
}

pub struct App {
    tree: Tree,
    current: usize,
    view: View,
    filter: Filter,
    apparent: bool,
    sort_name: bool,
    selected: usize,
    offset: usize,
    rows: Vec<Entry>,
    path: String,
    names: HashMap<(bool, u32), String>,
    overlay: Option<bool>, // true: help, false: errors
    error_scroll: u16,
    directory_focus: bool,
    directory_selected: usize,
    directory_offset: usize,
    owner_scope: usize,
}

impl App {
    pub fn new(tree: Tree, filter: Filter, view: View, apparent: bool) -> Self {
        let mut app = Self {
            tree,
            current: 0,
            view,
            filter,
            apparent,
            sort_name: false,
            selected: 0,
            offset: 0,
            rows: vec![],
            path: String::new(),
            names: HashMap::new(),
            overlay: None,
            error_scroll: 0,
            directory_focus: false,
            directory_selected: 0,
            directory_offset: 0,
            owner_scope: 0,
        };
        app.rebuild();
        app
    }

    fn owner_label(&self, id: u32) -> String {
        self.names
            .get(&(self.view == View::Groups, id))
            .map_or_else(
                || crate::platform::owner_identifier(id).unwrap_or_else(|| id.to_string()),
                |name| format!("{name} ({id})"),
            )
    }

    // Reopening a selector must allow replacing its current owner filter.
    fn selector_filter(&self) -> Filter {
        match self.view {
            View::Groups => Filter {
                gid: None,
                ..self.filter
            },
            View::Users => Filter {
                uid: None,
                ..self.filter
            },
            View::Directories => self.filter,
        }
    }

    fn preview_filter(&self) -> Option<Filter> {
        let Item::Owner(id) = self.rows.get(self.selected)?.item else {
            return None;
        };
        let mut filter = self.selector_filter();
        if self.view == View::Groups {
            filter.gid = Some(id);
        } else {
            filter.uid = Some(id);
        }
        Some(filter)
    }

    fn subdirectories(&self, filter: Filter) -> Vec<Entry> {
        let mut entries: Vec<_> = self.tree.nodes[self.current]
            .children
            .iter()
            .filter(|&&id| self.tree.nodes[id].is_dir)
            .map(|&id| Entry {
                item: Item::Node(id),
                name: format!("{}/", display_name(&self.tree.nodes[id].name)),
                usage: self.tree.nodes[id].usage(filter),
            })
            .collect();
        entries.sort_by(|a, b| {
            if self.sort_name {
                a.name.cmp(&b.name)
            } else {
                b.usage
                    .bytes(self.apparent)
                    .cmp(&a.usage.bytes(self.apparent))
                    .then(a.name.cmp(&b.name))
            }
        });
        entries
    }

    fn focus_directories(&mut self) {
        if let Some(filter) = self.preview_filter() {
            self.filter = filter;
            self.directory_focus = true;
        }
    }

    // Two independent cursors: owner selection remains fixed while browsing.
    fn picker_key(&mut self, key: KeyEvent) -> bool {
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            if self.directory_focus {
                self.directory_focus = false;
            } else {
                self.focus_directories();
            }
            return true;
        }
        if !self.directory_focus {
            if matches!(
                key.code,
                KeyCode::Right | KeyCode::Enter | KeyCode::Char('l')
            ) {
                self.focus_directories();
                return true;
            }
            return matches!(
                key.code,
                KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h')
            );
        }
        let Some(filter) = self.preview_filter() else {
            return false;
        };
        let entries = self.subdirectories(filter);
        self.directory_selected = self.directory_selected.min(entries.len().saturating_sub(1));
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.directory_selected =
                    (self.directory_selected + 1).min(entries.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.directory_selected = self.directory_selected.saturating_sub(1)
            }
            KeyCode::Home => self.directory_selected = 0,
            KeyCode::End => self.directory_selected = entries.len().saturating_sub(1),
            KeyCode::PageDown => {
                self.directory_selected =
                    (self.directory_selected + 20).min(entries.len().saturating_sub(1))
            }
            KeyCode::PageUp => self.directory_selected = self.directory_selected.saturating_sub(20),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(Entry {
                    item: Item::Node(id),
                    ..
                }) = entries.get(self.directory_selected)
                {
                    self.current = *id;
                    self.directory_selected = 0;
                    self.directory_offset = 0;
                    self.rebuild();
                }
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                if self.current == self.owner_scope {
                    self.directory_focus = false;
                } else if let Some(parent) = self.tree.nodes[self.current].parent {
                    let previous = self.current;
                    self.current = parent;
                    self.rebuild();
                    self.directory_selected = self
                        .subdirectories(filter)
                        .iter()
                        .position(|e| e.item == Item::Node(previous))
                        .unwrap_or(0);
                    self.directory_offset = 0;
                }
            }
            _ => return false,
        }
        true
    }

    fn draw_owner_picker(&mut self, frame: &mut Frame, area: Rect) {
        let panes = if area.width >= 100 {
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area)
        } else {
            Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area)
        };
        let height = panes[0].height.saturating_sub(3).max(1) as usize;
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        let owner = if self.view == View::Groups {
            "group"
        } else {
            "user"
        };
        let rows = self
            .rows
            .iter()
            .skip(self.offset)
            .take(height)
            .map(|entry| Row::new([human(entry.usage.bytes(self.apparent)), entry.name.clone()]));
        let table = Table::new(rows, [Constraint::Length(13), Constraint::Min(12)])
            .header(
                Row::new(["Total", "Owner"]).style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Choose {owner} · → directories "))
                    .border_style(Style::default().fg(if self.directory_focus {
                        Color::DarkGray
                    } else {
                        Color::Cyan
                    })),
            )
            .row_highlight_style(
                Style::default()
                    .bg(if self.directory_focus {
                        Color::Black
                    } else {
                        Color::DarkGray
                    })
                    .fg(Color::White),
            )
            .highlight_symbol(if self.directory_focus { "  " } else { "› " });
        let mut state = TableState::default();
        if !self.rows.is_empty() {
            state.select(Some(self.selected - self.offset));
        }
        frame.render_stateful_widget(table, panes[0], &mut state);
        if let Some(filter) = self.preview_filter() {
            let entries = self.subdirectories(filter);
            let height = panes[1].height.saturating_sub(3).max(1) as usize;
            self.directory_selected = self.directory_selected.min(entries.len().saturating_sub(1));
            if self.directory_selected < self.directory_offset {
                self.directory_offset = self.directory_selected;
            }
            if self.directory_selected >= self.directory_offset + height {
                self.directory_offset = self.directory_selected + 1 - height;
            }
            let rows = entries
                .iter()
                .skip(self.directory_offset)
                .take(height)
                .map(|entry| {
                    Row::new([human(entry.usage.bytes(self.apparent)), entry.name.clone()])
                });
            let selected = &self.rows[self.selected].name;
            let table = Table::new(rows, [Constraint::Length(13), Constraint::Min(12)])
                .header(
                    Row::new(["Size", "Subdirectory"])
                        .style(Style::default().add_modifier(Modifier::BOLD)),
                )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(
                            " {selected} · {} dirs · Tab owners ",
                            entries.len()
                        ))
                        .border_style(Style::default().fg(if self.directory_focus {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        })),
                )
                .row_highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
                .highlight_symbol("› ");
            let mut directory_state = TableState::default();
            if self.directory_focus && !entries.is_empty() {
                directory_state.select(Some(self.directory_selected - self.directory_offset));
            }
            frame.render_stateful_widget(table, panes[1], &mut directory_state);
            if entries.is_empty() {
                let inner = Block::default().borders(Borders::ALL).inner(panes[1]);
                frame.render_widget(
                    Paragraph::new(
                        "No subdirectories. d shows files; ← returns; Tab selects owner.",
                    ),
                    inner,
                );
            }
        } else {
            frame.render_widget(
                Paragraph::new("No owners match the other active filter. Esc clears filters."),
                panes[1],
            );
        }
    }

    fn rebuild(&mut self) {
        let owner = if self.view != View::Directories {
            self.rows.get(self.selected).map(|entry| entry.item)
        } else {
            None
        };
        self.path = display_name(self.tree.path(self.current).as_os_str());
        let node = &self.tree.nodes[self.current];
        self.rows = match self.view {
            View::Directories => {
                let mut entries: Vec<_> = node
                    .children
                    .iter()
                    .filter_map(|&id| {
                        let child = &self.tree.nodes[id];
                        let usage = child.usage(self.filter);
                        if !self.filter.is_empty() && usage.inodes == 0 && !child.is_dir {
                            return None;
                        }
                        let mut name = display_name(&child.name);
                        if child.is_dir {
                            name.push('/');
                        }
                        if child.is_symlink {
                            name.push_str(" @");
                        }
                        if child.shared_link {
                            name.push_str(" [hard link: counted elsewhere]");
                        }
                        Some(Entry {
                            item: Item::Node(id),
                            name,
                            usage,
                        })
                    })
                    .collect();
                if self.filter.matches(node.uid, node.gid) {
                    entries.push(Entry {
                        item: Item::SelfUsage,
                        name: "[directory itself]".into(),
                        usage: node.own,
                    });
                }
                entries
            }
            _ => self
                .tree
                .ownership(
                    self.owner_scope,
                    self.view == View::Groups,
                    self.selector_filter(),
                )
                .into_iter()
                .map(|(id, usage)| Entry {
                    item: Item::Owner(id),
                    name: self.owner_label(id),
                    usage,
                })
                .collect(),
        };
        let apparent = self.apparent;
        let sort_name = self.sort_name;
        self.rows.sort_by(|a, b| {
            if sort_name {
                a.name.cmp(&b.name)
            } else {
                b.usage
                    .bytes(apparent)
                    .cmp(&a.usage.bytes(apparent))
                    .then(a.name.cmp(&b.name))
            }
        });
        if let Some(owner) = owner
            && let Some(index) = self.rows.iter().position(|entry| entry.item == owner)
        {
            self.selected = index;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.offset = self.offset.min(self.selected);
    }

    /// Returns true when the user requests quit.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return true;
        }
        if self.overlay.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('e') => self.overlay = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.error_scroll = self.error_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.error_scroll = self.error_scroll.saturating_sub(1)
                }
                _ => {}
            }
            return false;
        }
        if self.view != View::Directories && self.picker_key(key) {
            return false;
        }
        let old_selected = self.selected;
        let mut changed = false;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::PageDown => {
                self.selected = (self.selected + 20).min(self.rows.len().saturating_sub(1))
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(20),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.rows.len().saturating_sub(1),
            KeyCode::Tab => {
                self.view = self.view.next();
                self.owner_scope = self.current;
                self.directory_focus = false;
                self.directory_selected = 0;
                self.directory_offset = 0;
                self.selected = 0;
                changed = true;
            }
            KeyCode::Char('g') => {
                self.view = View::Groups;
                self.owner_scope = self.current;
                self.directory_focus = false;
                self.directory_selected = 0;
                self.directory_offset = 0;
                self.selected = 0;
                changed = true;
            }
            KeyCode::Char('u') => {
                self.view = View::Users;
                self.owner_scope = self.current;
                self.directory_focus = false;
                self.directory_selected = 0;
                self.directory_offset = 0;
                self.selected = 0;
                changed = true;
            }
            KeyCode::Char('d') => {
                if let Some(filter) = self.preview_filter() {
                    self.filter = filter;
                }
                self.view = View::Directories;
                self.selected = 0;
                changed = true;
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(entry) = self.rows.get(self.selected) {
                    match entry.item {
                        Item::Node(id) if self.tree.nodes[id].is_dir => {
                            self.current = id;
                            self.selected = 0;
                            changed = true;
                        }
                        Item::Owner(id) => {
                            if self.view == View::Groups {
                                self.filter.gid = Some(id);
                            } else {
                                self.filter.uid = Some(id);
                            }
                            self.view = View::Directories;
                            self.selected = 0;
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                if let Some(parent) = self.tree.nodes[self.current].parent {
                    let previous = self.current;
                    self.current = parent;
                    self.rebuild();
                    self.selected = self
                        .rows
                        .iter()
                        .position(|r| r.item == Item::Node(previous))
                        .unwrap_or(0);
                    self.offset = 0;
                }
            }
            KeyCode::Esc => {
                self.view = View::Directories;
                self.directory_focus = false;
                self.filter = Filter::default();
                self.selected = 0;
                changed = true;
            }
            KeyCode::Char('a') => {
                self.apparent = !self.apparent;
                changed = true;
            }
            KeyCode::Char('s') => {
                self.sort_name = !self.sort_name;
                changed = true;
            }
            KeyCode::Char('?') => {
                self.overlay = Some(true);
                self.error_scroll = 0;
            }
            KeyCode::Char('e') => {
                self.overlay = Some(false);
                self.error_scroll = 0;
            }
            _ => {}
        }
        if self.view != View::Directories && self.selected != old_selected {
            self.directory_selected = 0;
            self.directory_offset = 0;
        }
        if changed {
            self.offset = 0;
            self.rebuild();
        }
        false
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(frame.area());
        let incomplete = if self.tree.error_count > 0 {
            " | INCOMPLETE"
        } else {
            ""
        };
        let title = format!(
            " zhandu | {:?} | {}{} ",
            self.view,
            if self.apparent {
                "apparent size"
            } else {
                "allocated disk usage"
            },
            incomplete
        );
        frame.render_widget(
            Paragraph::new(self.path.as_str())
                .block(Block::default().borders(Borders::ALL).title(title)),
            areas[0],
        );
        if let Some(help) = self.overlay {
            let text = if help {
                "↑/↓ or j/k: select     →/Enter: open directory / filter owner\n←/Backspace: parent     Tab: switch panels (in owner view)\ng: choose group   u: choose user   d: directories   Esc: clear BOTH filters\na: allocated / apparent size    s: size / name sort\nHome/End, PageUp/PageDown: move   e: errors   ?: help   q: quit\n\nOwner panel: →/Enter focuses directories. ↑↓ selects within the focused panel.\nDirectory panel: →/Enter opens, ← goes up (at starting path: owners).\nTab switches focus; d opens full file listing with the same filter.\nOwners: Unix UID/GID or Windows owner/primary-group SID. Filters combine.\nTotals include directory blocks and symlinks, and include hidden files.\nHard links count once at their first sorted depth-first scan location.\n@ marks a symlink (not followed). Allocated bytes are not reclaimable bytes.\n\nEsc closes this panel. No files are modified or deleted.".to_owned()
            } else if self.tree.errors.is_empty() {
                "No scan errors.".to_owned()
            } else {
                format!(
                    "{} errors (first {} shown). Up/down scroll.\n\n{}",
                    self.tree.error_count,
                    self.tree.errors.len(),
                    self.tree.errors.join("\n")
                )
            };
            frame.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((self.error_scroll, 0))
                    .block(Block::default().borders(Borders::ALL).title(if help {
                        " Help "
                    } else {
                        " Scan errors "
                    })),
                areas[1],
            );
        } else if self.view != View::Directories {
            self.draw_owner_picker(frame, areas[1]);
        } else {
            let height = areas[1].height.saturating_sub(3).max(1) as usize;
            if self.selected < self.offset {
                self.offset = self.selected;
            }
            if self.selected >= self.offset + height {
                self.offset = self.selected + 1 - height;
            }
            let total = self.tree.nodes[self.current]
                .usage(self.filter)
                .bytes(self.apparent);
            let rows = self
                .rows
                .iter()
                .skip(self.offset)
                .take(height)
                .map(|entry| {
                    let bytes = entry.usage.bytes(self.apparent);
                    let percent = if total == 0 {
                        0.0
                    } else {
                        bytes as f64 / total as f64 * 100.0
                    };
                    Row::new(vec![
                        Cell::from(human(bytes)),
                        Cell::from(format!("{percent:5.1}%")),
                        Cell::from(entry.usage.inodes.to_string()),
                        Cell::from(entry.name.as_str()),
                    ])
                });
            let table = Table::new(
                rows,
                [
                    Constraint::Length(13),
                    Constraint::Length(8),
                    Constraint::Length(10),
                    Constraint::Min(12),
                ],
            )
            .header(
                Row::new(["Size", "%", "Inodes", "Name"])
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} entries ", self.rows.len())),
            )
            .row_highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
            .highlight_symbol("› ");
            let mut state = TableState::default();
            if !self.rows.is_empty() {
                state.select(Some(self.selected - self.offset));
            }
            frame.render_stateful_widget(table, areas[1], &mut state);
        }
        let active = self.preview_filter().unwrap_or(self.filter);
        let usage = self.tree.nodes[self.current].usage(active);
        let filter = format!(
            "UID={} GID={}",
            active.uid.map_or("all".into(), |id| id.to_string()),
            active.gid.map_or("all".into(), |id| id.to_string())
        );
        let status = format!(
            "Total {} | {} inodes | {} | scan {:.2}s | {} errors\n↑↓ select  → enter  ← parent  Tab panels  Esc clear  a size  s sort  e errors  ? help  q quit",
            human(usage.bytes(self.apparent)),
            usage.inodes,
            filter,
            self.tree.elapsed_seconds,
            self.tree.error_count
        );
        frame.render_widget(
            Paragraph::new(status)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL)),
            areas[2],
        );
    }
}

pub fn run(
    options: ScanOptions,
    progress: Arc<Progress>,
    filter: Filter,
    view: View,
    apparent: bool,
    numeric: bool,
) -> io::Result<i32> {
    // Ratatui's guard restores terminal modes on return, errors, and panics.
    ratatui::run(|terminal| -> io::Result<i32> {
        let root_label = display_name(options.path.as_os_str());
        let start = Instant::now();
        let (sender, receiver) = mpsc::channel();
        let worker_progress = progress.clone();
        std::thread::spawn(move || {
            let result = scan(options, worker_progress);
            let _ = sender.send(result);
        });
        let tree = loop {
            if progress.is_cancelled() {
                return Ok(130);
            }
            match receiver.try_recv() {
                Ok(result) => break result?,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other("scanner stopped unexpectedly"));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            terminal.draw(|frame| {
                let phase = if progress.phase.load(Ordering::Relaxed) == 1 { "Aggregating ownership totals" } else { "Scanning" };
                let text = format!("{phase}: {root_label}\n\n{} entries inspected | {} errors | {:.1}s\n\nResults will open when scanning finishes. q / Esc / Ctrl-C cancels.",
                    progress.entries.load(Ordering::Relaxed), progress.errors.load(Ordering::Relaxed), start.elapsed().as_secs_f64());
                frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).title(" zhandu ")), frame.area());
            })?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
                && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)))
            {
                progress.cancel();
                return Ok(130);
            }
        };
        let exit_code = if tree.error_count == 0 { 0 } else { 2 };
        let (names_tx, names_rx) = mpsc::channel();
        if !numeric {
            let ids: Vec<_> = tree.nodes[0].owners.keys().copied().collect();
            std::thread::spawn(move || {
                let mut seen = std::collections::HashSet::new();
                for (uid, gid) in ids {
                    for (group, id) in [(false, uid), (true, gid)] {
                        if !seen.insert((group, id)) {
                            continue;
                        }
                        let name = crate::platform::owner_name(id, group);
                        if let Some(name) = name
                            && names_tx.send(((group, id), name)).is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
        let mut app = App::new(tree, filter, view, apparent);
        let mut dirty = true;
        loop {
            if progress.is_cancelled() {
                return Ok(130);
            }
            let mut names_changed = false;
            for (id, name) in names_rx.try_iter() {
                app.names.insert(id, name);
                names_changed = true;
            }
            if names_changed && app.view != View::Directories {
                let selected = app.rows.get(app.selected).map(|entry| entry.item);
                app.rebuild();
                if let Some(selected) = selected {
                    app.selected = app
                        .rows
                        .iter()
                        .position(|e| e.item == selected)
                        .unwrap_or(0);
                }
                dirty = true;
            }
            if dirty {
                terminal.draw(|frame| app.draw(frame))?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Release
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            return Ok(130);
                        }
                        if app.key(key) {
                            return Ok(exit_code);
                        }
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn directory_panel_scrolls_and_keeps_its_cursor_when_returning_to_parent() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..50 {
            std::fs::create_dir(dir.path().join(format!("dir{index:02}"))).unwrap();
        }
        let tree = scan(
            ScanOptions {
                path: dir.path().into(),
                threads: 2,
                one_file_system: false,
                excludes: vec![],
            },
            Arc::default(),
        )
        .unwrap();
        let mut app = App::new(tree, Filter::default(), View::Groups, false);
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.key(press(KeyCode::Right));
        app.key(press(KeyCode::End));
        let selected = app.directory_selected;
        assert_eq!(selected, 49);
        let owner_cursor = app.selected;
        let mut terminal = Terminal::new(TestBackend::new(120, 15)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.directory_offset > 0);
        let chosen = app.filter;
        app.key(press(KeyCode::Enter));
        assert_ne!(app.current, 0);
        app.key(press(KeyCode::Right)); // empty directory: harmless
        assert_eq!(app.filter, chosen);
        assert_eq!(app.selected, owner_cursor);
        app.key(press(KeyCode::Left));
        assert_eq!(app.current, 0);
        assert_eq!(app.directory_selected, selected);
        app.key(press(KeyCode::Left)); // at the starting path: focus owners
        assert!(!app.directory_focus);
        app.key(press(KeyCode::Tab));
        assert!(app.directory_focus);
        assert_eq!(app.directory_selected, selected);
    }

    #[test]
    fn owner_picker_previews_subdirectories_and_keeps_filter_during_navigation() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name).join("data"), b"data").unwrap();
        }
        let mut tree = scan(
            ScanOptions {
                path: dir.path().into(),
                threads: 2,
                one_file_system: false,
                excludes: vec![],
            },
            Arc::default(),
        )
        .unwrap();
        // Synthetic ownership avoids requiring privileged UID changes.
        for id in 0..tree.nodes.len() {
            let beta_file =
                !tree.nodes[id].is_dir && tree.path(id).parent().unwrap().ends_with("beta");
            let node = &mut tree.nodes[id];
            node.uid = if beta_file { 202 } else { 101 };
            node.gid = if beta_file { 20 } else { 10 };
            let bytes = if node.is_dir {
                0
            } else if beta_file {
                300
            } else {
                100
            };
            node.own = Usage {
                allocated: bytes,
                apparent: bytes,
                inodes: 1,
            };
        }
        tree.aggregate();
        let mut app = App::new(tree, Filter::default(), View::Directories, false);
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for (key, owner) in [('g', 10), ('u', 101)] {
            app.key(press(KeyCode::Esc));
            app.key(press(KeyCode::Char(key)));
            app.selected = app
                .rows
                .iter()
                .position(|row| row.item == Item::Owner(owner))
                .unwrap();
            let preview = app.subdirectories(app.preview_filter().unwrap());
            assert_eq!(preview.len(), 2);
            assert_eq!(
                (preview[0].name.as_str(), preview[0].usage.allocated),
                ("alpha/", 100)
            );
            assert_eq!(
                (preview[1].name.as_str(), preview[1].usage.allocated),
                ("beta/", 0)
            );
            for (width, height) in [(120, 30), (80, 24), (20, 5), (1, 1)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                if width == 120 {
                    let buffer = terminal.backend().buffer();
                    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                    assert!(text.contains("Subdirectory"));
                    assert!(text.contains("alpha/"));
                    assert!(text.contains("beta/"));
                }
            }
            app.key(press(KeyCode::Enter));
            assert_ne!(app.view, View::Directories);
            assert!(app.directory_focus);
            assert!(
                app.subdirectories(app.filter)
                    .iter()
                    .any(|row| row.name == "beta/" && row.usage.allocated == 0)
            );
            let chosen = app.filter;
            let owner_cursor = app.selected;
            app.key(press(KeyCode::Down));
            assert_eq!(app.directory_selected, 1);
            assert_eq!(app.selected, owner_cursor);
            app.key(press(KeyCode::Up));
            assert_eq!(app.directory_selected, 0);
            app.key(press(KeyCode::Tab));
            assert!(!app.directory_focus);
            app.key(press(KeyCode::Right));
            assert!(app.directory_focus);
            app.key(press(KeyCode::Right));
            assert_eq!(app.filter, chosen);
            assert_eq!(app.tree.nodes[app.current].usage(chosen).allocated, 100);
            app.key(press(KeyCode::Left));
            assert_eq!(app.current, 0);
            assert_eq!(app.filter, chosen);
            app.key(press(KeyCode::Char(key)));
            assert_eq!(
                app.rows.len(),
                2,
                "reopening the picker must expose other owners"
            );
        }
    }

    #[test]
    fn navigate_filter_resize_and_render() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/data"), [1u8; 8192]).unwrap();
        let tree = scan(
            ScanOptions {
                path: dir.path().to_owned(),
                threads: 2,
                one_file_system: false,
                excludes: vec![],
            },
            Arc::default(),
        )
        .unwrap();
        let mut app = App::new(tree, Filter::default(), View::Directories, false);
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.selected = app.rows.iter().position(|r| r.name == "nested/").unwrap();
        app.key(press(KeyCode::Right));
        assert_ne!(app.current, 0);
        app.key(press(KeyCode::Left));
        assert_eq!(app.current, 0);
        app.key(press(KeyCode::Char('g')));
        app.key(press(KeyCode::Enter));
        assert!(app.filter.gid.is_some());
        assert_eq!(app.view, View::Groups);
        assert!(app.directory_focus);
        app.key(press(KeyCode::Char('d')));
        assert_eq!(app.view, View::Directories);
        app.key(press(KeyCode::Esc));
        assert!(app.filter.is_empty());
        for (width, height) in [(100, 24), (20, 5), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
        }
        assert!(app.key(press(KeyCode::Char('q'))));
    }
}
