use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ParentElement,
    Render, Task, WeakEntity, Window,
};
use picker::{HighlightedTextBuilder, Picker, PickerDelegate, PreviewUpdate};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::SystemTime,
};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::paths::home_dir;
use workspace::{ModalView, OpenMode, Workspace};

const MAX_PREVIEW_ENTRIES: usize = 60;

/// Opens a finder that lists the folders inside the given roots and opens the
/// selected one as a project.
#[derive(Default, PartialEq, Eq, Clone, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = file_finder)]
#[serde(deny_unknown_fields)]
pub struct OpenFolder {
    /// Directories whose immediate subdirectories are listed. A leading `~`
    /// is expanded to the home directory.
    #[serde(default)]
    pub roots: Vec<String>,
}

pub(crate) fn register(
    workspace: &mut Workspace,
    _window: Option<&mut Window>,
    _: &mut Context<Workspace>,
) {
    workspace.register_action(|workspace, action: &OpenFolder, window, cx| {
        let roots = action
            .roots
            .iter()
            .map(|root| expand_root(root))
            .collect::<Vec<_>>();
        let workspace_handle = cx.entity().downgrade();
        let project = workspace.project().clone();
        workspace.toggle_modal(window, cx, |window, cx| {
            FolderFinder::new(workspace_handle, project, roots, window, cx)
        });
    });
}

fn expand_root(root: &str) -> PathBuf {
    if root == "~" {
        return home_dir().clone();
    }
    match root.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest),
        None => PathBuf::from(root),
    }
}

struct FolderEntry {
    path: PathBuf,
    name: String,
    modified: Option<SystemTime>,
    preview: String,
}

struct FolderFinder {
    picker: Entity<Picker<FolderFinderDelegate>>,
}

impl ModalView for FolderFinder {}

impl EventEmitter<DismissEvent> for FolderFinder {}

impl Focusable for FolderFinder {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for FolderFinder {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

impl FolderFinder {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<project::Project>,
        roots: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let finder = cx.entity().downgrade();
        let delegate = FolderFinderDelegate {
            finder,
            workspace,
            entries: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
            load_task: Task::ready(()),
        };
        let preview = picker_preview::editor_preview(project, window, cx);
        let picker = cx.new(|cx| Picker::uniform_list_with_preview(delegate, preview, window, cx));
        picker.update(cx, |picker, cx| {
            picker.delegate.load(roots, window, cx);
        });
        Self { picker }
    }
}

struct FolderFinderDelegate {
    finder: WeakEntity<FolderFinder>,
    workspace: WeakEntity<Workspace>,
    entries: Vec<FolderEntry>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    load_task: Task<()>,
}

impl FolderFinderDelegate {
    fn load(&mut self, roots: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let executor = cx.background_executor().clone();
        self.load_task = cx.spawn_in(window, async move |picker, cx| {
            let mut entries = executor.spawn(async move { read_folders(&roots) }).await;
            entries.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
            picker
                .update_in(cx, |picker, window, cx| {
                    picker.delegate.entries = entries;
                    let query = picker.query(cx);
                    picker.update_matches(query, window, cx);
                })
                .ok();
        });
    }

    fn entry_for_match(&self, mat: &StringMatch) -> Option<&FolderEntry> {
        self.entries.get(mat.candidate_id)
    }
}

fn read_folders(roots: &[PathBuf]) -> Vec<FolderEntry> {
    let mut entries = Vec::new();
    for root in roots {
        let Ok(read_dir) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let modified = entry.metadata().and_then(|meta| meta.modified()).ok();
            entries.push(FolderEntry {
                name: name.to_string(),
                preview: list_children(&path),
                path,
                modified,
            });
        }
    }
    entries
}

fn list_children(dir: &Path) -> String {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut names = read_dir
        .flatten()
        .map(|entry| {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() {
                name.push('/');
            }
            name
        })
        .collect::<Vec<_>>();
    names.sort();
    let total = names.len();
    names.truncate(MAX_PREVIEW_ENTRIES);
    let mut text = names.join("\n");
    if total > MAX_PREVIEW_ENTRIES {
        text.push_str(&format!("\n… and {} more", total - MAX_PREVIEW_ENTRIES));
    }
    text
}

impl PickerDelegate for FolderFinderDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "folder finder"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search folders...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        cx.notify();
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self
            .entries
            .iter()
            .enumerate()
            .map(|(id, entry)| StringMatchCandidate::new(id, &entry.name))
            .collect::<Vec<_>>();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        score: 0.,
                        positions: Vec::new(),
                        string: candidate.string,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    true,
                    true,
                    100,
                    &AtomicBool::new(false),
                    executor,
                )
                .await
            };
            picker
                .update_in(cx, |picker, _, cx| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    delegate.selected_index = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(path) = self
            .matches
            .get(self.selected_index)
            .and_then(|mat| self.entry_for_match(mat))
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_workspace_for_paths(OpenMode::Activate, vec![path], window, cx)
                    .detach_and_log_err(cx);
            })
            .ok();
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.finder.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn try_get_preview_data_for_match(&self, cx: &App) -> Option<PreviewUpdate> {
        let entry = self.entry_for_match(self.matches.get(self.selected_index)?)?;
        let highlight = gpui::HighlightStyle {
            color: Some(cx.theme().colors().text_accent),
            ..Default::default()
        };
        let mut message = HighlightedTextBuilder::default();
        message.push_styled(&entry.path.to_string_lossy(), highlight);
        message.push_plain("\n\n");
        message.push_plain(&entry.preview);
        Some(PreviewUpdate::message(message.build()))
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let entry = self.entry_for_match(mat)?;
        Some(
            ListItem::new(ix)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::Folder).color(Color::Muted))
                .child(HighlightedLabel::new(
                    entry.name.clone(),
                    mat.positions.clone(),
                )),
        )
    }
}
