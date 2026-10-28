use crate::{
    cache::{Cache, CacheConfig},
    deletion::{self, Mark, Outcome},
    model::{Filter, Tree, Usage, display_name, human},
    scan::{LiveEvent, LiveState, Progress, ScanOptions, scan},
    session::ScanSession,
};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::PathBuf,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Directories,
    Groups,
    Users,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Node(usize),
    Owner(u32),
    AllOwners,
    SelfUsage,
}

#[derive(Clone)]
struct Entry {
    item: Item,
    name: String,
    usage: Usage,
}

#[derive(Clone, Copy)]
enum Overlay {
    Help,
    Errors,
    Details,
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
    overlay: Option<Overlay>,
    details: String,
    row_total: usize,
    error_scroll: u16,
    directory_focus: bool,
    directory_selected: usize,
    directory_offset: usize,
    owner_scope: usize,
    marks: BTreeMap<PathBuf, Mark>,
    review: Option<String>,
    review_scroll: u16,
    notice: String,
    delete_requested: bool,
    busy: bool,
    deletion_errors: bool,
    scanning: bool,
    cached_refresh: bool,
    cache_age: Option<(i64, Instant)>,
    cache_was_partial: bool,
    refresh_requested: bool,
    refresh_error: Option<String>,
    hide_zero: bool,
    live_directory_anchor: Option<Item>,
    owner_search: String,
    directory_search: String,
    editing_search: bool,
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
            details: String::new(),
            row_total: 0,
            error_scroll: 0,
            directory_focus: false,
            directory_selected: 0,
            directory_offset: 0,
            owner_scope: 0,
            marks: BTreeMap::new(),
            review: None,
            review_scroll: 0,
            notice: String::new(),
            delete_requested: false,
            busy: false,
            deletion_errors: false,
            scanning: false,
            cached_refresh: false,
            cache_age: None,
            cache_was_partial: false,
            refresh_requested: false,
            refresh_error: None,
            hide_zero: false,
            live_directory_anchor: None,
            owner_search: String::new(),
            directory_search: String::new(),
            editing_search: false,
        };
        app.rebuild();
        if view != View::Directories {
            let owner = if view == View::Groups {
                filter.gid
            } else {
                filter.uid
            };
            app.selected = app
                .rows
                .iter()
                .position(|entry| entry.item == owner.map_or(Item::AllOwners, Item::Owner))
                .unwrap_or(0);
        }
        app
    }

    fn owner_label(&self, id: u32) -> String {
        self.named_owner(self.view == View::Groups, id)
    }

    fn named_owner(&self, group: bool, id: u32) -> String {
        self.names.get(&(group, id)).map_or_else(
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
        let id = match self.rows.get(self.selected)?.item {
            Item::Owner(id) => Some(id),
            Item::AllOwners => None,
            _ => return None,
        };
        let mut filter = self.selector_filter();
        if self.view == View::Groups {
            filter.gid = id;
        } else {
            filter.uid = id;
        }
        Some(filter)
    }

    fn searching_owners(&self) -> bool {
        self.view != View::Directories && !self.directory_focus
    }

    fn search_query(&self) -> &str {
        if self.searching_owners() {
            &self.owner_search
        } else {
            &self.directory_search
        }
    }

    fn search_query_mut(&mut self) -> &mut String {
        if self.searching_owners() {
            &mut self.owner_search
        } else {
            &mut self.directory_search
        }
    }

    fn update_search(&mut self) {
        self.rebuild();
        if self.searching_owners() {
            // All stays pinned, but typing a query selects its first matching owner.
            self.selected = usize::from(!self.owner_search.is_empty() && self.rows.len() > 1);
        } else if self.view == View::Directories {
            self.selected = 0;
        }
        self.offset = 0;
        self.directory_selected = 0;
        self.directory_offset = 0;
    }

    fn entry_scan(&self, item: Item) -> crate::model::ScanState {
        let id = match item {
            Item::Node(id) => id,
            Item::Owner(_) | Item::AllOwners => self.owner_scope,
            Item::SelfUsage => return crate::model::ScanState::default(),
        };
        self.tree.nodes[id].scan
    }

    fn uncertain_usage(&self, item: Item) -> bool {
        // Even an enumerated branch can receive a different hard-link attribution
        // during final deterministic reconciliation. Keep live aggregate sizes provisional.
        self.cached_refresh
            || self.entry_scan(item).uncertain()
            || (self.scanning
                && match item {
                    Item::Node(id) => self.tree.nodes[id].is_dir,
                    Item::Owner(_) | Item::AllOwners => true,
                    Item::SelfUsage => false,
                })
    }

    fn status_label(&self, item: Item) -> String {
        if self.cached_refresh {
            return if self.scanning { "cached" } else { "stale" }.into();
        }
        let state = self.entry_scan(item);
        if state.errors > 0 {
            format!("{} errors", state.errors)
        } else if self.scanning && state.pending == 0 {
            "scanned".into()
        } else {
            state.label().into()
        }
    }

    fn usage_label(&self, item: Item, usage: Usage) -> String {
        let bytes = usage.bytes(self.apparent);
        if self.uncertain_usage(item) {
            if bytes == 0 {
                "…".into()
            } else {
                format!("~{}", human(bytes))
            }
        } else {
            human(bytes)
        }
    }

    fn subdirectories(&self, filter: Filter) -> Vec<Entry> {
        let query = self.directory_search.to_lowercase();
        let mut entries: Vec<_> = self.tree.nodes[self.current]
            .children
            .iter()
            .filter(|&&id| self.tree.nodes[id].is_dir)
            .map(|&id| Entry {
                item: Item::Node(id),
                name: format!("{}/", display_name(&self.tree.nodes[id].name)),
                usage: self.tree.nodes[id].usage(filter),
            })
            .filter(|entry| {
                !self.hide_zero
                    || entry.usage.bytes(self.apparent) > 0
                    || self.uncertain_usage(entry.item)
            })
            .filter(|entry| query.is_empty() || entry.name.to_lowercase().contains(&query))
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

    fn open_picker(&mut self, view: View) {
        self.view = view;
        self.owner_scope = self.current;
        self.directory_focus = false;
        self.directory_selected = 0;
        self.directory_offset = 0;
        self.owner_search.clear();
        self.directory_search.clear();
        self.rows.clear();
        self.rebuild();
        let id = if view == View::Groups {
            self.filter.gid
        } else {
            self.filter.uid
        };
        let item = id.map_or(Item::AllOwners, Item::Owner);
        self.selected = self
            .rows
            .iter()
            .position(|entry| entry.item == item)
            .unwrap_or(0);
        self.offset = 0;
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
                    self.directory_search.clear();
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
                    self.directory_search.clear();
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
        // Keep All visible above the scrolling owner rows.
        let capacity = height.saturating_sub(1).max(1);
        self.offset = self.offset.max(1);
        if self.selected > 0 {
            if self.selected < self.offset {
                self.offset = self.selected;
            }
            if self.selected >= self.offset + capacity {
                self.offset = self.selected + 1 - capacity;
            }
        }
        let visible: Vec<usize> = std::iter::once(0)
            .chain(self.offset..self.rows.len())
            .take(height)
            .filter(|&index| index < self.rows.len())
            .collect();
        let owner = if self.view == View::Groups {
            "group"
        } else {
            "user"
        };
        let rows = visible.iter().map(|&index| {
            let entry = &self.rows[index];
            Row::new([
                self.usage_label(entry.item, entry.usage),
                entry.name.clone(),
            ])
        });
        let table = Table::new(rows, [Constraint::Length(13), Constraint::Min(12)])
            .header(
                Row::new(["Total", "Owner"]).style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        " Choose {owner} · {} matches / {} owners · → ",
                        self.rows.len().saturating_sub(1),
                        self.row_total
                    ))
                    .title_bottom(format!(
                        " Owners under {} ",
                        display_name(self.tree.path(self.owner_scope).as_os_str())
                    ))
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
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol(if self.directory_focus { "  " } else { "› " });
        let mut state = TableState::default();
        if !self.rows.is_empty() {
            state.select(visible.iter().position(|&index| index == self.selected));
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
                    Row::new([
                        self.usage_label(entry.item, entry.usage),
                        self.status_label(entry.item),
                        entry.name.clone(),
                    ])
                });
            let selected = &self.rows[self.selected].name;
            let table = Table::new(
                rows,
                [
                    Constraint::Length(13),
                    Constraint::Length(12),
                    Constraint::Min(12),
                ],
            )
            .header(
                Row::new(["Size", "Status", "Subdirectory"])
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        " {} shown / {} dirs · {selected} · Tab owners ",
                        entries.len(),
                        self.tree.nodes[self.current]
                            .children
                            .iter()
                            .filter(|&&id| self.tree.nodes[id].is_dir)
                            .count()
                    ))
                    .title_bottom(format!(" Browsing {} ", self.path))
                    .border_style(Style::default().fg(if self.directory_focus {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    })),
            )
            .row_highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol("› ");
            let mut directory_state = TableState::default();
            if self.directory_focus && !entries.is_empty() {
                directory_state.select(Some(self.directory_selected - self.directory_offset));
            }
            frame.render_stateful_widget(table, panes[1], &mut directory_state);
            if entries.is_empty() {
                let inner = Block::default().borders(Borders::ALL).inner(panes[1]);
                frame.render_widget(
                    Paragraph::new(if self.directory_search.is_empty() {
                        "No subdirectories. d shows files; ← returns; Tab selects owner."
                    } else {
                        "No matching directories. Esc clears search."
                    }),
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
                        if self.hide_zero
                            && usage.bytes(self.apparent) == 0
                            && !self.uncertain_usage(Item::Node(id))
                        {
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
        if self.view != View::Directories {
            let active = if self.view == View::Groups {
                self.filter.gid
            } else {
                self.filter.uid
            };
            if let Some(id) = active
                && !self.rows.iter().any(|entry| entry.item == Item::Owner(id))
            {
                self.rows.push(Entry {
                    item: Item::Owner(id),
                    name: format!("{} (no matching entries)", self.owner_label(id)),
                    usage: Usage::default(),
                });
            }
        }
        self.row_total = self.rows.len();
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
        let query = if self.view == View::Directories {
            &self.directory_search
        } else {
            &self.owner_search
        }
        .to_lowercase();
        self.rows.retain(|entry| {
            query.is_empty() || entry.name.to_lowercase().contains(&query)
            // A name lookup must not change the active owner while browsing its directories.
            || (self.directory_focus && Some(entry.item) == owner)
        });
        if self.view != View::Directories {
            self.rows.insert(
                0,
                Entry {
                    item: Item::AllOwners,
                    name: if self.view == View::Groups {
                        "All groups"
                    } else {
                        "All users"
                    }
                    .into(),
                    usage: self.tree.nodes[self.owner_scope].usage(self.selector_filter()),
                },
            );
        }
        if let Some(owner) = owner
            && let Some(index) = self.rows.iter().position(|entry| entry.item == owner)
        {
            self.selected = index;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.offset = self.offset.min(self.selected);
    }

    fn refresh_scan(&mut self, finished: Option<Tree>) {
        let selected = self.rows.get(self.selected).map(|r| r.item);
        let directory = self.live_directory_anchor.take().or_else(|| {
            self.preview_filter().and_then(|filter| {
                self.subdirectories(filter)
                    .get(self.directory_selected)
                    .map(|r| r.item)
            })
        });
        let mut target = selected;
        let mut directory_target = directory;
        if let Some(tree) = finished {
            let current = self.tree.path(self.current);
            let scope = self.tree.path(self.owner_scope);
            let target_path = match selected {
                Some(Item::Node(id)) => Some(self.tree.path(id)),
                _ => None,
            };
            let directory_path = match directory {
                Some(Item::Node(id)) => Some(self.tree.path(id)),
                _ => None,
            };
            let root_changed = self.tree.root != tree.root;
            self.tree = tree;
            self.current = 0;
            self.owner_scope = 0;
            for id in 0..self.tree.nodes.len() {
                if self.tree.nodes[id].is_dir {
                    let path = self.tree.path(id);
                    if current.starts_with(&path)
                        && path.components().count()
                            >= self.tree.path(self.current).components().count()
                    {
                        self.current = id;
                    }
                    if scope.starts_with(&path)
                        && path.components().count()
                            >= self.tree.path(self.owner_scope).components().count()
                    {
                        self.owner_scope = id;
                    }
                }
            }
            target = match target_path {
                Some(path) => self.tree.nodes[self.current]
                    .children
                    .iter()
                    .find(|&&id| self.tree.path(id) == path)
                    .copied()
                    .map(Item::Node),
                None => selected,
            };
            directory_target = directory_path.and_then(|path| {
                self.tree.nodes[self.current]
                    .children
                    .iter()
                    .find(|&&id| self.tree.path(id) == path)
                    .copied()
                    .map(Item::Node)
            });
            self.scanning = false;
            self.cached_refresh = false;
            self.refresh_error = None;
            self.cache_age = Some((0, Instant::now()));
            self.notice = if self.tree.error_count > 0 {
                "Scan finished with errors; e shows affected paths. Totals may be incomplete".into()
            } else {
                "Scan complete. Space marks files; x reviews deletion".into()
            };
            if root_changed {
                self.notice
                    .push_str(" | Scan root target changed; showing the new root");
            }
        }
        self.rebuild();
        if let Some(target) = target
            && let Some(index) = self.rows.iter().position(|r| r.item == target)
        {
            self.selected = index;
        }
        if let Some(target) = directory_target
            && let Some(filter) = self.preview_filter()
            && let Some(index) = self
                .subdirectories(filter)
                .iter()
                .position(|r| r.item == target)
        {
            self.directory_selected = index;
        }
        self.offset = self.offset.min(self.selected);
        self.directory_offset = self.directory_offset.min(self.directory_selected);
    }

    fn toggle_mark(&mut self) {
        if self.scanning || self.cached_refresh {
            self.notice =
                "Scanning: file marking/deletion becomes available when results are complete"
                    .into();
            return;
        }
        if self.view != View::Directories {
            self.notice = "Press d for the file listing, then Space to mark a file".into();
            return;
        }
        let Some(Item::Node(id)) = self.rows.get(self.selected).map(|row| row.item) else {
            self.notice = "Select a regular file; directory totals cannot be marked".into();
            return;
        };
        let path = self.tree.path(id);
        if self.marks.remove(&path).is_some() {
            self.notice = "File unmarked".into();
        } else {
            if !self
                .filter
                .matches(self.tree.nodes[id].uid, self.tree.nodes[id].gid)
            {
                self.notice = "File does not match the active owner filter".into();
                return;
            }
            match Mark::new(&self.tree, id) {
                Ok(mark) => {
                    self.marks.insert(path, mark);
                    self.notice =
                        "File marked; Space toggles, x reviews, c clears all marks".into();
                }
                Err(error) => {
                    self.notice = display_name(error.to_string().as_ref());
                    return;
                }
            }
        }
        self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
    }

    fn finish_deletion(&mut self, outcome: Outcome, scanned: io::Result<Tree>, previous: PathBuf) {
        self.busy = false;
        self.deletion_errors |= !outcome.failures.is_empty();
        let mut message = format!(
            "Deleted {} files; estimated space reclaimed: {}. {} failed/skipped.\nActual free space may differ (open files, snapshots, compression).",
            outcome.deleted,
            human(outcome.reclaimed),
            outcome.failures.len()
        );
        // Failed entries require a fresh explicit mark after review; never silently retry.
        for (mark, error) in outcome.failures.iter().take(100) {
            message.push_str(&format!(
                "\n{}: {}",
                display_name(mark.path.as_os_str()),
                display_name(error.as_ref())
            ));
        }
        match scanned {
            Ok(tree) => {
                self.tree = tree;
                self.current = (0..self.tree.nodes.len())
                    .find(|&id| self.tree.nodes[id].is_dir && self.tree.path(id) == previous)
                    .unwrap_or(0);
                self.view = View::Directories;
                self.owner_scope = self.current;
                self.directory_focus = false;
                self.selected = 0;
                self.offset = 0;
                self.rebuild();
            }
            Err(error) => {
                self.deletion_errors = true;
                // The old scan is stale. Disable further file actions until restart.
                self.rows.clear();
                self.tree.nodes.clear();
                message.push_str(&format!(
                    "\nRescan failed: {}. Restart zhandu to continue.",
                    display_name(error.to_string().as_ref())
                ));
            }
        }
        self.notice = message;
        self.review_scroll = 0;
        self.review = Some(String::new());
    }

    /// Returns true when the user requests quit.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if self.busy {
            return false;
        }
        if let Some(input) = &mut self.review {
            if self.marks.is_empty() {
                match key.code {
                    KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => self.review = None,
                    KeyCode::Down | KeyCode::PageDown => {
                        self.review_scroll = self.review_scroll.saturating_add(1)
                    }
                    KeyCode::Up | KeyCode::PageUp => {
                        self.review_scroll = self.review_scroll.saturating_sub(1)
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Esc => self.review = None,
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::PageDown | KeyCode::Down => {
                        self.review_scroll = self.review_scroll.saturating_add(1)
                    }
                    KeyCode::PageUp | KeyCode::Up => {
                        self.review_scroll = self.review_scroll.saturating_sub(1)
                    }
                    KeyCode::Char(c)
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                    {
                        if input.len() < 6 {
                            input.push(c);
                        }
                    }
                    KeyCode::Enter if input == "DELETE" && key.kind == KeyEventKind::Press => {
                        self.delete_requested = true;
                        self.busy = true;
                        self.review = None;
                    }
                    _ => {}
                }
            }
            return false;
        }
        if self.editing_search {
            match key.code {
                KeyCode::Enter => self.editing_search = false,
                KeyCode::Esc => {
                    self.editing_search = false;
                    self.search_query_mut().clear();
                    self.update_search();
                }
                KeyCode::Backspace => {
                    self.search_query_mut().pop();
                    self.update_search();
                }
                KeyCode::Char(c)
                    if !c.is_control()
                        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
                {
                    if self.search_query().len() < 256 {
                        self.search_query_mut().push(c);
                        self.update_search();
                    }
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
                _ => {}
            }
            return false;
        }
        if key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return true;
        }
        if self.tree.nodes.is_empty() {
            return false;
        }
        if self.overlay.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('e') | KeyCode::Char('i') => {
                    self.overlay = None
                }
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
        if key.code == KeyCode::Char('/') {
            self.editing_search = true;
            return false;
        }
        if key.code == KeyCode::Esc && !self.search_query().is_empty() {
            self.search_query_mut().clear();
            self.update_search();
            return false;
        }
        if self.overlay.is_none() && key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Char(' ') => {
                    self.toggle_mark();
                    return false;
                }
                KeyCode::Char('c') => {
                    self.marks.clear();
                    self.notice = "All marks cleared".into();
                    return false;
                }
                KeyCode::Char('x') => {
                    if self.scanning || self.cached_refresh {
                        self.notice =
                            "Scanning: deletion is disabled until the scan finishes".into();
                    } else if self.marks.is_empty() {
                        self.notice = "No files marked. Use Space on a file first".into();
                    } else {
                        self.review = Some(String::new());
                        self.review_scroll = 0;
                    }
                    return false;
                }
                _ => {}
            }
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
            KeyCode::Tab | KeyCode::BackTab => {
                self.notice = "One panel here. u chooses users; g chooses groups".into()
            }
            KeyCode::Char('g') => {
                self.open_picker(View::Groups);
                return false;
            }
            KeyCode::Char('u') => {
                self.open_picker(View::Users);
                return false;
            }
            KeyCode::Char('d') => {
                if let Some(filter) = self.preview_filter() {
                    self.filter = filter;
                }
                self.view = View::Directories;
                self.directory_focus = false;
                self.owner_search.clear();
                self.directory_search.clear();
                self.selected = 0;
                changed = true;
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(entry) = self.rows.get(self.selected) {
                    match entry.item {
                        Item::Node(id) if self.tree.nodes[id].is_dir => {
                            self.current = id;
                            self.directory_search.clear();
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
                    self.directory_search.clear();
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
                self.owner_search.clear();
                self.directory_search.clear();
                self.selected = 0;
                changed = true;
            }
            KeyCode::Char('r') => {
                if !self.scanning {
                    self.refresh_requested = true;
                } else {
                    self.notice = "Refresh already running".into();
                }
            }
            KeyCode::Char('z') => {
                self.hide_zero = !self.hide_zero;
                changed = true;
                self.directory_selected = 0;
                self.directory_offset = 0;
            }
            KeyCode::Char('a') => {
                self.apparent = !self.apparent;
                changed = true;
            }
            KeyCode::Char('s') => {
                self.sort_name = !self.sort_name;
                changed = true;
            }
            KeyCode::Char('i') => {
                self.details = self.selected_details();
                self.overlay = Some(Overlay::Details);
                self.error_scroll = 0;
            }
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                self.error_scroll = 0;
            }
            KeyCode::Char('e') => {
                self.overlay = Some(Overlay::Errors);
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

    fn selected_details(&self) -> String {
        let item = if self.directory_focus {
            self.preview_filter().and_then(|filter| {
                self.subdirectories(filter)
                    .get(self.directory_selected)
                    .map(|e| e.item)
            })
        } else {
            self.rows.get(self.selected).map(|e| e.item)
        };
        let Some(item) = item else {
            return "No selected entry. Esc returns.".into();
        };
        let id = match item {
            Item::Node(id) => id,
            Item::SelfUsage => self.current,
            _ => {
                return format!(
                    "Owner scope: {}\n{}\nSelect a directory or press d for individual files.\nEsc returns.",
                    display_name(self.tree.path(self.owner_scope).as_os_str()),
                    self.rows[self.selected].name
                );
            }
        };
        let node = &self.tree.nodes[id];
        let eligibility = if self.scanning || self.cached_refresh {
            "Marking disabled until refresh finishes."
        } else if node.is_dir {
            "Directories cannot be marked; open to select individual files."
        } else if node.is_symlink {
            "Symlinks cannot be marked."
        } else {
            "Space attempts to mark a regular file; current metadata and ownership are checked."
        };
        format!(
            "Path: {}\nUser: {}\nGroup: {}\nStatus: {}\nAllocated (unfiltered): {}\nApparent (unfiltered): {}\nAttributed inodes: {}\nHard link counted elsewhere: {}\n\n{}\nActual reclaimed space may differ from allocated usage.\nUp/down scroll; Esc returns.",
            display_name(self.tree.path(id).as_os_str()),
            self.named_owner(false, node.uid),
            self.named_owner(true, node.gid),
            self.status_label(item),
            human(node.total.allocated),
            human(node.total.apparent),
            node.total.inodes,
            node.shared_link,
            eligibility
        )
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        if self.busy {
            frame.render_widget(Paragraph::new("Deleting confirmed files and rescanning...\nCtrl-C stops before the next file; already deleted files cannot be restored.")
                .block(Block::default().borders(Borders::ALL).title(" zhandu ")).wrap(Wrap { trim: false }), frame.area());
            return;
        }
        if let Some(input) = &self.review {
            if self.marks.is_empty() {
                frame.render_widget(
                    Paragraph::new(format!("{}\n\nEnter / Esc: return", self.notice))
                        .wrap(Wrap { trim: false })
                        .scroll((self.review_scroll, 0))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(" Deletion result "),
                        ),
                    frame.area(),
                );
            } else {
                let areas = Layout::vertical([
                    Constraint::Length(6),
                    Constraint::Min(1),
                    Constraint::Length(3),
                ])
                .split(frame.area());
                let header = format!(
                    "Permanently delete {} marked files?\nEstimated reclaimable: {} (allocated bytes, not guaranteed free space).\nAll marks are included, even outside this directory or active filter.\nNo undo. Esc cancels. Type DELETE, then Enter to confirm.",
                    self.marks.len(),
                    human(deletion::estimate(self.marks.values()))
                );
                frame.render_widget(
                    Paragraph::new(header).wrap(Wrap { trim: false }).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Review deletion "),
                    ),
                    areas[0],
                );
                let paths = self
                    .marks
                    .keys()
                    .map(|p| display_name(p.as_os_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                frame.render_widget(
                    Paragraph::new(paths)
                        .wrap(Wrap { trim: false })
                        .scroll((self.review_scroll, 0))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(" Marked files · ↑↓ scroll "),
                        ),
                    areas[1],
                );
                frame.render_widget(
                    Paragraph::new(format!("Confirm: {input}"))
                        .block(Block::default().borders(Borders::ALL)),
                    areas[2],
                );
            }
            return;
        }
        if self.tree.nodes.is_empty() {
            frame.render_widget(
                Paragraph::new(format!("{}\nq: quit", self.notice)).wrap(Wrap { trim: false }),
                frame.area(),
            );
            return;
        }
        let areas = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(6),
        ])
        .split(frame.area());
        let incomplete = if self.tree.error_count > 0 {
            " | INCOMPLETE"
        } else {
            ""
        };
        let title = format!(
            " zhandu | {:?} | {}{}{} ",
            self.view,
            if self.apparent {
                "apparent size"
            } else {
                "allocated disk usage"
            },
            incomplete,
            if self.cached_refresh {
                if self.scanning {
                    " | CACHED - refreshing"
                } else {
                    " | STALE - refresh failed"
                }
            } else if self.scanning {
                " | Scanning - PARTIAL"
            } else if self.tree.error_count > 0 {
                " | Finished with errors - PARTIAL"
            } else {
                " | COMPLETE"
            }
        );
        let active = self.preview_filter().unwrap_or(self.filter);
        let filters = format!(
            "User: {} | Group: {}{}",
            active
                .uid
                .map_or_else(|| "All".into(), |id| self.named_owner(false, id)),
            active
                .gid
                .map_or_else(|| "All".into(), |id| self.named_owner(true, id)),
            if self.searching_owners() {
                " | Preview"
            } else {
                ""
            }
        );
        let title = if self.cached_refresh {
            format!(
                "Cache {}s old{} | {title}",
                self.cache_age
                    .map_or(0, |(age, since)| age + since.elapsed().as_secs() as i64),
                if self.cache_was_partial {
                    " (partial)"
                } else {
                    ""
                }
            )
        } else {
            title
        };
        frame.render_widget(
            Paragraph::new(format!("{}\n{}", self.path, filters))
                .block(Block::default().borders(Borders::ALL).title(title)),
            areas[0],
        );
        if let Some(overlay) = self.overlay {
            let text = if matches!(overlay, Overlay::Details) {
                self.details.clone()
            } else if matches!(overlay, Overlay::Help) {
                "↑/↓ or j/k: select     →/Enter: open directory / filter owner\n←/Backspace: parent     Tab: switch panels (in owner view)\ng: choose group   u: choose user   d: directories   Esc: clear search first, then BOTH filters\n/: search focused panel; Enter finishes typing; Esc clears search\nHome in owners: All users/groups (clears only that filter)\n~ provisional/incomplete size; … unknown; e shows scan errors\na: allocated / apparent size    s: size / name sort\nr: refresh (preserve navigation; revalidate marks)   z: hide/show known zero usage\nHome/End, PageUp/PageDown: move   e: errors   i: details   ?: help   q: quit\n\nOwner panel: →/Enter focuses directories. ↑↓ selects within the focused panel.\nDirectory panel: →/Enter opens, ← goes up (at starting path: owners).\nTab switches focus in owner views; d opens files with the same filter.\nAll stays pinned. Owner totals use the labeled scope; browsing may be deeper.\nOwners: Unix UID/GID or Windows owner/primary-group SID. Filters combine.\nTotals include directory blocks and symlinks, and include hidden files.\nHard links count once at their first sorted depth-first scan location.\n@ marks a symlink (not followed). Allocated bytes are not reclaimable bytes.\n\nSpace toggles a regular file mark and advances. c clears all marks.\nx reviews ALL marks; type DELETE and Enter to permanently delete.\nDirectories and symlinks cannot be marked. q exits without deleting marks.\nEsc closes this panel.".to_owned()
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
                    .block(Block::default().borders(Borders::ALL).title(match overlay {
                        Overlay::Help => " Help ",
                        Overlay::Errors => " Scan errors ",
                        Overlay::Details => " Details (snapshot at opening) ",
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
                        Cell::from(self.usage_label(entry.item, entry.usage)),
                        Cell::from(if self.scanning || self.uncertain_usage(Item::Node(self.current)) { "…".into() } else { format!("{percent:5.1}%") }),
                        Cell::from(entry.usage.inodes.to_string()),
                        Cell::from(self.status_label(entry.item)),
                        Cell::from(format!("{}{}", if matches!(entry.item, Item::Node(id) if self.marks.contains_key(&self.tree.path(id))) { "[x] " } else { "    " }, entry.name)),
                    ])
                });
            let table = Table::new(
                rows,
                [
                    Constraint::Length(13),
                    Constraint::Length(8),
                    Constraint::Length(10),
                    Constraint::Length(12),
                    Constraint::Min(12),
                ],
            )
            .header(
                Row::new(["Size", "%", "Inodes", "Status", "Name"])
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} shown / {} eligible entries ",
                self.rows.len(),
                self.row_total
            )))
            .row_highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol("› ");
            let mut state = TableState::default();
            if !self.rows.is_empty() {
                state.select(Some(self.selected - self.offset));
            }
            frame.render_stateful_widget(table, areas[1], &mut state);
            if self.rows.is_empty() {
                let inner = Block::default().borders(Borders::ALL).inner(areas[1]);
                frame.render_widget(
                    Paragraph::new(if !self.directory_search.is_empty() {
                        "No search matches. Esc clears search; totals and marks are unchanged."
                    } else {
                        "No visible entries. z shows zeros; Esc clears ownership filters."
                    })
                    .wrap(Wrap { trim: false }),
                    inner,
                );
            }
        }
        let active = self.preview_filter().unwrap_or(self.filter);
        let usage = self.tree.nodes[self.current].usage(active);
        let activity = format!(
            "Marked {} files | est. reclaimable {} | {}",
            self.marks.len(),
            human(deletion::estimate(self.marks.values())),
            if self.scanning || self.cached_refresh {
                "mark/delete disabled during refresh"
            } else {
                "Space mark  x review  c clear"
            }
        );
        let search = if self.editing_search || !self.search_query().is_empty() {
            format!(
                "Search {}: /{} | {}",
                if self.searching_owners() {
                    "owners"
                } else {
                    "names"
                },
                self.search_query(),
                if self.editing_search {
                    "type to filter; Enter navigate; Esc clear"
                } else {
                    "Esc clears search; / edit"
                }
            )
        } else if let Some(error) = &self.refresh_error {
            format!("Refresh failed: {error}; r retries")
        } else if self.cached_refresh {
            format!(
                "Cache: {}s old | Refreshing; unvisited entries may be stale",
                self.cache_age
                    .map_or(0, |(age, since)| age + since.elapsed().as_secs() as i64)
            )
        } else {
            self.notice.lines().next().unwrap_or("").into()
        };
        let keys = if self.editing_search {
            "Enter navigate  Esc clear search  Backspace edit"
        } else if self.view == View::Directories {
            "↑↓ select  → open  ← parent  / search  r refresh  z zeros  g/u owners  ? help"
        } else if self.directory_focus {
            "↑↓ select  → open  ← parent  Tab owners  / search  r refresh  z zeros  d files"
        } else {
            "↑↓ preview  Home All  → directories  Tab focus  / search  r refresh  z zeros"
        };
        let status = format!(
            "Total {} | {} inodes | scan {:.2}s | {} errors | zeros {}\n{}\n{}\n{}",
            self.usage_label(Item::Node(self.current), usage),
            usage.inodes,
            self.tree.elapsed_seconds,
            self.tree.error_count,
            if self.hide_zero { "hidden" } else { "shown" },
            activity,
            search,
            keys
        );
        frame.render_widget(
            Paragraph::new(status).block(Block::default().borders(Borders::ALL)),
            areas[2],
        );
    }
}

struct DeletionWorker {
    handle: Option<std::thread::JoinHandle<()>>,
    progress: Arc<Progress>,
}
impl Drop for DeletionWorker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.progress.cancel();
            let _ = handle.join();
        }
    }
}

pub fn run(
    options: ScanOptions,
    progress: Arc<Progress>,
    filter: Filter,
    view: View,
    apparent: bool,
    numeric: bool,
    cache_config: CacheConfig,
) -> io::Result<i32> {
    // Ratatui's guard restores terminal modes on return, errors, and panics.
    ratatui::run(|terminal| -> io::Result<i32> {
        let root_label = display_name(options.path.as_os_str());
        let mut start = Instant::now();
        let rescan_options = options.clone();
        let mut session = ScanSession::start(
            options,
            progress.clone(),
            cache_config.clone(),
            true,
            vec![],
        );
        let mut startup_ready = false;
        let mut startup_notice = String::new();
        let mut cached_age = None;
        let (tree, mut live_state, scanning) = loop {
            if progress.is_cancelled() {
                return Ok(130);
            }
            if !startup_ready {
                match session.startup.try_recv() {
                    Ok((cached, warning)) => {
                        startup_ready = true;
                        if let Some(warning) = warning {
                            startup_notice = display_name(warning.as_ref());
                        }
                        if let Some((tree, age)) = cached {
                            let state = LiveState::from_cached(&tree);
                            cached_age = Some(age);
                            break (tree, Some(state), true);
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(io::Error::other("cache loader stopped unexpectedly"));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if startup_ready && let Ok(LiveEvent::Root(root, metadata)) = session.events.try_recv()
            {
                let (tree, state) = LiveState::start(root, metadata);
                break (tree, Some(state), true);
            }
            if startup_ready {
                match session.completed.try_recv() {
                    Ok((result, message, _)) => {
                        if let Some(message) = message {
                            startup_notice = display_name(message.as_ref());
                        }
                        break (result?, None, false);
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(io::Error::other("scanner stopped unexpectedly"));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            terminal.draw(|frame| {
                let phase = if !startup_ready { "Loading cache / preparing scan" } else if progress.phase.load(Ordering::Relaxed) == 1 { "Aggregating ownership totals" } else { "Scanning" };
                let text = format!("{phase}: {root_label}\n\n{} entries inspected | {} errors | {:.1}s\n\nOpening live results as entries arrive. q / Esc / Ctrl-C cancels.",
                    progress.entries.load(Ordering::Relaxed), progress.errors.load(Ordering::Relaxed), start.elapsed().as_secs_f64());
                frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).title(" zhandu ")), frame.area());
            })?;
            if event::poll(Duration::from_millis(10))?
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
        let (names_tx, names_rx) = mpsc::channel();
        let (owner_tx, owner_rx) = mpsc::channel();
        let mut requested_owners = std::collections::HashSet::new();
        if !numeric {
            std::thread::spawn(move || {
                let mut seen = std::collections::HashSet::new();
                for (uid, gid) in owner_rx {
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
        app.scanning = scanning;
        app.cached_refresh = cached_age.is_some();
        app.cache_age = cached_age.map(|age| (age, Instant::now()));
        app.cache_was_partial = cached_age.is_some() && app.tree.error_count > 0;
        app.notice = if let Some(age) = cached_age {
            format!(
                "Cached snapshot from {age}s ago; unvisited/missing entries remain until refresh completes"
            )
        } else if !scanning {
            format!("Scan complete. {startup_notice}")
        } else {
            startup_notice
        };
        if app.cached_refresh {
            terminal.draw(|frame| app.draw(frame))?;
        }
        let mut dirty = true;
        let mut live_changed = false;
        let mut last_refresh = Instant::now();
        type DeleteResult = (Outcome, io::Result<Tree>, PathBuf, Option<String>);
        let (delete_tx, delete_rx) = mpsc::channel::<DeleteResult>();
        let mut deletion_worker = DeletionWorker {
            handle: None,
            progress: progress.clone(),
        };
        loop {
            if app.refresh_requested {
                app.refresh_requested = false;
                progress.entries.store(0, Ordering::Relaxed);
                progress.errors.store(0, Ordering::Relaxed);
                progress.phase.store(0, Ordering::Relaxed);
                live_state = Some(LiveState::from_cached(&app.tree));
                session = ScanSession::start(
                    rescan_options.clone(),
                    progress.clone(),
                    cache_config.clone(),
                    false,
                    app.marks.values().cloned().collect(),
                );
                app.scanning = true;
                app.cached_refresh = true;
                app.cache_was_partial = app.tree.error_count > 0;
                app.refresh_error = None;
                if app.cache_age.is_none() {
                    app.cache_age = Some((0, Instant::now()));
                }
                app.notice = "Refreshing filesystem metadata; browsing remains available".into();
                start = Instant::now();
                live_changed = false;
                dirty = true;
            }
            if app.scanning {
                // Bound UI work per turn; render/sort at most five times per second.
                let drain_start = Instant::now();
                while drain_start.elapsed() < Duration::from_millis(10) {
                    let Ok(event) = session.events.try_recv() else {
                        break;
                    };
                    if !live_changed {
                        app.live_directory_anchor = app.preview_filter().and_then(|filter| {
                            app.subdirectories(filter)
                                .get(app.directory_selected)
                                .map(|r| r.item)
                        });
                    }
                    live_state.as_mut().unwrap().apply(&mut app.tree, event);
                    live_changed = true;
                }
                match session.completed.try_recv() {
                    Ok((result, cache_message, valid)) => {
                        let old_marks = app.marks.len();
                        match result {
                            Ok(tree) => {
                                app.refresh_scan(Some(tree));
                                let valid: std::collections::HashSet<_> =
                                    valid.into_iter().collect();
                                app.marks.retain(|path, _| valid.contains(path));
                                if old_marks != app.marks.len() {
                                    app.notice.push_str(&format!(
                                        " | {} changed/missing marks cleared",
                                        old_marks - app.marks.len()
                                    ));
                                }
                            }
                            Err(error) => {
                                app.scanning = false;
                                app.cached_refresh = true;
                                app.refresh_error = Some(display_name(error.to_string().as_ref()));
                                app.notice = "Refresh failed. Previous values are stale; r retries. Deletion disabled".into();
                            }
                        }
                        if let Some(message) = cache_message {
                            app.notice
                                .push_str(&format!(" | {}", display_name(message.as_ref())));
                        }
                        live_state = None;
                        live_changed = false;
                        dirty = true;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(io::Error::other("scanner stopped unexpectedly"));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                if app.scanning && last_refresh.elapsed() >= Duration::from_millis(200) {
                    app.tree.elapsed_seconds = start.elapsed().as_secs_f64();
                    if live_changed {
                        app.refresh_scan(None);
                        live_changed = false;
                    }
                    dirty = true;
                    last_refresh = Instant::now();
                }
            }
            if !numeric && !app.tree.nodes.is_empty() {
                for &owner in app.tree.nodes[0].owners.keys() {
                    if requested_owners.insert(owner) {
                        let _ = owner_tx.send(owner);
                    }
                }
            }
            if let Ok((outcome, scanned, previous, cache_message)) = delete_rx.try_recv() {
                if let Some(handle) = deletion_worker.handle.take() {
                    handle
                        .join()
                        .map_err(|_| io::Error::other("deletion worker panicked"))?;
                }
                app.finish_deletion(outcome, scanned, previous);
                if let Some(message) = cache_message {
                    app.notice
                        .push_str(&format!("\n{}", display_name(message.as_ref())));
                }
                dirty = true;
            }
            if deletion_worker
                .handle
                .as_ref()
                .is_some_and(|handle| handle.is_finished())
            {
                deletion_worker
                    .handle
                    .take()
                    .unwrap()
                    .join()
                    .map_err(|_| io::Error::other("deletion worker panicked"))?;
            }
            if progress.is_cancelled() && !app.busy {
                return Ok(130);
            }
            if app.delete_requested {
                app.delete_requested = false;
                terminal.draw(|frame| app.draw(frame))?;
                let marks = std::mem::take(&mut app.marks).into_values().collect();
                let previous = app.tree.path(app.current);
                let options = rescan_options.clone();
                let worker_progress = progress.clone();
                let sender = delete_tx.clone();
                let config = cache_config.clone();
                deletion_worker.handle = Some(std::thread::spawn(move || {
                    let outcome = deletion::execute(marks, &worker_progress);
                    let cache = Cache::new(&options, &config);
                    let scanned = scan(options, worker_progress.clone());
                    let cache_message = match (cache, &scanned) {
                        (Ok(Some(cache)), Ok(tree)) => {
                            Some(match cache.save(tree, &worker_progress) {
                                Ok(()) => {
                                    if tree.error_count > 0 {
                                        "Partial cache saved; previous complete snapshot retained"
                                            .into()
                                    } else {
                                        "Cache saved".into()
                                    }
                                }
                                Err(error) => format!("Cache not saved: {error}"),
                            })
                        }
                        (Err(error), _) => Some(format!("Cache unavailable: {error}")),
                        _ => None,
                    };
                    let _ = sender.send((outcome, scanned, previous, cache_message));
                }));
            }
            let mut names_changed = false;
            for (id, name) in names_rx.try_iter() {
                app.names.insert(id, name);
                names_changed = true;
            }
            if names_changed && !app.tree.nodes.is_empty() {
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
                if live_changed {
                    app.refresh_scan(None);
                    live_changed = false;
                }
                terminal.draw(|frame| app.draw(frame))?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(if app.scanning { 1 } else { 100 }))? {
                match event::read()? {
                    Event::Key(key) => {
                        if live_changed {
                            app.refresh_scan(None);
                            live_changed = false;
                        }
                        if key.kind != KeyEventKind::Release
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            progress.cancel();
                            if !app.busy {
                                return Ok(130);
                            }
                        }
                        if app.key(key) {
                            if app.scanning {
                                progress.cancel();
                                return Ok(130);
                            }
                            return Ok(
                                if app.deletion_errors
                                    || app.tree.error_count > 0
                                    || app.refresh_error.is_some()
                                {
                                    2
                                } else {
                                    0
                                },
                            );
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

    fn fixture_app() -> App {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        for i in 0..30 {
            std::fs::write(dir.path().join(format!("file{i:02}")), b"data").unwrap();
        }
        let mut tree = scan(
            ScanOptions {
                path: dir.path().into(),
                threads: 1,
                one_file_system: false,
                excludes: vec![],
            },
            Arc::default(),
        )
        .unwrap();
        for (id, node) in tree.nodes.iter_mut().enumerate() {
            node.uid = id as u32;
            node.gid = 50;
        }
        tree.aggregate();
        App::new(
            tree,
            Filter {
                uid: Some(3),
                gid: Some(50),
            },
            View::Users,
            false,
        )
    }

    fn press(app: &mut App, key: KeyCode) {
        assert!(!app.key(KeyEvent::new(key, KeyModifiers::NONE)));
    }

    fn type_search(app: &mut App, query: &str) {
        press(app, KeyCode::Char('/'));
        for c in query.chars() {
            press(app, KeyCode::Char(c));
        }
        press(app, KeyCode::Enter);
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn all_owners_is_pinned_and_clears_only_its_dimension() {
        let mut app = fixture_app();
        assert_eq!(app.rows[app.selected].item, Item::Owner(3));
        press(&mut app, KeyCode::End);
        let text = screen(&mut app, 140, 18);
        assert!(app.offset > 1);
        assert!(text.contains("All users"));
        press(&mut app, KeyCode::Home);
        assert_eq!(app.rows[app.selected].item, Item::AllOwners);
        assert_eq!(
            app.preview_filter(),
            Some(Filter {
                uid: None,
                gid: Some(50)
            })
        );
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.filter,
            Filter {
                uid: None,
                gid: Some(50)
            }
        );
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Enter);
        assert!(app.filter.is_empty());
        // Clearing a group must also preserve a nonempty user filter.
        app.filter.uid = Some(3);
        app.filter.gid = Some(50);
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(
            app.filter,
            Filter {
                uid: Some(3),
                gid: None
            }
        );
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::Directories);
    }

    #[test]
    fn search_is_panel_local_and_escape_closes_overlay_then_search_then_filters() {
        let mut app = fixture_app();
        app.names.insert((false, 3), "Alice".into());
        app.names.insert((true, 50), "Research".into());
        app.rebuild();
        type_search(&mut app, "aLiCe");
        assert_eq!(app.rows.len(), 2); // All plus the matching user.
        assert_eq!(app.rows[app.selected].item, Item::Owner(3));
        let text = screen(&mut app, 140, 24);
        assert!(text.contains("User: Alice (3) | Group: Research (50)"));
        press(&mut app, KeyCode::Right);
        type_search(&mut app, "BETA");
        let entries = app.subdirectories(app.preview_filter().unwrap());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "beta/");
        press(&mut app, KeyCode::Char('?'));
        press(&mut app, KeyCode::Esc);
        assert!(app.overlay.is_none());
        assert_eq!(app.directory_search, "BETA");
        press(&mut app, KeyCode::Esc);
        assert!(app.directory_search.is_empty());
        assert_eq!(app.owner_search, "aLiCe");
        assert_eq!(
            app.filter,
            Filter {
                uid: Some(3),
                gid: Some(50)
            }
        );
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Esc);
        assert!(app.owner_search.is_empty());
        assert!(app.filter.uid.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.filter.is_empty());
        type_search(&mut app, "file03");
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].name, "file03");
        app.refresh_scan(None);
        assert_eq!(app.rows.len(), 1);
        press(&mut app, KeyCode::Esc);
        // Typing shortcut letters in search cannot quit, delete, or change views.
        type_search(&mut app, "qx ug");
        assert!(app.rows.is_empty());
        assert!(app.review.is_none());
        assert_eq!(app.view, View::Directories);
        assert!(screen(&mut app, 80, 24).contains("Search names:"));
        press(&mut app, KeyCode::Esc);
        assert!(!app.rows.is_empty());
    }

    #[test]
    fn disappearing_owner_does_not_change_active_filter() {
        let mut app = fixture_app();
        press(&mut app, KeyCode::Right);
        let filter = app.filter;
        for node in &mut app.tree.nodes {
            if node.uid == 3 {
                node.uid = 999;
            }
        }
        app.tree.aggregate();
        app.refresh_scan(None);
        assert_eq!(app.preview_filter(), Some(filter));
        assert!(app.rows[app.selected].name.contains("no matching entries"));
        press(&mut app, KeyCode::Char('i'));
        assert!(matches!(app.overlay, Some(Overlay::Details)));
        press(&mut app, KeyCode::Esc);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn removed_current_directory_returns_to_nearest_surviving_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        let options = ScanOptions {
            path: dir.path().into(),
            threads: 1,
            one_file_system: false,
            excludes: vec![],
        };
        let tree = scan(options.clone(), Arc::default()).unwrap();
        let mut app = App::new(tree, Filter::default(), View::Directories, false);
        app.current = app.tree.nodes.iter().position(|n| n.name == "b").unwrap();
        app.rebuild();
        std::fs::remove_dir(dir.path().join("a/b")).unwrap();
        app.refresh_scan(Some(scan(options, Arc::default()).unwrap()));
        assert_eq!(
            app.tree.path(app.current),
            dir.path().canonicalize().unwrap().join("a")
        );
        app.selected = app
            .rows
            .iter()
            .position(|e| e.item == Item::SelfUsage)
            .unwrap();
        let details = app.selected_details();
        assert!(details.contains("Directories cannot be marked"));
        assert!(details.contains("Allocated (unfiltered)"));
    }

    #[test]
    fn refresh_and_hide_zero_keep_filters_and_unknown_directories() {
        let mut app = fixture_app();
        let filter = app.filter;
        press(&mut app, KeyCode::Right);
        let before = app.subdirectories(filter).len();
        assert!(before > 0);
        press(&mut app, KeyCode::Char('z'));
        assert!(app.subdirectories(filter).is_empty());
        app.scanning = true;
        for node in &mut app.tree.nodes {
            node.scan.own_pending = node.is_dir;
        }
        app.tree.aggregate();
        assert_eq!(app.subdirectories(filter).len(), before);
        press(&mut app, KeyCode::Char('r'));
        assert!(!app.refresh_requested);
        app.scanning = false;
        press(&mut app, KeyCode::Char('r'));
        assert!(app.refresh_requested);
        assert_eq!(app.filter, filter);
        assert!(app.directory_focus);
        app.cached_refresh = true;
        app.refresh_error = Some("offline".into());
        press(&mut app, KeyCode::Char('x'));
        assert!(app.review.is_none());
        assert!(screen(&mut app, 140, 24).contains("STALE - refresh failed"));
    }

    #[test]
    fn marks_and_cache_age_remain_visible_with_errors_and_search() {
        let mut app = fixture_app();
        app.tree.error_count = 2;
        assert!(screen(&mut app, 120, 24).contains("Marked 0 files"));
        app.cached_refresh = true;
        app.scanning = true;
        app.cache_age = Some((3600, Instant::now()));
        type_search(&mut app, "missing");
        let text = screen(&mut app, 80, 24);
        assert!(text.contains("Cache 3600s old"));
        assert!(text.contains("Marked 0 files"));
        assert!(text.contains("Search owners:"));
    }

    #[test]
    fn uncertain_totals_are_not_reported_as_known_zero() {
        let mut app = fixture_app();
        let zero = Usage::default();
        app.scanning = true;
        app.tree.nodes[0].scan.own_pending = true;
        app.tree.aggregate();
        assert_eq!(app.usage_label(Item::AllOwners, zero), "…");
        assert!(screen(&mut app, 160, 24).contains("Scanning - PARTIAL"));
        assert!(
            app.usage_label(Item::AllOwners, app.tree.nodes[0].total)
                .starts_with('~')
        );
        app.cached_refresh = true;
        assert!(screen(&mut app, 160, 24).contains("CACHED - refreshing"));
        app.scanning = false;
        app.cached_refresh = false;
        app.tree.error_count = 1;
        assert_eq!(app.usage_label(Item::AllOwners, zero), "…");
        assert!(screen(&mut app, 160, 24).contains("Finished with errors - PARTIAL"));
        app.tree.error_count = 0;
        app.tree.nodes[0].scan = crate::model::ScanState::default();
        assert_eq!(app.usage_label(Item::AllOwners, zero), "0.0 B");
        assert!(screen(&mut app, 160, 24).contains("COMPLETE"));
    }

    #[test]
    fn owner_directory_columns_stay_aligned_when_focus_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("example")).unwrap();
        let tree = || {
            scan(
                ScanOptions {
                    path: dir.path().into(),
                    threads: 1,
                    one_file_system: false,
                    excludes: vec![],
                },
                Arc::default(),
            )
            .unwrap()
        };
        for view in [View::Users, View::Groups] {
            for width in [80, 140] {
                let mut app = App::new(tree(), Filter::default(), view, false);
                let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
                let positions = |terminal: &Terminal<TestBackend>| {
                    let buffer = terminal.backend().buffer();
                    let mut positions = Vec::new();
                    for y in 0..30 {
                        let line: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                        for label in ["Size", "Subdirectory", "example/"] {
                            if let Some(x) = line.find(label) {
                                positions.push((label, line[..x].chars().count(), y));
                            }
                        }
                    }
                    assert_eq!(positions.len(), 3);
                    positions
                };
                terminal
                    .draw(|f| app.draw_owner_picker(f, f.area()))
                    .unwrap();
                let before = positions(&terminal);
                app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
                assert!(app.directory_focus);
                terminal
                    .draw(|f| app.draw_owner_picker(f, f.area()))
                    .unwrap();
                assert_eq!(positions(&terminal), before);
                app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
                assert!(!app.directory_focus);
                terminal
                    .draw(|f| app.draw_owner_picker(f, f.area()))
                    .unwrap();
                assert_eq!(positions(&terminal), before);
            }
        }
    }

    #[test]
    fn live_owner_directory_cursor_stays_on_same_directory_after_resort() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("b/data"), [1; 8192]).unwrap();
        let root = dir.path().canonicalize().unwrap();
        let metadata = crate::platform::read_metadata;
        let (mut tree, mut state) = LiveState::start(root.clone(), metadata(&root).unwrap());
        state.apply(
            &mut tree,
            LiveEvent::Entries {
                parent: root.clone(),
                entries: vec![
                    ("a".into(), metadata(&root.join("a")).unwrap()),
                    ("b".into(), metadata(&root.join("b")).unwrap()),
                ],
            },
        );
        let mut app = App::new(tree, Filter::default(), View::Groups, false);
        app.scanning = true;
        app.focus_directories();
        let b = app.tree.nodes.iter().position(|n| n.name == "b").unwrap();
        app.directory_selected = app
            .subdirectories(app.filter)
            .iter()
            .position(|r| r.item == Item::Node(b))
            .unwrap();
        app.live_directory_anchor = Some(Item::Node(b));
        state.apply(
            &mut app.tree,
            LiveEvent::Entries {
                parent: root.join("b"),
                entries: vec![("data".into(), metadata(&root.join("b/data")).unwrap())],
            },
        );
        app.refresh_scan(None);
        assert_eq!(
            app.subdirectories(app.filter)[app.directory_selected].item,
            Item::Node(b)
        );
        assert_eq!(app.directory_selected, 0);
        assert!(app.directory_focus);
    }

    #[test]
    fn live_refresh_preserves_paths_filters_and_disables_deletion() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name).join("data"), [3; 8192]).unwrap();
        }
        let root = dir.path().canonicalize().unwrap();
        let metadata = crate::platform::read_metadata;
        let (tree, mut state) = LiveState::start(root.clone(), metadata(&root).unwrap());
        let mut app = App::new(tree, Filter::default(), View::Directories, false);
        app.scanning = true;
        state.apply(
            &mut app.tree,
            LiveEvent::Entries {
                parent: root.clone(),
                entries: vec![
                    ("b".into(), metadata(&root.join("b")).unwrap()),
                    ("a".into(), metadata(&root.join("a")).unwrap()),
                ],
            },
        );
        app.refresh_scan(None);
        let b = app.tree.nodes.iter().position(|n| n.name == "b").unwrap();
        app.selected = app
            .rows
            .iter()
            .position(|r| r.item == Item::Node(b))
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        state.apply(
            &mut app.tree,
            LiveEvent::Entries {
                parent: root.join("b"),
                entries: vec![("data".into(), metadata(&root.join("b/data")).unwrap())],
            },
        );
        app.refresh_scan(None);
        app.selected = app.rows.iter().position(|r| r.name == "data").unwrap();
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.marks.is_empty() && app.review.is_none());
        app.filter.gid = Some(app.tree.nodes[b].gid);
        let filter = app.filter;
        let final_tree = scan(
            ScanOptions {
                path: root.clone(),
                threads: 2,
                one_file_system: false,
                excludes: vec![],
            },
            Arc::default(),
        )
        .unwrap();
        app.refresh_scan(Some(final_tree));
        assert!(!app.scanning);
        assert_eq!(app.tree.path(app.current), root.join("b"));
        assert_eq!(app.filter, filter);
        let Item::Node(selected) = app.rows[app.selected].item else {
            panic!("file selected");
        };
        assert_eq!(app.tree.path(selected), root.join("b/data"));
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(app.marks.len(), 1);
    }

    #[test]
    fn marking_review_confirmation_and_rescan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), [7; 8192]).unwrap();
        let options = ScanOptions {
            path: dir.path().into(),
            threads: 1,
            one_file_system: false,
            excludes: vec![],
        };
        let tree = scan(options.clone(), Arc::default()).unwrap();
        let mut app = App::new(tree, Filter::default(), View::Directories, false);
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let select_file = |app: &mut App| {
            app.selected = app.rows.iter().position(|r| r.name == "file").unwrap();
        };
        select_file(&mut app);
        let cursor = app.selected;
        app.key(press(KeyCode::Char(' ')));
        assert_eq!(app.marks.len(), 1);
        assert_ne!(app.selected, cursor);
        select_file(&mut app);
        app.key(press(KeyCode::Char(' ')));
        assert!(app.marks.is_empty());
        select_file(&mut app);
        app.key(press(KeyCode::Char(' ')));
        // Marks survive navigation/view and filter changes.
        app.key(press(KeyCode::Char('g')));
        app.key(press(KeyCode::Esc));
        assert_eq!(app.marks.len(), 1);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("[x] file"));
        assert!(text.contains("Marked 1 files"));
        app.key(press(KeyCode::Char('x')));
        app.key(press(KeyCode::Enter));
        assert!(!app.delete_requested);
        app.key(press(KeyCode::Esc));
        assert!(dir.path().join("file").exists());
        assert_eq!(app.marks.len(), 1);
        app.key(press(KeyCode::Char('x')));
        for (w, h) in [(120, 30), (20, 5), (1, 1)] {
            Terminal::new(TestBackend::new(w, h))
                .unwrap()
                .draw(|f| app.draw(f))
                .unwrap();
        }
        for c in "DELETE".chars() {
            app.key(press(KeyCode::Char(c)));
        }
        app.key(press(KeyCode::Enter));
        assert!(app.delete_requested && app.busy);
        let marks = std::mem::take(&mut app.marks).into_values().collect();
        let outcome = deletion::execute(marks, &Progress::default());
        app.finish_deletion(
            outcome,
            scan(options, Arc::default()),
            dir.path().canonicalize().unwrap(),
        );
        assert!(!dir.path().join("file").exists());
        assert!(app.notice.contains("Deleted 1 files"));
        assert!(app.marks.is_empty());
        assert!(!app.rows.iter().any(|r| r.name == "file"));
        app.key(press(KeyCode::Enter));
        assert!(app.review.is_none());
        app.key(press(KeyCode::Char('x')));
        assert!(app.review.is_none());
    }

    #[test]
    fn marks_cannot_include_directories_or_other_owners() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("file"), b"data").unwrap();
        let tree = scan(
            ScanOptions {
                path: dir.path().into(),
                threads: 1,
                one_file_system: false,
                excludes: vec![],
            },
            Arc::default(),
        )
        .unwrap();
        let mut app = App::new(tree, Filter::default(), View::Directories, false);
        app.selected = app.rows.iter().position(|r| r.name == "nested/").unwrap();
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(app.marks.is_empty());
        app.selected = app.rows.iter().position(|r| r.name == "file").unwrap();
        app.filter.gid = Some(app.tree.nodes[0].gid.wrapping_add(1));
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(app.marks.is_empty());
    }

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
                3,
                "reopening the picker must expose All and other owners"
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
        app.key(press(KeyCode::Down)); // All is the first row.
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
