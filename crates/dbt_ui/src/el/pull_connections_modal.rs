//! Pulls a platform project's connections.yml down from its console into
//! `el/connections.yml`, the reverse of "Send connections". The console
//! keeps no secret's value in that file (the platform refuses one), only
//! `${NAME}` references, so what arrives is safe to write; the names this
//! machine does not set are listed so they can go into `.env`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task,
    WeakEntity, Window,
};
use project::Project;
use ui::prelude::*;
use workspace::{ModalView, Workspace};

use super::dialog;
use super::panel::ElPanel;
use super::platform::{self, PlatformRemote};
use el_engine::spec::Connections;

/// What pulling would do to the local file.
#[derive(Debug, Default, PartialEq, Eq)]
struct Changes {
    /// The local file is already the console's, byte for byte.
    same_text: bool,
    /// The local file does not parse: it is replaced whole.
    local_unreadable: bool,
    /// Connection names, a profile's as `profile/name`.
    added: Vec<String>,
    changed: Vec<String>,
    removed: Vec<String>,
    /// The `${VAR}` names the console's file references that neither the
    /// environment nor `.env` sets. Names only, never values.
    missing_vars: Vec<String>,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.same_text
    }
}

struct Fetched {
    content: String,
    changes: Changes,
}

enum Fetching {
    Fetching,
    Ready(Fetched),
    Failed(SharedString),
}

pub struct ElPullConnectionsModal {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    panel: Option<WeakEntity<ElPanel>>,
    project: Entity<Project>,
    root: PathBuf,
    remotes: Vec<(SharedString, PlatformRemote)>,
    selected: usize,
    fetching: Fetching,
    writing: bool,
    error: Option<SharedString>,
    _fetch: Task<()>,
}

/// The platform remotes `root` declares, by name.
fn platform_remotes(root: &Path) -> Vec<(SharedString, PlatformRemote)> {
    el_engine::spec::load_remotes(&super::el_dir(root).join("remotes.yml"))
        .map(|remotes| remotes.remotes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, remote)| {
            platform::platform_remote(&remote.url)
                .map(|platform| (SharedString::from(name), platform))
        })
        .collect()
}

/// Every connection of `connections`, a profile's under `profile/name`,
/// as the JSON it serializes to, so two files compare entry by entry.
fn entries(connections: &Connections) -> BTreeMap<String, serde_json::Value> {
    let mut entries = BTreeMap::new();
    for (name, connection) in &connections.connections {
        entries.insert(
            name.clone(),
            serde_json::to_value(connection).unwrap_or(serde_json::Value::Null),
        );
    }
    for (profile, spec) in &connections.profiles {
        for (name, connection) in &spec.connections {
            entries.insert(
                format!("{profile}/{name}"),
                serde_json::to_value(connection).unwrap_or(serde_json::Value::Null),
            );
        }
    }
    entries
}

/// What replacing `local` (None: there is no file) with `remote` changes.
/// `is_set` says whether a variable is set on this machine.
fn compare(
    local: Option<&str>,
    remote: &str,
    is_set: impl Fn(&str) -> bool,
) -> anyhow::Result<Changes> {
    let pulled = el_engine::spec::parse_connections_yaml(remote)
        .map_err(|error| anyhow::anyhow!("the console's connections.yml does not read: {error}"))?;
    let mut changes = Changes {
        same_text: local == Some(remote),
        ..Changes::default()
    };
    let theirs = entries(&pulled);
    let ours = match local.map(el_engine::spec::parse_connections_yaml) {
        Some(Ok(connections)) => entries(&connections),
        Some(Err(_)) => {
            changes.local_unreadable = true;
            BTreeMap::new()
        }
        None => BTreeMap::new(),
    };
    for (name, value) in &theirs {
        match ours.get(name) {
            None => changes.added.push(name.clone()),
            Some(existing) if existing != value => changes.changed.push(name.clone()),
            Some(_) => {}
        }
    }
    changes.removed = ours
        .keys()
        .filter(|name| !theirs.contains_key(*name))
        .cloned()
        .collect();
    let mut refs = Vec::new();
    el_engine::env::collect_var_refs(remote, &mut refs);
    refs.sort();
    refs.dedup();
    changes.missing_vars = refs.into_iter().filter(|var| !is_set(var)).collect();
    Ok(changes)
}

/// A list of names as a sentence reads it, at most five.
fn names(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(5).map(String::as_str).collect();
    let more = names.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

impl ElPullConnectionsModal {
    /// Opens the modal on `preferred`'s remote when it is a console one,
    /// else on the first.
    pub fn deploy(
        workspace: &mut Workspace,
        panel: Option<WeakEntity<ElPanel>>,
        root: PathBuf,
        preferred: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let remotes = platform_remotes(&root);
        if remotes.is_empty() {
            super::toast(
                workspace,
                "No Ellode platform remote yet: add one (Servers, then Add server, Ellode platform) to pull its connections.",
                cx,
            );
            return;
        }
        let selected = preferred
            .and_then(|preferred| remotes.iter().position(|(name, _)| *name == preferred))
            .unwrap_or(0);
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, move |_, cx| {
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace_handle,
                panel,
                project,
                root,
                remotes,
                selected,
                fetching: Fetching::Fetching,
                writing: false,
                error: None,
                _fetch: Task::ready(()),
            };
            this.fetch(cx);
            this
        });
    }

    fn connections_path(&self) -> PathBuf {
        super::el_dir(&self.root).join("connections.yml")
    }

    /// Reads the console's file and compares it with ours, off the UI
    /// thread: a network call and a few file reads.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some((name, remote)) = self.remotes.get(self.selected).cloned() else {
            return;
        };
        self.fetching = Fetching::Fetching;
        self.error = None;
        cx.notify();
        let root = self.root.clone();
        let path = self.connections_path();
        let fetching = cx.background_spawn(async move {
            let client = el_engine::server::RemoteClient::connect(&root, &name)?;
            let file = client.connections_file()?;
            let local = match std::fs::read_to_string(&path) {
                Ok(text) => Some(text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "{} could not be read: {error}",
                        path.display()
                    ));
                }
            };
            let env = el_engine::env::EnvMap::load(&root, None);
            let changes = compare(local.as_deref(), &file.content, |var| env.contains(var))?;
            anyhow::Ok(Fetched {
                content: file.content,
                changes,
            })
        });
        self._fetch = cx.spawn(async move |this, cx| {
            let fetched = fetching.await;
            this.update(cx, |this, cx| {
                this.fetching = match fetched {
                    Ok(fetched) => Fetching::Ready(fetched),
                    Err(error) => Fetching::Failed(
                        platform::refusal_text(&remote, &error)
                            .unwrap_or_else(|| format!("{error:#}"))
                            .into(),
                    ),
                };
                cx.notify();
            })
            .ok();
        });
    }

    fn pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Fetching::Ready(fetched) = &self.fetching else {
            return;
        };
        if self.writing || fetched.changes.is_empty() {
            return;
        }
        let Some((_, remote)) = self.remotes.get(self.selected).cloned() else {
            return;
        };
        let content = fetched.content.clone();
        let missing = fetched.changes.missing_vars.clone();
        let workspace = self.workspace.clone();
        let panel = self.panel.clone();
        let project = self.project.clone();
        let path = self.connections_path();
        self.writing = true;
        self.error = None;
        cx.notify();
        // Detached: dismissing the modal must never cancel a write that
        // has already started.
        cx.spawn_in(window, async move |this, cx| {
            let result =
                super::spec_io::write_text(workspace.clone(), project, path, content, cx).await;
            if result.is_ok() {
                if let Some(panel) = &panel {
                    panel
                        .update(cx, |panel, cx| panel.connections_changed(cx))
                        .ok();
                }
                let message = if missing.is_empty() {
                    format!("connections.yml now matches {}.", remote.project)
                } else {
                    format!(
                        "connections.yml now matches {}. Set in .env: {}.",
                        remote.project,
                        missing.join(", ")
                    )
                };
                workspace
                    .update(cx, |workspace, cx| super::toast(workspace, &message, cx))
                    .ok();
            }
            this.update(cx, |this, cx| {
                this.writing = false;
                match &result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl EventEmitter<DismissEvent> for ElPullConnectionsModal {}
impl ModalView for ElPullConnectionsModal {}

impl Focusable for ElPullConnectionsModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ElPullConnectionsModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        let target = self
            .remotes
            .get(self.selected)
            .map(|(_, remote)| remote.clone());
        let mut body = dialog::body();
        if self.remotes.len() > 1 {
            let chips = self.remotes.iter().enumerate().map(|(ix, (name, remote))| {
                dialog::chip(
                    SharedString::from(format!("el-pull-remote-{name}")),
                    format!("{} · {}", remote.project, remote.host),
                    ix == self.selected,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.selected != ix {
                        this.selected = ix;
                        this.fetch(cx);
                    }
                }))
            });
            body = body.child(dialog::labeled("From", dialog::chips(chips)));
        }
        body = match &self.fetching {
            Fetching::Fetching => body.child(dialog::loading("Reading the console's connections…")),
            Fetching::Failed(error) => body.child(dialog::notice(Severity::Error, error.clone())),
            Fetching::Ready(fetched) => {
                let changes = &fetched.changes;
                if changes.is_empty() {
                    body.child(dialog::notice(
                        Severity::Success,
                        "el/connections.yml already matches the console.",
                    ))
                } else {
                    let mut said = Vec::new();
                    if !changes.added.is_empty() {
                        said.push(format!("Adds {}.", names(&changes.added)));
                    }
                    if !changes.changed.is_empty() {
                        said.push(format!("Changes {}.", names(&changes.changed)));
                    }
                    if !changes.removed.is_empty() {
                        said.push(format!("Removes {}.", names(&changes.removed)));
                    }
                    if said.is_empty() {
                        said.push(
                            "The connections are the same; other settings or the layout differ."
                                .to_owned(),
                        );
                    }
                    let mut body = body.child(dialog::notice(
                        Severity::Info,
                        format!(
                            "el/connections.yml is replaced by the console's. {} Secrets stay on the console: the file holds only their names.",
                            said.join(" ")
                        ),
                    ));
                    if changes.local_unreadable {
                        body = body.child(dialog::notice(
                            Severity::Warning,
                            "The local connections.yml does not parse, so it is replaced whole.",
                        ));
                    }
                    if !changes.missing_vars.is_empty() {
                        body = body.child(dialog::notice(
                            Severity::Warning,
                            format!(
                                "Not set on this machine, to add to .env: {}.",
                                changes.missing_vars.join(", ")
                            ),
                        ));
                    }
                    body
                }
            }
        };
        if self.writing {
            body = body.child(dialog::loading("Writing connections.yml…"));
        }
        if let Some(error) = &self.error {
            body = body.child(dialog::notice(Severity::Error, error.clone()));
        }
        let ready = matches!(&self.fetching, Fetching::Ready(fetched) if !fetched.changes.is_empty())
            && !self.writing;

        dialog::frame(dialog::NARROW, cx)
            .track_focus(&self.focus_handle)
            .key_context("ElPullConnectionsModal")
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.pull(window, cx)))
            .child(dialog::header(
                "Pull connections",
                target.map(|remote| {
                    SharedString::from(format!("{} on {}", remote.project, remote.host))
                }),
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                cx,
            ))
            .child(body)
            .child(dialog::footer(
                [],
                [
                    dialog::cancel_button("el-pull-connections-cancel", &focus, cx)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
                        .into_any_element(),
                    dialog::primary_button(
                        "el-pull-connections-confirm",
                        "Replace connections.yml",
                        &focus,
                        cx,
                    )
                    .disabled(!ready)
                    .on_click(cx.listener(|this, _, window, cx| this.pull(window, cx)))
                    .into_any_element(),
                ],
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = "version: 1\nconnections:\n  \
        wh: { type: postgres, url: '${WH_URL}' }\n  \
        old: { type: postgres, url: '${OLD_URL}' }\n";
    const REMOTE: &str = "version: 1\nconnections:\n  \
        wh: { type: postgres, url: '${WH_URL_2}' }\n  \
        crm: { type: postgres, url: '${CRM_URL}' }\n\
        profiles:\n  prod:\n    connections:\n      \
        wh: { type: postgres, url: '${WH_PROD}' }\n";

    #[test]
    fn a_pull_says_what_it_adds_changes_and_removes() {
        let changes = compare(Some(LOCAL), REMOTE, |var| var == "CRM_URL").unwrap();
        assert_eq!(changes.added, vec!["crm", "prod/wh"]);
        assert_eq!(changes.changed, vec!["wh"]);
        assert_eq!(changes.removed, vec!["old"]);
        assert_eq!(changes.missing_vars, vec!["WH_PROD", "WH_URL_2"]);
        assert!(!changes.same_text && !changes.local_unreadable);
    }

    #[test]
    fn a_pull_with_nothing_to_change_or_nothing_local() {
        let same = compare(Some(REMOTE), REMOTE, |_| true).unwrap();
        assert!(same.is_empty() && same.added.is_empty() && same.missing_vars.is_empty());
        let fresh = compare(None, REMOTE, |_| true).unwrap();
        assert_eq!(fresh.added, vec!["crm", "prod/wh", "wh"]);
        let broken = compare(Some("connections: ["), REMOTE, |_| true).unwrap();
        assert!(broken.local_unreadable);
        assert!(compare(Some(LOCAL), "connections: [", |_| true).is_err());
    }

    #[test]
    fn only_platform_remotes_are_pulled_from() {
        let dir = tempfile::tempdir().unwrap();
        let el = dir.path().join("el");
        std::fs::create_dir_all(&el).unwrap();
        std::fs::write(
            el.join("remotes.yml"),
            "version: 1\nremotes:\n  vm: { url: 'https://el.example.com:7431', token: '${ELLODE_TOKEN}' }\n  \
             shop: { url: 'https://console.example.com/api/platform/projects/shop', token: '${ELLODE_PAT_SHOP}', live: false }\n",
        )
        .unwrap();
        let remotes = platform_remotes(dir.path());
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].1.project, "shop");
        assert_eq!(names(&["a".into(), "b".into()]), "a, b");
    }
}
