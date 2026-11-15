//! TUI model: a lazily expanded directory tree and the selection over it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::fs as fsmeta;
use crate::walk::{ChildSize, Kind, Options, Size, SizeSource};
use crate::worker::{JobResult, Worker};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Pending,
    Ready,
    Error(String),
}

impl State {
    fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "",
            Self::Error(_) => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UiNode {
    pub path: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub size: Option<Size>,
    pub source: Option<SizeSource>,
    pub complete: bool,
    pub state: State,
    pub expanded: bool,
    pub children: Option<Vec<UiNode>>,
}

impl UiNode {
    fn new(path: PathBuf, name: String, kind: Kind) -> Self {
        Self {
            path,
            name,
            kind,
            size: None,
            source: None,
            complete: false,
            state: State::Pending,
            expanded: false,
            children: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VisibleRow {
    pub path: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub size: Option<Size>,
    pub state: State,
    pub source: Option<SizeSource>,
    pub complete: bool,
    pub expanded: bool,
    pub depth: usize,
}

pub struct App {
    pub root: PathBuf,
    pub options: Options,
    pub node: UiNode,
    pub selected_path: PathBuf,
    pub show_help: bool,
    pub status: Vec<String>,
    pub quit: bool,
    generation: u64,
    worker: Worker,
    results: Receiver<JobResult>,
}

impl App {
    pub fn new(root: &Path, options: Options) -> Self {
        let (worker, results) = Worker::spawn();
        let mut app = Self {
            root: root.to_path_buf(),
            options,
            node: UiNode::new(root.to_path_buf(), display_name(root), Kind::Directory),
            selected_path: root.to_path_buf(),
            show_help: false,
            status: Vec::new(),
            quit: false,
            generation: 0,
            worker,
            results,
        };
        app.rebuild();
        app
    }

    /// Re-list every expanded directory and ask the worker to measure each.
    fn rebuild(&mut self) {
        self.worker.clear();
        let expanded = self.expanded_paths();
        self.node = UiNode::new(self.root.clone(), display_name(&self.root), Kind::Directory);
        self.node.expanded = true;
        let root = self.root.clone();
        let root_children = self.list_children(&root);
        self.node.children = Some(root_children);
        self.worker
            .measure(&self.root, self.options, self.generation);

        for path in expanded {
            if path == self.root {
                continue;
            }
            let listing = self.list_children(&path);
            let mut did_expand = false;
            if let Some(node) = find_node_mut(&mut self.node, &path)
                && node.kind == Kind::Directory
                && !node.expanded
            {
                node.expanded = true;
                node.children = Some(listing);
                did_expand = true;
            }
            if did_expand {
                self.worker.measure(&path, self.options, self.generation);
            }
        }

        if find_node(&self.node, &self.selected_path).is_none() {
            self.selected_path = self.root.clone();
        }
    }

    fn expanded_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        collect_expanded(&self.node, &mut paths);
        paths
    }

    /// Names and kinds are cheap; sizes are not, so rows start as pending.
    fn list_children(&mut self, path: &Path) -> Vec<UiNode> {
        let mut children = Vec::new();
        let directory = match fs::read_dir(path) {
            Ok(directory) => directory,
            Err(error) => {
                self.status.push(format!("{}: {error}", path.display()));
                return children;
            }
        };
        for entry in directory {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.status.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let child_path = entry.path();
            let stat = match fsmeta::lstat(&child_path) {
                Ok(stat) => stat,
                Err(error) => {
                    self.status
                        .push(format!("{}: {error}", child_path.display()));
                    continue;
                }
            };
            let name = child_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| child_path.display().to_string());
            children.push(UiNode::new(child_path, name, Kind::from_mode(stat.mode)));
        }
        children.sort_by(|a, b| a.name.cmp(&b.name));
        children
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.results.try_recv() {
            if result.generation == self.generation {
                self.apply(result);
                changed = true;
            }
        }
        changed
    }

    fn apply(&mut self, result: JobResult) {
        if result.options != self.options {
            return;
        }
        for error in &result.errors {
            self.status
                .push(format!("{}: {}", error.path.display(), error.message));
        }
        if result.skipped > 0 {
            self.status.push(format!(
                "{} entries on other filesystems skipped (x to include)",
                result.skipped
            ));
        }

        let options = self.options;
        let Some(node) = find_node_mut(&mut self.node, &result.dir) else {
            return;
        };
        if let Some(children) = node.children.as_mut() {
            for child in children.iter_mut() {
                if let Some(measured) = result.children.iter().find(|c| c.path == child.path) {
                    apply_child(child, measured);
                } else if matches!(child.state, State::Pending) {
                    child.state = State::Error("not measured".to_string());
                }
            }
            sort_ui_children(children, options.allocated);
        }
        node.state = State::Ready;
        node.complete = node
            .children
            .as_ref()
            .is_some_and(|children| children.iter().all(|c| c.complete));
        // Keep only the most recent status lines; the panel is two rows.
        if self.status.len() > 8 {
            self.status.drain(0..self.status.len() - 8);
        }
    }

    pub fn rows(&self) -> Vec<VisibleRow> {
        let mut rows = Vec::new();
        collect_rows(&self.node, 0, &mut rows);
        rows
    }

    fn selected_row(&self) -> Option<VisibleRow> {
        self.rows()
            .into_iter()
            .find(|row| row.path == self.selected_path)
    }

    pub fn selected(&self) -> Option<&UiNode> {
        find_node(&self.node, &self.selected_path)
    }

    pub fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let current = rows
            .iter()
            .position(|row| row.path == self.selected_path)
            .unwrap_or(0);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            (current + delta as usize).min(rows.len() - 1)
        };
        self.selected_path = rows[next].path.clone();
    }

    pub fn expand_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.kind != Kind::Directory {
            return;
        }
        if row.expanded {
            return;
        }
        let path = row.path;
        let listing = if find_node(&self.node, &path)
            .and_then(|node| node.children.as_ref())
            .is_none()
        {
            Some(self.list_children(&path))
        } else {
            None
        };
        if let Some(node) = find_node_mut(&mut self.node, &path) {
            node.expanded = true;
            if node.children.is_none() {
                node.children = listing;
            }
        }
        self.worker.measure(&path, self.options, self.generation);
    }

    pub fn collapse_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.expanded && row.kind == Kind::Directory {
            if let Some(node) = find_node_mut(&mut self.node, &row.path) {
                node.expanded = false;
            }
            return;
        }
        if row.path == self.root {
            return;
        }
        if let Some(parent) = parent_of(&self.node, &row.path) {
            self.selected_path = parent;
        }
    }

    pub fn refresh(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.status.clear();
        self.rebuild();
    }

    pub fn toggle_allocated(&mut self) {
        self.options.allocated = !self.options.allocated;
        let allocated = self.options.allocated;
        sort_tree(&mut self.node, allocated);
        let expanded = self.expanded_paths();
        for path in expanded {
            self.worker.measure(&path, self.options, self.generation);
        }
    }

    pub fn toggle_cross(&mut self) {
        self.options.cross = !self.options.cross;
        self.generation = self.generation.wrapping_add(1);
        self.rebuild();
    }

    /// Returns `false` when the event loop should stop.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return false;
        }
        if self.show_help {
            self.show_help = false;
            return !matches!(key.code, KeyCode::Char('q'));
        }
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('a') => self.toggle_allocated(),
            KeyCode::Char('x') => self.toggle_cross(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Enter => {
                if self.selected_row().is_some_and(|row| row.expanded) {
                    self.collapse_selected();
                } else {
                    self.expand_selected();
                }
            }
            KeyCode::Right | KeyCode::Char('l') => self.expand_selected(),
            KeyCode::Left | KeyCode::Char('h') => self.collapse_selected(),
            _ => {}
        }
        true
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);

        self.render_header(frame, chunks[0]);
        self.render_table(frame, chunks[1]);
        self.render_status(frame, chunks[2]);

        if self.show_help {
            render_help(frame, area);
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let path = &self.selected_path;
        let mut lines = vec![Line::from(vec![
            Span::styled("path  ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(path.display().to_string()),
        ])];
        match fsmeta::filesystem_summary(path) {
            Ok(summary) => {
                lines.push(Line::from(format!(
                    "fs    {} ({})  total {}  used {}  avail {}  use {:.0}%",
                    summary.mount_point.display(),
                    summary.fs_type,
                    format_bytes(summary.total_bytes),
                    format_bytes(summary.used_bytes),
                    format_bytes(summary.available_bytes),
                    summary.use_percent,
                )));
            }
            Err(error) => lines.push(Line::from(format!("fs    unavailable: {error}"))),
        }
        lines.push(Line::from(format!(
            "mode  {}  ·  {}  ·  sizes {}",
            if self.options.allocated {
                "allocated (st_blocks)"
            } else {
                "apparent (st_size)"
            },
            if self.options.cross {
                "crossing filesystems"
            } else {
                "one filesystem"
            },
            if self.options.allocated {
                "from statx or a filesystem summary"
            } else {
                "from a generic statx walk"
            },
        )));

        let block = Block::bordered().title(" diskfoot ");
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_table(&self, frame: &mut Frame, area: Rect) {
        let rows_data = self.rows();
        let selected = rows_data
            .iter()
            .position(|row| row.path == self.selected_path)
            .unwrap_or(0);

        let rows: Vec<Row> = rows_data
            .iter()
            .map(|row| {
                let indent = "  ".repeat(row.depth);
                let marker = if row.kind == Kind::Directory {
                    if row.expanded { "▾ " } else { "▸ " }
                } else if row.kind == Kind::Symlink {
                    "@ "
                } else {
                    "  "
                };
                let name = format!("{indent}{marker}{}", row.name);
                let allocated = row
                    .size
                    .map(|size| format_bytes(size.allocated))
                    .unwrap_or_else(|| "…".to_string());
                let apparent = match (row.size, row.source) {
                    (Some(size), Some(source))
                        if source.is_fast_path() && size.apparent.is_none() =>
                    {
                        "—".to_string()
                    }
                    (Some(size), _) => size
                        .apparent
                        .map(format_bytes)
                        .unwrap_or_else(|| "—".to_string()),
                    (None, _) => "…".to_string(),
                };
                let mut state = row.state.label().to_string();
                if row.complete && state.is_empty() {
                    state = "ready".to_string();
                } else if !row.complete && state.is_empty() {
                    state = "partial".to_string();
                }
                Row::new(vec![
                    Cell::from(name),
                    Cell::from(allocated),
                    Cell::from(apparent),
                    Cell::from(state),
                ])
            })
            .collect();

        let header = Row::new(vec!["NAME", "ALLOCATED", "APPARENT", "STATE"])
            .style(Style::default().add_modifier(Modifier::BOLD));
        let table = Table::new(
            rows,
            [
                Constraint::Min(20),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(9),
            ],
        )
        .header(header)
        .block(Block::bordered().title(display_name(&self.root)))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");

        let mut state = TableState::default().with_selected(Some(selected));
        frame.render_stateful_widget(table, area, &mut state);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let hint =
            "q quit  ? help  enter/→ expand  ← collapse  r refresh  a allocated/apparent  x cross";
        let lines = match self.status.last() {
            Some(message) => vec![Line::from(message.clone()), Line::from(hint)],
            None => vec![Line::from(hint)],
        };
        frame.render_widget(Paragraph::new(lines), area);
    }
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 70, 16);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from("diskfoot keys"),
        Line::from(""),
        Line::from("q            quit"),
        Line::from("?            this help"),
        Line::from("enter / →    expand a directory (lazily)"),
        Line::from("←            collapse, or move to parent"),
        Line::from("↑ ↓ / k j    move the selection"),
        Line::from("r            refresh and clear the size cache"),
        Line::from("a            allocated (st_blocks) / apparent (st_size)"),
        Line::from("x            cross filesystem boundaries"),
        Line::from(""),
        Line::from("Sizes begin as pending. Only an expanded directory is walked."),
        Line::from("A fast path is used only at a btrfs subvolume root, a ZFS dataset"),
        Line::from("root, or an XFS project quota. Everything else uses exact statx."),
    ];
    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(" help "))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, popup);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height.min(area.height)),
        Constraint::Fill(1),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width.min(area.width)),
        Constraint::Fill(1),
    ])
    .split(vertical[1])[1]
}

fn apply_child(child: &mut UiNode, measured: &ChildSize) {
    child.size = Some(measured.size);
    child.source = Some(measured.source);
    child.complete = measured.complete;
    child.state = if measured.skipped_other_filesystem {
        State::Error("other filesystem".to_string())
    } else {
        State::Ready
    };
}

fn sort_ui_children(children: &mut [UiNode], allocated: bool) {
    children.sort_by(|a, b| {
        let a_size = a.size.map(|s| s.key(allocated)).unwrap_or(0);
        let b_size = b.size.map(|s| s.key(allocated)).unwrap_or(0);
        b_size.cmp(&a_size).then_with(|| a.name.cmp(&b.name))
    });
}

fn sort_tree(node: &mut UiNode, allocated: bool) {
    if let Some(children) = node.children.as_mut() {
        for child in children.iter_mut() {
            sort_tree(child, allocated);
        }
        sort_ui_children(children, allocated);
    }
}

fn collect_rows(node: &UiNode, depth: usize, rows: &mut Vec<VisibleRow>) {
    rows.push(VisibleRow {
        path: node.path.clone(),
        name: node.name.clone(),
        kind: node.kind,
        size: node.size,
        state: node.state.clone(),
        source: node.source,
        complete: node.complete,
        expanded: node.expanded,
        depth,
    });
    if node.expanded
        && let Some(children) = node.children.as_ref()
    {
        for child in children {
            collect_rows(child, depth + 1, rows);
        }
    }
}

fn collect_expanded(node: &UiNode, paths: &mut Vec<PathBuf>) {
    if node.expanded {
        paths.push(node.path.clone());
    }
    if let Some(children) = node.children.as_ref() {
        for child in children {
            collect_expanded(child, paths);
        }
    }
}

fn find_node<'a>(node: &'a UiNode, path: &Path) -> Option<&'a UiNode> {
    if node.path == path {
        return Some(node);
    }
    node.children
        .as_ref()
        .and_then(|children| children.iter().find_map(|child| find_node(child, path)))
}

fn find_node_mut<'a>(node: &'a mut UiNode, path: &Path) -> Option<&'a mut UiNode> {
    if node.path == path {
        return Some(node);
    }
    node.children.as_mut().and_then(|children| {
        children
            .iter_mut()
            .find_map(|child| find_node_mut(child, path))
    })
}

fn parent_of(root: &UiNode, path: &Path) -> Option<PathBuf> {
    fn walk(node: &UiNode, path: &Path) -> Option<PathBuf> {
        if let Some(children) = node.children.as_ref() {
            if children.iter().any(|child| child.path == path) {
                return Some(node.path.clone());
            }
            for child in children {
                if let Some(parent) = walk(child, path) {
                    return Some(parent);
                }
            }
        }
        None
    }
    walk(root, path)
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::io::Write;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp tree");
        std::fs::create_dir(dir.path().join("src")).expect("dir");
        let mut file = std::fs::File::create(dir.path().join("src/main.rs")).expect("file");
        file.write_all(&vec![b'x'; 4096]).expect("write");
        std::fs::write(dir.path().join("README.md"), b"# example\n").expect("write");
        dir
    }

    #[test]
    fn layout_renders_header_columns_and_rows() {
        let dir = fixture();
        let app = App::new(dir.path(), Options::default());
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");

        let buffer = terminal.backend().buffer();
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("diskfoot"), "title missing: {text}");
        assert!(text.contains("NAME"), "name column missing: {text}");
        assert!(text.contains("ALLOCATED"), "allocated column missing");
        assert!(text.contains("APPARENT"), "apparent column missing");
        assert!(text.contains("src"), "child row missing");
        assert!(text.contains("README.md"), "child row missing");
        assert!(text.contains("total"), "df header missing");
    }

    #[test]
    fn expanding_and_collapsing_changes_visible_rows() {
        let dir = fixture();
        let mut app = App::new(dir.path(), Options::default());
        app.selected_path = dir.path().join("src");
        assert_eq!(app.rows().len(), 3);
        app.expand_selected();
        assert_eq!(app.rows().len(), 4);
        assert!(app.rows().iter().any(|row| row.name == "main.rs"));
        app.collapse_selected();
        assert_eq!(app.rows().len(), 3);
    }

    #[test]
    fn help_toggles() {
        let dir = fixture();
        let mut app = App::new(dir.path(), Options::default());
        assert!(!app.show_help);
        app.on_key(KeyEvent::from(KeyCode::Char('?')));
        assert!(app.show_help);
        app.on_key(KeyEvent::from(KeyCode::Char('?')));
        assert!(!app.show_help);
    }

    #[test]
    fn formats_bytes_for_humans() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn pending_rows_are_visible_before_sizes_arrive() {
        let dir = fixture();
        let app = App::new(dir.path(), Options::default());
        let row = app
            .rows()
            .into_iter()
            .find(|row| row.name == "README.md")
            .expect("row");
        assert!(matches!(row.state, State::Pending) || row.size.is_some());
    }
}
